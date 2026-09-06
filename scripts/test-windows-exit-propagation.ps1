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
    ever stops holding, this regression's fixture is stale.

    Exits non-zero (failing the CI job) if any candidate assertion fails, or
    if the extraction itself cannot locate the step (so a rename/reshape of
    the workflow can't silently defeat this test).

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
    MATRIX_MODE forced to "windows-core". Returns the child's exit code plus
    the fake task's call log.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$ScriptBody,
        [Parameter(Mandatory = $true)][string]$WorkDir,
        [Parameter(Mandatory = $true)][string]$FakeBinDir,
        [Parameter(Mandatory = $true)][string]$LogPath,
        [Parameter(Mandatory = $true)][int]$CoreExitCode,
        [Parameter(Mandatory = $true)][int]$DocExitCode
    )

    if (Test-Path $LogPath) {
        Remove-Item $LogPath -Force
    }

    $scriptFile = Join-Path $WorkDir "candidate.ps1"
    Set-Content -Path $scriptFile -Value $ScriptBody -Encoding utf8

    $testsDir = Join-Path $WorkDir "tests"
    New-Item -ItemType Directory -Path $testsDir -Force | Out-Null

    $originalPath = $env:PATH
    $originalMode = $env:MATRIX_MODE
    $originalThreads = $env:MATRIX_TEST_THREADS
    $originalCoreExit = $env:FAKE_CORE_EXIT_CODE
    $originalDocExit = $env:FAKE_DOC_EXIT_CODE
    $originalLog = $env:FAKE_TASK_LOG
    $originalLocation = Get-Location

    try {
        $env:PATH = "$FakeBinDir;$originalPath"
        $env:MATRIX_MODE = "windows-core"
        $env:MATRIX_TEST_THREADS = "1"
        $env:FAKE_CORE_EXIT_CODE = "$CoreExitCode"
        $env:FAKE_DOC_EXIT_CODE = "$DocExitCode"
        $env:FAKE_TASK_LOG = $LogPath

        Set-Location $WorkDir
        & pwsh -NoProfile -NonInteractive -File $scriptFile
        $exitCode = $LASTEXITCODE
    }
    finally {
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
        ExitCode = $exitCode
        Calls    = $callLog
    }
}

$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("css2302-winexit-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null

try {
    $fakeBinDir = Join-Path $tempRoot "bin"
    New-FakeTaskBin -Dir $fakeBinDir | Out-Null
    $logPath = Join-Path $tempRoot "task-calls.log"

    $candidateBody = Get-WindowsRunBlock -Ref "HEAD"
    if (-not $candidateBody) {
        throw "Could not read HEAD:${WorkflowRelativePath} -- is this running inside a git checkout?"
    }
    $baselineBody = Get-WindowsRunBlock -Ref $KnownBuggyBaselineRef

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

    if (-not $baselineBody) {
        Write-Host "Skipping known-buggy baseline check: $KnownBuggyBaselineRef is unreachable in this checkout's history."
    }
    else {
        $baselineWorkDir = Join-Path $tempRoot "baseline"
        New-Item -ItemType Directory -Path $baselineWorkDir -Force | Out-Null

        Write-Host "Running scenario: baseline ($KnownBuggyBaselineRef) reproduces the masking bug"
        $baselineResult = Invoke-RunBlock -ScriptBody $baselineBody -WorkDir $baselineWorkDir -FakeBinDir $fakeBinDir `
            -LogPath $logPath -CoreExitCode 17 -DocExitCode 0

        if ($baselineResult.ExitCode -eq 0 -and $baselineResult.Calls.Count -eq 2) {
            Write-Host "  PASS (negative control confirmed: the pre-fix script masks the core failure)"
        }
        else {
            $failures.Add("[baseline] expected the known-buggy $KnownBuggyBaselineRef script to mask a core failure (exit 0, 2 task calls), but got exit code $($baselineResult.ExitCode) with $($baselineResult.Calls.Count) call(s). This regression test's negative-control fixture may be stale.")
            Write-Host "  FAIL"
        }
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
finally {
    Remove-Item -Path $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
