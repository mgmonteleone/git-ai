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
use crate::repos::test_repo::TestRepo;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::error::GitAiError;
use serde_json::json;
use std::fs;

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
                "expected main.rs, got {:?}",
                e.file_paths
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
