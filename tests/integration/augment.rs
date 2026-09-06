//! Integration tests for the Augment Code (Auggie) preset.
//!
//! Exercises the full flow: real `git-ai checkpoint augment` invocation
//! through the test binary, real `TestRepo`, real attribution checks
//! against produced authorship notes.
//!
//! The hook payload schema mirrors what auggie sends per
//! https://docs.augmentcode.com/cli/hooks:
//!   - top-level: `hook_event_name`, `conversation_id`, `workspace_roots[]`
//!   - per-event: `tool_name`, `tool_input`
//!   - tool_input: `path` for `save-file`/`str-replace-editor`,
//!     `file_paths[]` for `remove-files`, `command` for `launch-process`

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, get_binary_path};
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::error::GitAiError;
use git_ai::mdm::utils::{clean_path, normalize_windows_path_for_shell};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn parse_augment(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("augment")?.parse(hook_input, "t_test123456789a")
}

// ============================================================================
// Preset routing tests (in-process)
// ============================================================================

#[test]
fn test_augment_preset_resolves() {
    assert!(
        resolve_preset("augment").is_ok(),
        "augment preset must be registered in resolve_preset"
    );
}

#[test]
fn test_augment_routes_save_file_to_post_file_edit() {
    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "conversation_id": "conv-1",
        "workspace_roots": ["/tmp/proj"],
        "tool_name": "save-file",
        "tool_input": {"path": "/tmp/proj/main.rs", "content": "fn main() {}"},
    })
    .to_string();
    let events = parse_augment(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "augment");
            assert_eq!(e.context.agent_id.id, "conv-1");
            assert_eq!(e.context.agent_id.model, "unknown");
            assert!(
                e.stream_source.is_none(),
                "stream_source should be None until an Augment reader lands"
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_augment_routes_str_replace_editor_to_post_file_edit() {
    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "conversation_id": "conv-2",
        "workspace_roots": ["/tmp/proj"],
        "tool_name": "str-replace-editor",
        "tool_input": {
            "path": "/tmp/proj/lib.rs",
            "command": "str_replace",
            "old_str_1": "a",
            "new_str_1": "b",
        },
    })
    .to_string();
    let events = parse_augment(&hook_input).unwrap();
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(
                e.file_paths,
                vec![std::path::PathBuf::from("/tmp/proj/lib.rs")]
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_augment_routes_remove_files_with_array() {
    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "conversation_id": "conv-3",
        "workspace_roots": ["/tmp/proj"],
        "tool_name": "remove-files",
        "tool_input": {"file_paths": ["/tmp/proj/a.rs", "/tmp/proj/b.rs"]},
    })
    .to_string();
    let events = parse_augment(&hook_input).unwrap();
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.file_paths.len(), 2);
            assert_eq!(
                e.file_paths,
                vec![
                    std::path::PathBuf::from("/tmp/proj/a.rs"),
                    std::path::PathBuf::from("/tmp/proj/b.rs"),
                ]
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_augment_routes_launch_process_to_bash() {
    let pre = json!({
        "hook_event_name": "PreToolUse",
        "conversation_id": "conv-4",
        "workspace_roots": ["/tmp/proj"],
        "tool_name": "launch-process",
        "tool_input": {"command": "git status"},
    })
    .to_string();
    let events = parse_augment(&pre).unwrap();
    match &events[0] {
        ParsedHookEvent::PreBashCall(e) => {
            assert_eq!(e.context.agent_id.tool, "augment");
            assert_eq!(e.tool_use_id, "bash");
        }
        _ => panic!("Expected PreBashCall"),
    }

    let post = json!({
        "hook_event_name": "PostToolUse",
        "conversation_id": "conv-4",
        "workspace_roots": ["/tmp/proj"],
        "tool_name": "launch-process",
        "tool_input": {"command": "git status"},
        "tool_output": "...",
    })
    .to_string();
    let events = parse_augment(&post).unwrap();
    match &events[0] {
        ParsedHookEvent::PostBashCall(e) => {
            assert_eq!(e.context.agent_id.tool, "augment");
            assert!(e.stream_source.is_none());
        }
        _ => panic!("Expected PostBashCall"),
    }
}

#[test]
fn test_augment_skips_lifecycle_events_silently() {
    // Lifecycle events carry no tool/file information and are deliberately
    // not checkpointed, but they are not an error either: the installer's
    // catch-all ".*" matcher fires this hook for every event Augment sends,
    // so a documented lifecycle event is an expected, successful no-op
    // (empty Ok), not a PresetError (CSS-2302, discussion_r3942067001).
    for event in ["SessionStart", "SessionEnd", "Stop"] {
        let payload = json!({
            "hook_event_name": event,
            "conversation_id": "conv-rej",
            "workspace_roots": ["/tmp/proj"],
        })
        .to_string();
        let result = parse_augment(&payload);
        assert!(
            result.unwrap().is_empty(),
            "expected silent no-op for lifecycle event {event}"
        );
    }
}

#[test]
fn test_augment_skips_unsupported_tools_silently() {
    // Read-only/inspection tools are documented but intentionally never
    // checkpointed. Because the installer's catch-all ".*" matcher fires
    // this hook for every tool call, these must silently no-op rather than
    // surface as a PresetError -- otherwise `git-ai checkpoint` prints a
    // spurious "augment preset error" to stderr on an otherwise-successful
    // exit 0, which Augment renders as a user-visible warning for ordinary
    // non-edit tool use (CSS-2302, discussion_r3942067001).
    for tool in [
        "view",
        "grep-search",
        "codebase-retrieval",
        "web-fetch",
        "web-search",
    ] {
        let payload = json!({
            "hook_event_name": "PostToolUse",
            "conversation_id": "conv-tool",
            "workspace_roots": ["/tmp/proj"],
            "tool_name": tool,
            "tool_input": {},
        })
        .to_string();
        let result = parse_augment(&payload);
        assert!(
            result.unwrap().is_empty(),
            "expected silent no-op for unsupported tool {tool}"
        );
    }
}

// ============================================================================
// End-to-end tests using TestRepo
// ============================================================================

#[test]
fn test_augment_e2e_save_file_attributes_to_augment() {
    let repo = TestRepo::new();

    let file_path = repo.path().join("app.py");
    fs::write(&file_path, "def hello():\n    pass\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(
        &file_path,
        "def hello():\n    pass\ndef world():\n    pass\n",
    )
    .unwrap();

    let canonical_root = repo.canonical_path();
    let canonical_file = canonical_root.join("app.py");
    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "conversation_id": "augment-e2e-1",
        "workspace_roots": [canonical_root.to_string_lossy().to_string()],
        "tool_name": "save-file",
        "tool_input": {
            "path": canonical_file.to_string_lossy().to_string(),
            "content": "def hello():\n    pass\ndef world():\n    pass\n",
        },
    })
    .to_string();

    repo.git_ai(&["checkpoint", "augment", "--hook-input", &hook_input])
        .unwrap();

    let commit = repo
        .stage_all_and_commit("Add world function")
        .expect("commit should succeed");

    let mut file = repo.filename("app.py");
    file.assert_lines_and_blame(crate::lines![
        "def hello():".human(),
        "    pass".human(),
        "def world():".ai(),
        "    pass".ai(),
    ]);

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Should have AI attestations from Augment"
    );
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Should have a session record"
    );
    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session record should exist");
    assert_eq!(session.agent_id.tool, "augment");
    assert_eq!(session.agent_id.id, "augment-e2e-1");
    assert_eq!(
        session.agent_id.model, "unknown",
        "model defaults to 'unknown' when context not enabled"
    );
}

