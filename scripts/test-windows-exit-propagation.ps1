<#
.SYNOPSIS
    CSS-2302 regression test: proves the "Run tests (Windows)" step in
    .github/workflows/test.yml propagates a failing `task test` exit code
    instead of masking it, the way the pre-fix script (commit
    c2b4ac2faa589e67a5c62f58654973ca027c95bb) did with a hardcoded `exit 0`.

.DESCRIPTION
    This does NOT re-implement or hand-copy the exit-code logic under test.
    It extracts the *actual* `run: |` block scalar for the "Run tests
    (Windows)" step directly out of the workflow YAML -- both the current
    HEAD (the candidate) and the known-buggy baseline commit -- and executes
    each extracted block verbatim as a real pwsh child process, driven only
    by a bounded fake `task` command on PATH (no Rust build, no daemon, no
    real cargo invocation). `task` is the only external command the block
    under test invokes, so faking it is sufficient to fully control the
    scenario.

    Scenarios exercised against the candidate (current HEAD) script:
      1. core `task test` fails               -> exit code propagates, and
                                                   the doc-test task is never
                                                   invoked.
      2. core passes, doc-test `task test` fails -> exit code propagates.
      3. both succeed                          -> exit code 0.

    Negative control: the same "core fails" scenario is replayed against the
    known-buggy baseline script text (commit c2b4ac2), which is expected to
    mask the failure (exit 0, doc-test still invoked). If that expectation
    ever stops holding, this regression's fixture is stale. The baseline
    MUST be reachable in this checkout (the workflow already checks out with
    fetch-depth: 0, and this candidate is a descendant of c2b4ac2): if it is
    not, that is an environment/setup failure, and this harness fails closed
    (throws) rather than silently skipping the negative control.

    Every extracted block is run as a real child `pwsh` process under an
    explicit, bounded timeout (see $ChildProcessTimeoutSeconds below). A
    child that hangs has its full process tree killed via the .NET runtime's
    own Process.Kill($true) (never by shelling out to a second, equally
    killable process such as taskkill.exe), the kill is confirmed by a
    bounded WaitForExit, and the child is reported as its own distinct
    failure -- never misread as an expected core/doc-test exit code, and
    never treated as a pass unless that cleanup is actually confirmed.

    Additional focused self-checks (exercising the harness's own failure
    paths, not the workflow script) are run alongside the four scenarios
    above:
      - the fail-closed baseline check actually throws when handed a null
        baseline body.
      - a deliberately hanging child (under a short overridden timeout) is
        detected, killed, and reported as a timeout rather than a pass/fail.

    Exits non-zero (failing the CI job) if any candidate assertion fails, if
    the baseline negative control is unreachable, if a child process times
    out unexpectedly, or if the extraction itself cannot locate the step (so
    a rename/reshape of the workflow can't silently defeat this test).

.NOTES
    Requires local `pwsh` (to run the extracted script blocks as real native
    processes) and `git` (to extract the exact YAML block text via `git
    show`, avoiding any hand-copied duplicate of the algorithm). If `pwsh`
    is unavailable this script cannot prove anything and fails loudly rather
    than fabricating a result.
#>

param(
    [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = "Stop"

$WorkflowRelativePath = ".github/workflows/test.yml"
$KnownBuggyBaselineRef = "c2b4ac2faa589e67a5c62f58654973ca027c95bb"
$StepNamePattern = '^\s*-\s+name:\s*Run tests \(Windows\)\s*$'

# Upper bound on how long any single extracted-block child pwsh process may
# run before the harness kills it and reports a timeout. The fake `task.cmd`
# is instant, so real scenarios never approach this; it exists purely so a
# future run-block shape (or a genuine hang in command resolution/PowerShell
# startup) fails the CI job quickly instead of hanging until the runner's
# own default job timeout.
$ChildProcessTimeoutSeconds = 30

function Stop-ProcessTree {
    <#
    Best-effort kill of a process and its full descendant tree, using the
    .NET runtime's own Process.Kill($true) rather than shelling out to
    taskkill.exe. Killing via a spawned external process is itself an
    unbounded operation -- if taskkill.exe stalls (contested handles,
    driver hangs, etc.) the harness would be stuck waiting on a *second*
    process it doesn't control, which defeats the purpose of a bounded
    kill. Kill($true) is a direct, synchronous, non-spawning termination of
    the process and its full descendant tree (supported since .NET Core
    3.0 / pwsh 7+, which this harness already requires), so there is no
    subprocess of our own that can itself hang.

    Returns $true if the kill call completed without throwing (the
    termination signal was issued), $false otherwise (e.g. the process had
    already exited on its own). This is a best-effort signal only -- it is
    NOT proof the process has actually exited. Callers MUST still bound-wait
    via WaitForExit and use its return value as the real, authoritative
    confirmation that cleanup succeeded before treating the timeout as
    handled.
    #>
    param([Parameter(Mandatory = $true)][System.Diagnostics.Process]$Process)

    try {
        $Process.Kill($true)
        return $true
    }
    catch {
        # best-effort; the process may have already exited on its own, or
        # termination may have been denied. Either way, the caller's
        # bounded WaitForExit below is what actually confirms cleanup.
        return $false
    }
}

function Assert-BaselineReachable {
    <#
    Fail-closed guard for the known-buggy baseline negative control. A null
    $Body means Get-WindowsRunBlock could not read $Ref at all (unreachable
    history). The workflow already checks out with fetch-depth: 0, and this
    candidate is a descendant of the baseline commit, so an unreachable
    baseline is an environment/setup failure -- never a valid reason to
    silently skip the negative control and still exit 0.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Ref,
        [AllowNull()][string]$Body
    )

    if (-not $Body) {
        throw "CSS-2302 regression harness: known-buggy baseline '$Ref' is unreachable in this checkout (Get-WindowsRunBlock returned no content). This candidate must remain a descendant of $Ref and the checkout must use fetch-depth: 0 (it already does in .github/workflows/test.yml). Treating this as fail-closed rather than silently skipping the negative control."
    }
}

function Get-WindowsRunBlock {
    <#
    Extracts the exact `run: |` block-scalar body of the "Run tests
    (Windows)" step from `.github/workflows/test.yml` at the given git ref,
    dedented to column 0. Returns $null if the ref cannot be read at all
    (e.g. an unreachable historical commit); throws if the ref is readable
    but the expected step/block shape cannot be found (a real regression in
    this test's ability to locate its target).
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Ref
    )

    $yaml = & git -C $RepoRoot show "${Ref}:${WorkflowRelativePath}" 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $yaml) {
        return $null
    }

    $lines = $yaml -split '\r?\n'

    $stepLineIndex = -1
    for ($i = 0; $i -lt $lines.Length; $i++) {
        if ($lines[$i] -match $StepNamePattern) {
            $stepLineIndex = $i
            break
        }
    }
    if ($stepLineIndex -lt 0) {
        throw "Could not find the 'Run tests (Windows)' step in ${Ref}:${WorkflowRelativePath}"
    }

    $runLineIndex = -1
    for ($i = $stepLineIndex; $i -lt $lines.Length; $i++) {
        if ($lines[$i] -match '^(\s*)run:\s*\|\s*$') {
            $runLineIndex = $i
            break
        }
    }
    if ($runLineIndex -lt 0) {
        throw "Could not find the 'run: |' block scalar for 'Run tests (Windows)' in ${Ref}:${WorkflowRelativePath}"
    }

    $bodyIndent = -1
    $bodyLines = New-Object System.Collections.Generic.List[string]
    for ($i = $runLineIndex + 1; $i -lt $lines.Length; $i++) {
        $line = $lines[$i]
        if ($line.Trim().Length -eq 0) {
            $bodyLines.Add("")
            continue
        }
        $indent = $line.Length - $line.TrimStart().Length
        if ($bodyIndent -lt 0) {
            $bodyIndent = $indent
        }
        if ($indent -lt $bodyIndent) {
            break
        }
        $bodyLines.Add($line.Substring($bodyIndent))
    }

    if ($bodyLines.Count -eq 0) {
        throw "Extracted an empty run block for 'Run tests (Windows)' in ${Ref}:${WorkflowRelativePath}"
    }

    return ($bodyLines -join "`n")
}

function New-FakeTaskBin {
    <#
    Creates a real native Windows command `task.cmd` in $Dir. It is not a
    Bash/emulation proxy -- it is a genuine external (non-PowerShell-cmdlet)
    command, which is exactly the class of command the CSS-2302 bug is
    about: $ErrorActionPreference = "Stop" does not stop a pwsh script on a
    failing *native* command's exit code. Behavior is controlled purely via
    env vars so no Rust build or real cargo/task invocation is needed:
      - Logs its full argument line to %FAKE_TASK_LOG%.
      - Exits with %FAKE_DOC_EXIT_CODE% if invoked with `--doc` in its args,
        otherwise exits with %FAKE_CORE_EXIT_CODE%.
    #>
    param([Parameter(Mandatory = $true)][string]$Dir)

    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $taskCmd = Join-Path $Dir "task.cmd"
    @'
@echo off
echo %* >> "%FAKE_TASK_LOG%"
echo %*|findstr /C:"--doc" >nul
if %ERRORLEVEL% EQU 0 (
    exit /b %FAKE_DOC_EXIT_CODE%
) else (
    exit /b %FAKE_CORE_EXIT_CODE%
)
'@ | Set-Content -Path $taskCmd -Encoding ascii

    return $taskCmd
}

function Invoke-RunBlock {
    <#
    Runs $ScriptBody (an extracted "Run tests (Windows)" body, verbatim) as
    a real child pwsh process, with $FakeBinDir prepended to PATH so the
    script's bare `task` invocations resolve to the fake native command, and
    MATRIX_MODE forced to "windows-core". Returns the child's exit code, its
    fake task call log, whether it was killed for exceeding $TimeoutSeconds,
    and paths to its captured stdout/stderr (always written, for evidence).

    Uses System.Diagnostics.Process directly (not `&` / Start-Process) so:
      - arguments are passed via ArgumentList (no manual quoting -- correct
        even if $WorkDir/$scriptFile contain spaces).
      - stdout/stderr are drained asynchronously via BeginOutputReadLine /
        BeginErrorReadLine, which avoids the classic redirected-pipe
        deadlock (a child that fills the OS pipe buffer while the parent is
        blocked synchronously reading the other stream, or blocked in
        WaitForExit before either stream is drained).
      - WaitForExit(timeout-ms) bounds the wait; on timeout the full process
        tree is force-killed (Stop-ProcessTree) and the cleanup itself is
        confirmed via a second bounded WaitForExit whose result is recorded
        as CleanupVerified, so a wedged child -- or a wedged kill -- can
        never hang the harness or be silently misread as a clean pass.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$ScriptBody,
        [Parameter(Mandatory = $true)][string]$WorkDir,
        [Parameter(Mandatory = $true)][string]$FakeBinDir,
        [Parameter(Mandatory = $true)][string]$LogPath,
        [Parameter(Mandatory = $true)][int]$CoreExitCode,
        [Parameter(Mandatory = $true)][int]$DocExitCode,
        [int]$TimeoutSeconds = $ChildProcessTimeoutSeconds
    )

    if (Test-Path $LogPath) {
        Remove-Item $LogPath -Force
    }

    $scriptFile = Join-Path $WorkDir "candidate.ps1"
    Set-Content -Path $scriptFile -Value $ScriptBody -Encoding utf8

    $testsDir = Join-Path $WorkDir "tests"
    New-Item -ItemType Directory -Path $testsDir -Force | Out-Null

    $stdoutPath = Join-Path $WorkDir "candidate.stdout.log"
    $stderrPath = Join-Path $WorkDir "candidate.stderr.log"

    $originalPath = $env:PATH
    $originalMode = $env:MATRIX_MODE
    $originalThreads = $env:MATRIX_TEST_THREADS
    $originalCoreExit = $env:FAKE_CORE_EXIT_CODE
    $originalDocExit = $env:FAKE_DOC_EXIT_CODE
    $originalLog = $env:FAKE_TASK_LOG
    $originalLocation = Get-Location

    $proc = $null
    $timedOut = $false
    $exitCode = $null
    $cleanupVerified = $false
    $stdoutBuilder = New-Object System.Text.StringBuilder
    $stderrBuilder = New-Object System.Text.StringBuilder
    $outSubscription = $null
    $errSubscription = $null

    try {
        $env:PATH = "$FakeBinDir;$originalPath"
        $env:MATRIX_MODE = "windows-core"
        $env:MATRIX_TEST_THREADS = "1"
        $env:FAKE_CORE_EXIT_CODE = "$CoreExitCode"
        $env:FAKE_DOC_EXIT_CODE = "$DocExitCode"
        $env:FAKE_TASK_LOG = $LogPath

        Set-Location $WorkDir

        $psi = [System.Diagnostics.ProcessStartInfo]::new()
        $psi.FileName = "pwsh"
        foreach ($arg in @('-NoProfile', '-NonInteractive', '-File', $scriptFile)) {
            $psi.ArgumentList.Add($arg)
        }
        $psi.WorkingDirectory = $WorkDir
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.CreateNoWindow = $true

        $proc = [System.Diagnostics.Process]::new()
        $proc.StartInfo = $psi

        $outSubscription = Register-ObjectEvent -InputObject $proc -EventName OutputDataReceived -MessageData $stdoutBuilder -Action {
            if ($null -ne $EventArgs.Data) { [void]$Event.MessageData.AppendLine($EventArgs.Data) }
        } -PassThru
        $errSubscription = Register-ObjectEvent -InputObject $proc -EventName ErrorDataReceived -MessageData $stderrBuilder -Action {
            if ($null -ne $EventArgs.Data) { [void]$Event.MessageData.AppendLine($EventArgs.Data) }
        } -PassThru

        if (-not $proc.Start()) {
            throw "Failed to start child pwsh process for $scriptFile (Process.Start returned false)"
        }
        $proc.BeginOutputReadLine()
        $proc.BeginErrorReadLine()

        $exitedInTime = $proc.WaitForExit([Math]::Max(0, $TimeoutSeconds) * 1000)
        $timedOut = -not $exitedInTime

        if ($timedOut) {
            $killIssued = Stop-ProcessTree -Process $proc
            # Bounded cleanup wait: this is the ONLY authoritative signal
            # that cleanup actually succeeded. Stop-ProcessTree not throwing
            # only means the termination signal was issued, not that the
            # process (or its tree) has actually gone away -- so its return
            # value is never treated as proof of cleanup on its own. Never
            # blocks indefinitely even if the kill itself is incomplete.
            $cleanupVerified = $proc.WaitForExit(5000)
            if ($cleanupVerified) {
                # Drain any remaining async stream events now that the
                # process is confirmed gone, mirroring the non-timeout path.
                $proc.WaitForExit()
            }
            elseif (-not $killIssued) {
                # Both the kill attempt and the bounded wait failed -- the
                # process is very likely still alive. Surface this distinctly
                # so a stalled kill is never silently conflated with an
                # ordinary timeout.
                Write-Host "  [warning] Stop-ProcessTree failed to signal termination for PID $($proc.Id), and it did not exit within the bounded cleanup wait"
            }
        }
        else {
            # No-arg overload after the timed overload returns true is the
            # documented way to ensure the async redirected-stream events
            # have fully drained before we read $proc.ExitCode.
            $proc.WaitForExit()
            $exitCode = $proc.ExitCode
            # The process exited on its own within the timeout -- there is
            # nothing to kill/clean up, so cleanup is trivially satisfied.
            $cleanupVerified = $true
        }
    }
    finally {
        if ($null -ne $outSubscription) { Unregister-Event -SourceIdentifier $outSubscription.Name -ErrorAction SilentlyContinue }
        if ($null -ne $errSubscription) { Unregister-Event -SourceIdentifier $errSubscription.Name -ErrorAction SilentlyContinue }
        if ($null -ne $proc) { $proc.Dispose() }

        Set-Content -Path $stdoutPath -Value $stdoutBuilder.ToString() -Encoding utf8
        Set-Content -Path $stderrPath -Value $stderrBuilder.ToString() -Encoding utf8

        # Redirecting the child's streams (needed to bound/kill it on timeout
        # and avoid pipe deadlocks) means they are no longer inherited live
        # by this process's console the way the original `&` invocation did.
        # Echo the captured content now so the CI job log still shows exactly
        # what the extracted candidate script printed -- evidence is not
        # lost, just no longer interleaved in real time.
        if ($stdoutBuilder.Length -gt 0) {
            Write-Host "  [child stdout]"
            Write-Host $stdoutBuilder.ToString().TrimEnd()
        }
        if ($stderrBuilder.Length -gt 0) {
            Write-Host "  [child stderr]"
            Write-Host $stderrBuilder.ToString().TrimEnd()
        }

        Set-Location $originalLocation
        $env:PATH = $originalPath
        $env:MATRIX_MODE = $originalMode
        $env:MATRIX_TEST_THREADS = $originalThreads
        $env:FAKE_CORE_EXIT_CODE = $originalCoreExit
        $env:FAKE_DOC_EXIT_CODE = $originalDocExit
        $env:FAKE_TASK_LOG = $originalLog
    }

    $callLog = @()
    if (Test-Path $LogPath) {
        $callLog = @(Get-Content $LogPath)
    }

    return [PSCustomObject]@{
        ExitCode        = $exitCode
        TimedOut        = $timedOut
        CleanupVerified = $cleanupVerified
        Calls           = $callLog
        StdOutPath      = $stdoutPath
        StdErrPath      = $stderrPath
    }
}