#[test]
fn test_augment_e2e_pre_then_post_isolates_human_lines() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("index.ts");
    fs::write(&file_path, "console.log('hello');\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let canonical_root = repo.canonical_path();
    let canonical_file = canonical_root.join("index.ts");

    let pre = json!({
        "hook_event_name": "PreToolUse",
        "conversation_id": "augment-e2e-2",
        "workspace_roots": [canonical_root.to_string_lossy().to_string()],
        "tool_name": "save-file",
        "tool_input": {
            "path": canonical_file.to_string_lossy().to_string(),
            "content": "console.log('hello');\nconsole.log('from augment');\n",
        },
    })
    .to_string();
    repo.git_ai(&["checkpoint", "augment", "--hook-input", &pre])
        .unwrap();

    fs::write(
        &file_path,
        "console.log('hello');\nconsole.log('from augment');\n",
    )
    .unwrap();

    let post = json!({
        "hook_event_name": "PostToolUse",
        "conversation_id": "augment-e2e-2",
        "workspace_roots": [canonical_root.to_string_lossy().to_string()],
        "tool_name": "save-file",
        "tool_input": {
            "path": canonical_file.to_string_lossy().to_string(),
            "content": "console.log('hello');\nconsole.log('from augment');\n",
        },
    })
    .to_string();
    repo.git_ai(&["checkpoint", "augment", "--hook-input", &post])
        .unwrap();

    let commit = repo
        .stage_all_and_commit("Add augment line")
        .expect("commit should succeed");

    let mut file = repo.filename("index.ts");
    file.assert_lines_and_blame(crate::lines![
        "console.log('hello');".human(),
        "console.log('from augment');".ai(),
    ]);

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Should have attestations"
    );
}

#[test]
fn test_augment_e2e_str_replace_editor_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("greet.py");
    fs::write(&file_path, "def greet():\n    print('hi')\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(&file_path, "def greet():\n    print('hello world')\n").unwrap();

    let canonical_root = repo.canonical_path();
    let canonical_file = canonical_root.join("greet.py");
    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "conversation_id": "augment-e2e-3",
        "workspace_roots": [canonical_root.to_string_lossy().to_string()],
        "tool_name": "str-replace-editor",
        "tool_input": {
            "path": canonical_file.to_string_lossy().to_string(),
            "command": "str_replace",
            "old_str_1": "    print('hi')",
            "new_str_1": "    print('hello world')",
        },
    })
    .to_string();

    repo.git_ai(&["checkpoint", "augment", "--hook-input", &hook_input])
        .unwrap();

    let commit = repo
        .stage_all_and_commit("Replace string")
        .expect("commit should succeed");

    let mut file = repo.filename("greet.py");
    file.assert_lines_and_blame(crate::lines![
        "def greet():".human(),
        "    print('hello world')".ai(),
    ]);

    assert!(!commit.authorship_log.attestations.is_empty());
}

// ============================================================================
// v2 (`auggie-v2` / cosmos-agent) preset tests (CSS-2302)
//
// v2's hook payload uses `hook_type` (not `hook_event_name`), lowercase
// Claude-Code-shaped tool names (`write`/`edit`/`bash`, not v1's kebab-case),
// and never sends `workspace_roots` -- the workspace root is the hook
// subprocess's own cwd, which `TestRepo::git_ai` sets via
// `Command::current_dir(&self.path)`, so file paths below are workspace-
// relative rather than absolute.
// ============================================================================