$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("css2302-winexit-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null

try {
  try {
    $fakeBinDir = Join-Path $tempRoot "bin"
    New-FakeTaskBin -Dir $fakeBinDir | Out-Null
    $logPath = Join-Path $tempRoot "task-calls.log"

    $candidateBody = Get-WindowsRunBlock -Ref "HEAD"
    if (-not $candidateBody) {
        throw "Could not read HEAD:${WorkflowRelativePath} -- is this running inside a git checkout?"
    }
    $baselineBody = Get-WindowsRunBlock -Ref $KnownBuggyBaselineRef
    # Fail closed if the negative-control baseline is unreachable, rather
    # than silently skipping it (see Assert-BaselineReachable). Caught by
    # the outer catch below, which reports it and exits non-zero.
    Assert-BaselineReachable -Ref $KnownBuggyBaselineRef -Body $baselineBody

    $failures = New-Object System.Collections.Generic.List[string]

    function Test-Scenario {
        param(
            [string]$Name,
            [string]$ScriptBody,
            [string]$WorkDir,
            [int]$CoreExitCode,
            [int]$DocExitCode,
            [int]$ExpectedExitCode,
            [int]$ExpectedCallCount,
            [switch]$ExpectDocCallSkipped
        )

        Write-Host "Running scenario: $Name"
        $result = Invoke-RunBlock -ScriptBody $ScriptBody -WorkDir $WorkDir -FakeBinDir $fakeBinDir `
            -LogPath $logPath -CoreExitCode $CoreExitCode -DocExitCode $DocExitCode

        $ok = $true
        if ($result.TimedOut) {
            # A timeout is never a valid stand-in for an expected core/doc
            # exit code -- report it as its own failure mode instead of
            # comparing $result.ExitCode (which is $null here). Also surface
            # whether cleanup itself was verified: an unverified cleanup on
            # top of an unexpected timeout means a real orphan process may
            # still be running.
            $cleanupNote = if ($result.CleanupVerified) { "cleanup verified" } else { "CLEANUP NOT VERIFIED -- possible orphan process" }
            $failures.Add("[$Name] child pwsh process timed out unexpectedly (harness bug or genuine hang); $cleanupNote -- see $($result.StdOutPath) / $($result.StdErrPath)")
            $ok = $false
        }
        else {
            if ($result.ExitCode -ne $ExpectedExitCode) {
                $failures.Add("[$Name] expected exit code $ExpectedExitCode, got $($result.ExitCode)")
                $ok = $false
            }
            if ($result.Calls.Count -ne $ExpectedCallCount) {
                $failures.Add("[$Name] expected $ExpectedCallCount task invocation(s), observed $($result.Calls.Count): $($result.Calls -join ' | ')")
                $ok = $false
            }
            if ($ExpectDocCallSkipped -and (@($result.Calls | Where-Object { $_ -match '--doc' })).Count -gt 0) {
                $failures.Add("[$Name] doc-test task call should have been skipped after a core failure, but it ran")
                $ok = $false
            }
        }

        if ($ok) {
            Write-Host "  PASS"
        }
        else {
            Write-Host "  FAIL"
        }
    }

    $candidateWorkDir = Join-Path $tempRoot "candidate"
    New-Item -ItemType Directory -Path $candidateWorkDir -Force | Out-Null

    Test-Scenario -Name "candidate: core failure blocks doc-test and propagates exit code" `
        -ScriptBody $candidateBody -WorkDir $candidateWorkDir `
        -CoreExitCode 17 -DocExitCode 0 -ExpectedExitCode 17 -ExpectedCallCount 1 -ExpectDocCallSkipped

    Test-Scenario -Name "candidate: doc-test failure propagates after core success" `
        -ScriptBody $candidateBody -WorkDir $candidateWorkDir `
        -CoreExitCode 0 -DocExitCode 5 -ExpectedExitCode 5 -ExpectedCallCount 2

    Test-Scenario -Name "candidate: all-pass returns 0" `
        -ScriptBody $candidateBody -WorkDir $candidateWorkDir `
        -CoreExitCode 0 -DocExitCode 0 -ExpectedExitCode 0 -ExpectedCallCount 2

    # Baseline is confirmed reachable above (Assert-BaselineReachable did
    # not throw) -- always run the negative control, never skip it.
    $baselineWorkDir = Join-Path $tempRoot "baseline"
    New-Item -ItemType Directory -Path $baselineWorkDir -Force | Out-Null

    Write-Host "Running scenario: baseline ($KnownBuggyBaselineRef) reproduces the masking bug"
    $baselineResult = Invoke-RunBlock -ScriptBody $baselineBody -WorkDir $baselineWorkDir -FakeBinDir $fakeBinDir `
        -LogPath $logPath -CoreExitCode 17 -DocExitCode 0

    if ($baselineResult.TimedOut) {
        $cleanupNote = if ($baselineResult.CleanupVerified) { "cleanup verified" } else { "CLEANUP NOT VERIFIED -- possible orphan process" }
        $failures.Add("[baseline] child pwsh process timed out unexpectedly while running the $KnownBuggyBaselineRef script body ($cleanupNote) -- see $($baselineResult.StdOutPath) / $($baselineResult.StdErrPath)")
        Write-Host "  FAIL"
    }
    elseif ($baselineResult.ExitCode -eq 0 -and $baselineResult.Calls.Count -eq 2) {
        Write-Host "  PASS (negative control confirmed: the pre-fix script masks the core failure)"
    }
    else {
        $failures.Add("[baseline] expected the known-buggy $KnownBuggyBaselineRef script to mask a core failure (exit 0, 2 task calls), but got exit code $($baselineResult.ExitCode) with $($baselineResult.Calls.Count) call(s). This regression test's negative-control fixture may be stale.")
        Write-Host "  FAIL"
    }

    # --- Focused self-check: the fail-closed baseline guard actually fails
    # closed (reviewer finding 1). Exercises Assert-BaselineReachable
    # directly with a null body, independent of this checkout's real
    # history, so the check is meaningful even though the real baseline
    # above is reachable.
    Write-Host "Running scenario: fail-closed guard throws when the baseline body is missing"
    $missingBaselineThrew = $false
    try {
        Assert-BaselineReachable -Ref "0000000000000000000000000000000000000000" -Body $null
    }
    catch {
        $missingBaselineThrew = $true
    }
    if (-not $missingBaselineThrew) {
        $failures.Add("[fail-closed baseline guard] Assert-BaselineReachable did not throw for a null baseline body -- the negative control could silently be skipped again")
        Write-Host "  FAIL"
    }
    else {
        Write-Host "  PASS"
    }

    # --- Focused self-check: a hanging child is detected, killed, and
    # reported as its own failure mode (reviewer finding 2). Uses a short
    # overridden timeout (2s) against a deliberately sleeping script body so
    # this adds only ~2s to the run, not the full $ChildProcessTimeoutSeconds
    # default, and leaves no sleeping process behind (Stop-ProcessTree kills
    # the whole tree).
    Write-Host "Running scenario: hanging child is detected, killed, and reported as a timeout"
    $timeoutWorkDir = Join-Path $tempRoot "timeout-selfcheck"
    New-Item -ItemType Directory -Path $timeoutWorkDir -Force | Out-Null
    $hangingBody = "Start-Sleep -Seconds 60`nexit 0`n"
    $timeoutResult = Invoke-RunBlock -ScriptBody $hangingBody -WorkDir $timeoutWorkDir -FakeBinDir $fakeBinDir `
        -LogPath $logPath -CoreExitCode 0 -DocExitCode 0 -TimeoutSeconds 2
    if (-not $timeoutResult.TimedOut) {
        $failures.Add("[timeout self-check] expected the harness to detect and kill a hanging child within 2s, but TimedOut=$($timeoutResult.TimedOut) exitcode=$($timeoutResult.ExitCode)")
        Write-Host "  FAIL"
    }
    elseif (-not $timeoutResult.CleanupVerified) {
        # A timeout alone is not enough to call this a pass: it must also be
        # true that the kill was confirmed and the child actually exited
        # within the bounded cleanup wait. Otherwise this self-check could
        # pass with a live orphan `Start-Sleep` process still running.
        $failures.Add("[timeout self-check] child was detected as hung, but cleanup was not verified within the bounded wait (kill may have failed and the process may still be alive) -- CleanupVerified=$($timeoutResult.CleanupVerified)")
        Write-Host "  FAIL"
    }
    else {
        Write-Host "  PASS (hanging child was killed, cleanup verified, and reported as a timeout, not misread as a pass/fail)"
    }

    if ($failures.Count -gt 0) {
        Write-Host ""
        Write-Host "CSS-2302 Windows exit-code propagation regression FAILED:"
        foreach ($f in $failures) {
            Write-Host "  - $f"
        }
        exit 1
    }

    Write-Host ""
    Write-Host "CSS-2302 Windows exit-code propagation regression: all scenarios PASSED."
    exit 0
  }
  catch {
    Write-Host ""
    Write-Host "CSS-2302 Windows exit-code propagation regression FAILED with an unhandled error:"
    Write-Host "  $($_.Exception.Message)"
    exit 1
  }
}
finally {
    Remove-Item -Path $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