#[test]
fn test_augment_v2_routes_write_to_post_file_edit() {
    let hook_input = json!({
        "hook_type": "PostToolUse",
        "tool_name": "write",
        "tool_input": {"path": "main.rs", "content": "fn main() {}"},
        "tool_result": [{"type": "text", "text": "Successfully wrote 12 bytes to main.rs"}],
        "tool_is_error": false,
    })
    .to_string();
    let events = parse_augment(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "augment");
            assert_eq!(e.context.agent_id.model, "unknown");
            assert!(
                e.stream_source.is_none(),
                "stream_source should be None until a v2 reader lands"
            );
            assert!(
                e.file_paths[0].ends_with("main.rs"),
                "expected main.rs, got {}",
                e.file_paths[0].display()
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_augment_v2_routes_edit_to_post_file_edit() {
    let hook_input = json!({
        "hook_type": "PostToolUse",
        "tool_name": "edit",
        "tool_input": {
            "path": "lib.rs",
            "edits": [{"oldText": "a", "newText": "b"}],
        },
        "tool_result": [{"type": "text", "text": "ok"}],
        "tool_is_error": false,
    })
    .to_string();
    let events = parse_augment(&hook_input).unwrap();
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.file_paths[0].ends_with("lib.rs"));
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_augment_v2_routes_bash_to_bash_call() {
    let pre = json!({
        "hook_type": "PreToolUse",
        "tool_name": "bash",
        "tool_input": {"command": "git status"},
    })
    .to_string();
    let events = parse_augment(&pre).unwrap();
    match &events[0] {
        ParsedHookEvent::PreBashCall(e) => {
            assert_eq!(e.context.agent_id.tool, "augment");
            assert_eq!(e.tool_use_id, "bash");
            assert_eq!(e.command.as_deref(), Some("git status"));
        }
        _ => panic!("Expected PreBashCall"),
    }

    let post = json!({
        "hook_type": "PostToolUse",
        "tool_name": "bash",
        "tool_input": {"command": "git status"},
        "tool_result": [{"type": "text", "text": "clean"}],
        "tool_is_error": false,
    })
    .to_string();
    let events = parse_augment(&post).unwrap();
    match &events[0] {
        ParsedHookEvent::PostBashCall(e) => {
            assert_eq!(e.context.agent_id.tool, "augment");
            assert!(e.stream_source.is_none());
        }
        _ => panic!("Expected PostBashCall"),
    }
}

#[test]
fn test_augment_v2_skips_lifecycle_events_silently() {
    // Same rationale as test_augment_skips_lifecycle_events_silently
    // (v1): documented lifecycle hook_types are an expected, successful
    // no-op, not a PresetError (CSS-2302, discussion_r3942067001).
    for hook_type in [
        "SessionStart",
        "SessionEnd",
        "Stop",
        "Notification",
        "PromptSubmit",
    ] {
        let payload = json!({"hook_type": hook_type}).to_string();
        let result = parse_augment(&payload);
        assert!(
            result.unwrap().is_empty(),
            "expected silent no-op for v2 lifecycle event {hook_type}"
        );
    }
}

#[test]
fn test_augment_v2_skips_read_tool_silently() {
    // `read` is v2's default toolset name for a non-mutating tool; it must
    // not be checkpointed (ToolClass::Skip), and -- since the installer's
    // catch-all ".*" matcher fires this hook for every tool call -- that
    // must be a silent no-op rather than a PresetError (CSS-2302,
    // discussion_r3942067001).
    let payload = json!({
        "hook_type": "PreToolUse",
        "tool_name": "read",
        "tool_input": {"path": "foo.txt"},
    })
    .to_string();
    let result = parse_augment(&payload);
    assert!(
        result.unwrap().is_empty(),
        "expected silent no-op for read tool"
    );
}

#[test]
fn test_augment_v1_and_v2_shapes_are_never_confused() {
    // A v1-shaped payload must never be routed through the v2 path (and
    // vice versa) -- exercised end-to-end through the same preset
    // instance to guard against any future shared-state regression.
    let v1_input = json!({
        "hook_event_name": "PostToolUse",
        "conversation_id": "conv-mix-1",
        "workspace_roots": ["/tmp/proj"],
        "tool_name": "save-file",
        "tool_input": {"path": "a.rs"},
    })
    .to_string();
    let v2_input = json!({
        "hook_type": "PostToolUse",
        "tool_name": "write",
        "tool_input": {"path": "b.rs"},
    })
    .to_string();

    match &parse_augment(&v1_input).unwrap()[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.id, "conv-mix-1");
        }
        _ => panic!("Expected PostFileEdit"),
    }
    match &parse_augment(&v2_input).unwrap()[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_ne!(e.context.agent_id.id, "conv-mix-1");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

// ----------------------------------------------------------------------
// End-to-end tests using TestRepo (v2 shape)
// ----------------------------------------------------------------------

#[test]
fn test_augment_v2_e2e_write_attributes_to_augment() {
    let repo = TestRepo::new();

    let file_path = repo.path().join("app.py");
    fs::write(&file_path, "def hello():\n    pass\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(
        &file_path,
        "def hello():\n    pass\ndef world():\n    pass\n",
    )
    .unwrap();

    // v2 never sends workspace_roots; `tool_input.path` is workspace-
    // relative and resolved against the hook subprocess's own cwd, which
    // `repo.git_ai` sets to `repo.path()`.
    let hook_input = json!({
        "hook_type": "PostToolUse",
        "tool_name": "write",
        "tool_input": {
            "path": "app.py",
            "content": "def hello():\n    pass\ndef world():\n    pass\n",
        },
        "tool_result": [{"type": "text", "text": "Successfully wrote 46 bytes to app.py"}],
        "tool_is_error": false,
    })
    .to_string();

    repo.git_ai(&["checkpoint", "augment", "--hook-input", &hook_input])
        .unwrap();

    let commit = repo
        .stage_all_and_commit("Add world function")
        .expect("commit should succeed");

    let mut file = repo.filename("app.py");
    file.assert_lines_and_blame(crate::lines![
        "def hello():".human(),
        "    pass".human(),
        "def world():".ai(),
        "    pass".ai(),
    ]);

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Should have AI attestations from Augment v2"
    );
    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session record should exist");
    assert_eq!(session.agent_id.tool, "augment");
    assert_eq!(
        session.agent_id.model, "unknown",
        "model defaults to 'unknown' when no v2 session file is mineable"
    );
}

#[test]
fn test_augment_v2_e2e_pre_then_post_isolates_human_lines() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("index.ts");
    fs::write(&file_path, "console.log('hello');\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let pre = json!({
        "hook_type": "PreToolUse",
        "tool_name": "write",
        "tool_input": {
            "path": "index.ts",
            "content": "console.log('hello');\nconsole.log('from augment v2');\n",
        },
    })
    .to_string();
    repo.git_ai(&["checkpoint", "augment", "--hook-input", &pre])
        .unwrap();

    fs::write(
        &file_path,
        "console.log('hello');\nconsole.log('from augment v2');\n",
    )
    .unwrap();

    let post = json!({
        "hook_type": "PostToolUse",
        "tool_name": "write",
        "tool_input": {
            "path": "index.ts",
            "content": "console.log('hello');\nconsole.log('from augment v2');\n",
        },
        "tool_result": [{"type": "text", "text": "ok"}],
        "tool_is_error": false,
    })
    .to_string();
    repo.git_ai(&["checkpoint", "augment", "--hook-input", &post])
        .unwrap();

    let commit = repo
        .stage_all_and_commit("Add augment v2 line")
        .expect("commit should succeed");

    let mut file = repo.filename("index.ts");
    file.assert_lines_and_blame(crate::lines![
        "console.log('hello');".human(),
        "console.log('from augment v2');".ai(),
    ]);

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Should have attestations"
    );
}

#[test]
fn test_augment_v2_e2e_edit_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("greet.py");
    fs::write(&file_path, "def greet():\n    print('hi')\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(&file_path, "def greet():\n    print('hello world')\n").unwrap();

    let hook_input = json!({
        "hook_type": "PostToolUse",
        "tool_name": "edit",
        "tool_input": {
            "path": "greet.py",
            "edits": [{"oldText": "print('hi')", "newText": "print('hello world')"}],
        },
        "tool_result": [{"type": "text", "text": "ok"}],
        "tool_is_error": false,
    })
    .to_string();

    repo.git_ai(&["checkpoint", "augment", "--hook-input", &hook_input])
        .unwrap();

    let commit = repo
        .stage_all_and_commit("Replace string via v2 edit")
        .expect("commit should succeed");

    let mut file = repo.filename("greet.py");
    file.assert_lines_and_blame(crate::lines![
        "def greet():".human(),
        "    print('hello world')".ai(),
    ]);

    assert!(!commit.authorship_log.attestations.is_empty());
}

// ============================================================================
// Handler-level stderr regression tests (CSS-2302, discussion_r3942067001)
//
// The installer wires the Augment hook under the catch-all ".*" matcher, so
// `git-ai checkpoint augment` runs for every tool call, including tools the
// preset deliberately never checkpoints (read-only tools, lifecycle events).
// `handle_checkpoint` always exits 0 for a checkpoint invocation, but Augment
// renders any exit-0 stderr as a user-visible warning -- so these assert the
// real subprocess's combined stdout+stderr, not just the preset's Result
// type, to guard against a future regression that reintroduces a printed
// PresetError for an intentional no-op.
// ============================================================================

#[test]
fn test_augment_handler_readonly_tool_exits_clean_with_empty_stderr() {
    let repo = TestRepo::new();
    let hook_input = json!({
        "hook_event_name": "PreToolUse",
        "conversation_id": "conv-handler-view",
        "workspace_roots": [repo.canonical_path().to_string_lossy().to_string()],
        "tool_name": "view",
        "tool_input": {"path": "src/main.rs"},
    })
    .to_string();

    let output = repo
        .git_ai(&["checkpoint", "augment", "--hook-input", &hook_input])
        .expect("git-ai checkpoint must exit 0 for an intentional read-only-tool skip");
    assert!(
        output.trim().is_empty(),
        "expected empty stdout/stderr for an intentional read-only-tool skip, got: {output:?}"
    );
}

#[test]
fn test_augment_handler_lifecycle_event_exits_clean_with_empty_stderr() {
    let repo = TestRepo::new();
    let hook_input = json!({
        "hook_event_name": "SessionStart",
        "conversation_id": "conv-handler-lifecycle",
        "workspace_roots": [repo.canonical_path().to_string_lossy().to_string()],
    })
    .to_string();

    let output = repo
        .git_ai(&["checkpoint", "augment", "--hook-input", &hook_input])
        .expect("git-ai checkpoint must exit 0 for an intentional lifecycle-event skip");
    assert!(
        output.trim().is_empty(),
        "expected empty stdout/stderr for an intentional lifecycle-event skip, got: {output:?}"
    );
}

#[test]
fn test_augment_v2_handler_read_tool_exits_clean_with_empty_stderr() {
    let repo = TestRepo::new();
    let hook_input = json!({
        "hook_type": "PreToolUse",
        "tool_name": "read",
        "tool_input": {"path": "foo.txt"},
    })
    .to_string();

    let output = repo
        .git_ai(&["checkpoint", "augment", "--hook-input", &hook_input])
        .expect("git-ai checkpoint must exit 0 for an intentional v2 read-tool skip");
    assert!(
        output.trim().is_empty(),
        "expected empty stdout/stderr for an intentional v2 read-tool skip, got: {output:?}"
    );
}

#[test]
fn test_augment_handler_malformed_json_reports_actionable_stderr() {
    let repo = TestRepo::new();

    let output = repo
        .git_ai(&["checkpoint", "augment", "--hook-input", "not valid json"])
        .expect("git-ai checkpoint always exits 0, even on a preset error");
    assert!(
        output.contains("augment preset error") && output.contains("Invalid JSON in hook_input"),
        "expected an actionable diagnostic for malformed JSON, got: {output:?}"
    );
}

#[test]
fn test_augment_handler_ambiguous_discriminator_reports_actionable_stderr() {
    let repo = TestRepo::new();
    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "hook_type": "PostToolUse",
        "conversation_id": "conv-ambiguous",
        "workspace_roots": [repo.canonical_path().to_string_lossy().to_string()],
    })
    .to_string();

    let output = repo
        .git_ai(&["checkpoint", "augment", "--hook-input", &hook_input])
        .expect("git-ai checkpoint always exits 0, even on a preset error");
    assert!(
        output.contains("Ambiguous Augment hook_input"),
        "expected an actionable diagnostic for an ambiguous discriminator, got: {output:?}"
    );
}

#[test]
fn test_augment_handler_wrong_type_discriminator_reports_actionable_stderr() {
    let repo = TestRepo::new();
    let hook_input = json!({
        "hook_event_name": 123,
        "conversation_id": "conv-wrong-type",
        "workspace_roots": [repo.canonical_path().to_string_lossy().to_string()],
    })
    .to_string();

    let output = repo
        .git_ai(&["checkpoint", "augment", "--hook-input", &hook_input])
        .expect("git-ai checkpoint always exits 0, even on a preset error");
    assert!(
        output.contains("hook_event_name must be a string"),
        "expected an actionable diagnostic for a wrong-type discriminator, got: {output:?}"
    );
}

// ============================================================================
// Real Auggie hook-dispatch regression (CSS-2302)
//
// The e2e tests above invoke `git-ai checkpoint augment` directly as a
// subprocess of the *default* compiled test binary. None of them exercise
// the actual command Auggie's installer writes into
// `~/.augment/settings.json`, nor the real algorithm Auggie's hook executor
// (`hook-executor.ts` / `hook-executor-platform.ts` in augmentcode/augment,
// read-only reference) uses to turn that command string into a spawned
// process.
//
// Per https://docs.augmentcode.com/cli/hooks, `hooks[].hooks[].command` is
// documented as "a path to the script to execute (must use a supported
// script extension: .ps1, .cmd, .bat, or .sh)". Auggie's *actual* hook
// executor is more permissive than the docs suggest:
// `parseWindowsCommand`/`parseUnixCommand` in hook-executor-platform.ts only
// special-case a command whose first token ends in one of those four
// extensions (routed through powershell.exe / bash.exe / cmd.exe) or one
// that contains a shell metacharacter (`|&;><$` or a backtick, routed
// through cmd.exe /c or bash -c). git-ai's installer-generated command
// (`'<binary path>' checkpoint augment --hook-input stdin`) is neither: its
// first token is the git-ai binary itself (`.exe` on Windows, no extension
// on Unix), and it contains no shell metacharacters. Both platform branches
// therefore fall through to the SAME `splitCommand` quote-aware tokenizer
// and spawn the result directly with `shell: false` -- no OS shell is ever
// involved for this command, on Windows OR Unix.
//
// The test below verifies those two preconditions against the REAL
// installer output (so a future installer change that adds a shell
// metacharacter or a script extension is caught), reproduces
// `splitCommand`'s tokenization faithfully in Rust, and spawns the
// resulting argv exactly as Auggie would: no shell, with the git-ai binary
// copied to a path containing a space (a realistic install location, e.g.
// "Program Files"). This code path runs unmodified on Unix and Windows:
// unlike the OS-level CreateProcess/execve syscalls (which do differ), the
// argv-splitting algorithm exercised here is the identical,
// platform-independent logic Auggie runs on both operating systems for this
// specific command shape. Running it in this Linux sandbox is therefore
// genuine evidence for the Windows dispatch outcome, but it is NOT a
// substitute for an actual Windows OS-level process spawn -- that still
// requires the hosted Windows CI matrix.
// ============================================================================

/// Faithful port of `getFirstToken` in `hook-executor-platform.ts`
/// (augmentcode/augment, read-only reference).
fn augment_get_first_token(command: &str) -> &str {
    if let Some(rest) = command.strip_prefix('"')
        && let Some(end) = rest.find('"')
    {
        return &rest[..end];
    }
    if let Some(rest) = command.strip_prefix('\'')
        && let Some(end) = rest.find('\'')
    {
        return &rest[..end];
    }
    match command.find(' ') {
        Some(idx) => &command[..idx],
        None => command,
    }
}

/// Faithful port of `hasShellMetacharacters` (same file as above).
fn augment_has_shell_metacharacters(command: &str) -> bool {
    command.starts_with('~') || command.chars().any(|c| "|&;><$`".contains(c))
}

/// Faithful port of `splitCommand` (same file as above): a quote-aware
/// tokenizer that is NOT a real shell word-splitter -- unlike a POSIX
/// shell, it does not understand `'\''`-escaped quotes inside a
/// single-quoted segment. This is Auggie's actual fallback for a command
/// with spaces and no shell metacharacters, used identically on Windows
/// and Unix (both `parseWindowsCommand` and `parseUnixCommand` delegate to
/// it).
fn augment_split_command(command: &str) -> (String, Vec<String>) {
    let re = regex::Regex::new(r#""([^"]*(?:\\.[^"]*)*)"|'([^']*(?:\\.[^']*)*)'|(\S+)"#).unwrap();
    let mut parts = Vec::new();
    for cap in re.captures_iter(command) {
        let token = cap
            .get(1)
            .or_else(|| cap.get(2))
            .or_else(|| cap.get(3))
            .expect("regex alternation always captures exactly one group");
        parts.push(token.as_str().to_string());
    }
    if parts.is_empty() {
        return (command.to_string(), Vec::new());
    }
    (parts[0].clone(), parts[1..].to_vec())
}

#[test]
fn test_augment_installer_command_spacey_path_dispatches_without_shell_and_checkpoints() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("app.py");
    fs::write(&file_path, "def hello():\n    pass\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Copy the compiled test binary to an install location containing a
    // space -- a realistic "Program Files"-style path (the exact scenario
    // from AugmentInstaller's `q2`/`q4` unit tests, now exercised through
    // the real installer + real dispatch instead of just the
    // string-building helper).
    let install_root = TempDir::new().unwrap();

    // On Unix, exercise a deterministic directory alias (raw path !=
    // canonical path, same real file) so this regression test proves the
    // canonicalize-before-compare fix locally on Linux too, not only via
    // hosted macOS's inherent `/var` -> `/private/var` TempDir alias (the
    // actual CSS-2302 regression). Windows is excluded: creating symlinks
    // there typically needs elevated privileges/Developer Mode, and its
    // canonicalization concern is the `\\?\` extended-length prefix
    // (already handled by `clean_path`), not a directory alias.
    #[cfg(unix)]
    let alias_parent = TempDir::new().unwrap();
    #[cfg(unix)]
    let spacey_root = {
        let alias = alias_parent.path().join("install-alias");
        std::os::unix::fs::symlink(install_root.path(), &alias)
            .expect("failed to create deterministic path-alias symlink");
        alias
    };
    #[cfg(not(unix))]
    let spacey_root = install_root.path().to_path_buf();

    let spacey_dir = spacey_root.join("Program Files").join("git-ai install");
    fs::create_dir_all(&spacey_dir).unwrap();
    let exe_name = if cfg!(windows) {
        "git-ai.exe"
    } else {
        "git-ai"
    };
    let spacey_binary = spacey_dir.join(exe_name);
    fs::copy(get_binary_path(), &spacey_binary).unwrap();

    // Isolated HOME for the installer subprocess: pre-create `.augment/` so
    // AugmentInstaller's dotfile fallback (`check_hooks_with`) reports
    // tool_installed=true without needing a real `auggie`/`auggie-v2`/
    // `cosmos-agent` on PATH -- a realistic "Augment CLI configured before,
    // hooks not yet installed" scenario.
    let install_home = TempDir::new().unwrap();
    fs::create_dir_all(install_home.path().join(".augment")).unwrap();
    let install_test_db = install_home.path().join("install-hooks.db");

    // Run the REAL `git-ai install-hooks` CLI *as* the spacey-path binary,
    // so the installer's own `get_current_binary_path()`
    // (`std::env::current_exe()`) naturally resolves to the spacey path --
    // exactly reproducing what a real user gets installing git-ai there.
    let mut install_cmd = Command::new(&spacey_binary);
    install_cmd
        .arg("install-hooks")
        .current_dir(install_home.path())
        .env("HOME", install_home.path())
        .env("GIT_AI_TEST_DB_PATH", &install_test_db)
        .env("GITAI_TEST_DB_PATH", &install_test_db)
        .env("GIT_CONFIG_GLOBAL", install_home.path().join(".gitconfig"))
        .env("GIT_AI_ALLOW_SUPERUSER", "1")
        .env("GIT_AI_DEBUG", "0");
    #[cfg(windows)]
    install_cmd
        .env("USERPROFILE", install_home.path())
        .env(
            "APPDATA",
            install_home.path().join("AppData").join("Roaming"),
        )
        .env(
            "LOCALAPPDATA",
            install_home.path().join("AppData").join("Local"),
        );
    let install_output = install_cmd
        .output()
        .expect("git-ai install-hooks must spawn");
    assert!(
        install_output.status.success(),
        "git-ai install-hooks failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&install_output.stdout),
        String::from_utf8_lossy(&install_output.stderr),
    );

    let settings_path = install_home.path().join(".augment").join("settings.json");
    assert!(
        settings_path.exists(),
        "installer must write ~/.augment/settings.json for a detected Augment install"
    );
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    let desired_cmd = settings["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse hooks must be an array")
        .iter()
        .flat_map(|block| block["hooks"].as_array().cloned().unwrap_or_default())
        .find_map(|hook| {
            hook.get("command")
                .and_then(|c| c.as_str())
                .filter(|cmd| cmd.contains("checkpoint augment --hook-input stdin"))
                .map(|s| s.to_string())
        })
        .expect("augment PreToolUse hook command must be present after install");

    // Determine (don't assume) which of Auggie's real dispatch branches
    // applies: neither a dispatch-script extension nor a shell
    // metacharacter is present, so both parseWindowsCommand and
    // parseUnixCommand fall through to the identical splitCommand +
    // spawn(shell:false) path.
    let first_token = augment_get_first_token(&desired_cmd).to_ascii_lowercase();
    for ext in [".ps1", ".sh", ".bat", ".cmd"] {
        assert!(
            !first_token.ends_with(ext),
            "installer command's first token carries a dispatch-script \
             extension ({ext}); Auggie would route this through a script \
             interpreter instead of the direct spawn(shell:false) fallback \
             this test exercises -- got: {desired_cmd}"
        );
    }
    assert!(
        !augment_has_shell_metacharacters(&desired_cmd),
        "installer command contains a shell metacharacter; Auggie would wrap \
         it in cmd.exe /c or bash -c instead of the direct spawn(shell:false) \
         fallback this test exercises -- got: {desired_cmd}"
    );

    let (program, args) = augment_split_command(&desired_cmd);
    // The installer resolves its own executable identity via
    // `get_current_binary_path()` (canonicalize + `clean_path`), then
    // renders it with `normalize_windows_path_for_shell` -- mirror that
    // exact pipeline here rather than comparing against the raw, pre-copy
    // spacey path. This is a platform-generic fix (plain `Path::canonicalize`
    // plus the installer's own helpers), not a macOS-only prefix hack, and it
    // resolves BOTH hosted-CI alias classes actually observed for this test:
    //   - macOS: the temp dir under `spacey_dir` is reached through a
    //     `/var` -> `/private/var` symlink (`/private/var/folders/...` vs
    //     `/var/folders/...` for the same file); `canonicalize` resolves the
    //     symlink via `realpath`.
    //   - Windows: GitHub-hosted runners report `%TEMP%` using the account's
    //     NTFS 8.3 short name (`C:/Users/RUNNER~1/AppData/...` vs the real
    //     long name `C:/Users/runneradmin/AppData/...` for the same file);
    //     `canonicalize` resolves this too, since `GetFinalPathNameByHandleW`
    //     is called with the default `FILE_NAME_NORMALIZED` flag, which
    //     returns the normalized (long-name) form of every path component.
    // Either way the real installed binary and the freshly copied
    // `spacey_binary` are the same executable reached via two textually
    // different (but canonically identical) paths; comparing raw strings
    // falsely fails on that alias.
    let expected_program = normalize_windows_path_for_shell(&clean_path(
        spacey_binary
            .canonicalize()
            .expect("spacey_binary must exist and be resolvable after being copied"),
    ));
    assert_eq!(
        program, expected_program,
        "Auggie's real tokenizer must resolve the quoted spacey path as a \
         single argv[0] token, not split it at the space, and it must match \
         the installer's canonicalized executable identity"
    );
    assert_eq!(args, vec!["checkpoint", "augment", "--hook-input", "stdin"]);

    // Build the realistic PostToolUse save-file hook payload (same shape as
    // `test_augment_e2e_save_file_attributes_to_augment` above).
    fs::write(
        &file_path,
        "def hello():\n    pass\ndef world():\n    pass\n",
    )
    .unwrap();
    let canonical_root = repo.canonical_path();
    let canonical_file = canonical_root.join("app.py");
    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "conversation_id": "augment-windows-dispatch-1",
        "workspace_roots": [canonical_root.to_string_lossy().to_string()],
        "tool_name": "save-file",
        "tool_input": {
            "path": canonical_file.to_string_lossy().to_string(),
            "content": "def hello():\n    pass\ndef world():\n    pass\n",
        },
    })
    .to_string();

    // Spawn EXACTLY as Auggie's hook-executor.ts does: no shell, argv
    // resolved by `parseCommand`, event JSON written to stdin then closed.
    // Env/cwd match what a normal `repo.git_ai(...)` call would use (same
    // per-test daemon sockets/db path), harvested from the test harness's
    // own command builder via the stable `Command::get_envs`/
    // `get_current_dir` introspection so this test never needs its own
    // copy of that (private, intentionally non-public) wiring.
    let baseline = repo.git_ai_command_without_pre_sync_for_test(&[], &[]);
    let mut hook_cmd = Command::new(&program);
    hook_cmd.args(&args);
    if let Some(dir) = baseline.get_current_dir() {
        hook_cmd.current_dir(dir);
    }
    for (key, value) in baseline.get_envs() {
        match value {
            Some(v) => hook_cmd.env(key, v),
            None => hook_cmd.env_remove(key),
        };
    }
    hook_cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = hook_cmd
        .spawn()
        .expect("real dispatch must spawn the copied binary");
    child
        .stdin
        .take()
        .expect("stdin must be piped")
        .write_all(hook_input.as_bytes())
        .expect("write hook JSON to stdin");
    let output = child.wait_with_output().expect("wait for hook process");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "checkpoint via real dispatch must exit 0: stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.trim().is_empty(),
        "checkpoint via real dispatch must not print diagnostics for a \
         well-formed save-file event, got stderr: {stderr}"
    );

    // Assert an ACTUAL checkpoint effect -- exit 0 alone proves nothing
    // (handle_checkpoint exits 0 even on malformed input): the working log
    // must now contain a real checkpoint entry, and after committing, the
    // new line must be attributed to Augment, not left as unattributed
    // human/untracked content.
    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    assert!(
        !checkpoints.is_empty(),
        "expected a real checkpoint entry to be recorded via the real dispatch path"
    );

    let commit = repo
        .stage_all_and_commit("Add world function via real dispatch")
        .expect("commit should succeed");

    let mut file = repo.filename("app.py");
    file.assert_lines_and_blame(crate::lines![
        "def hello():".human(),
        "    pass".human(),
        "def world():".ai(),
        "    pass".ai(),
    ]);
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Should have AI attestations from the real installer-dispatched checkpoint"
    );
}
