//! Augment Code preset — supports BOTH the v1 (`auggie`) and v2
//! (`auggie-v2` / cosmos-agent) hook protocols, autodetected per-payload
//! from its shape (CSS-2302). One preset registration serves both CLI
//! generations with zero changes to either auggie or the installer
//! (`mdm/agents/augment.rs`): both write hook commands to the same
//! `~/.augment/settings.json`, and v2's config loader already accepts
//! the shape the installer writes.
//!
//! ## Autodetection
//!
//! Dispatch is purely shape-based, never config/env/flag-based, and is
//! decided by KEY PRESENCE (a JSON `null` counts as absent), never by the
//! discriminator's value type:
//!   - `hook_event_name` present, `hook_type` absent → v1 (see below)
//!   - `hook_type` present, `hook_event_name` absent → v2 (see below)
//!   - both present, or neither present → `PresetError` (never guess)
//!   - once routed, a present-but-non-string discriminator (e.g. a stray
//!     number/object) is a `PresetError` too, not a silent fallback
//!
//! ## v1 (`auggie`) protocol
//!
//! Single-line UTF-8 JSON payload on stdin per
//! <https://docs.augmentcode.com/cli/hooks>. Top-level fields:
//!
//!   - `hook_event_name` — `"PreToolUse"`, `"PostToolUse"`, `"SessionStart"`,
//!     `"SessionEnd"`, `"Stop"`
//!   - `conversation_id` — opaque session identifier (NOT `session_id`)
//!   - `workspace_roots` — array of workspace paths (NOT scalar `cwd`)
//!
//! Per-event additions:
//!   - `PreToolUse`: `tool_name`, `tool_input`
//!   - `PostToolUse`: `tool_name`, `tool_input`, `tool_output`,
//!     `tool_error`, `file_changes[]`
//!   - `Stop`: `agent_stop_cause`
//!
//! Tool naming differs from Claude (kebab-case lowercase):
//!   - `save-file` / `str-replace-editor` → file edit (field: `tool_input.path`)
//!   - `remove-files` → file edit (field: `tool_input.file_paths`, plural array)
//!   - `apply_patch` → file edit (field: `tool_input.input`, a patch-format
//!     string with `*** Add/Update/Delete File: <path>` markers; observed
//!     live against auggie CLI 0.36.0, which uses this tool for whole-file
//!     creates instead of `save-file`)
//!   - `launch-process` → bash (field: `tool_input.command`)
//!
//! Notably **absent** from the v1 payload (verified against the public
//! docs): `transcript_path`, scalar `cwd` (`workspace_roots` array is used
//! instead), and `session_id` (`conversation_id` is used instead).
//!
//! Path resolution: `tool_input.path` is workspace-relative per the docs'
//! jq examples; resolve against `workspace_roots[0]` to get an absolute
//! path. Already-absolute paths are passed through unchanged.
//!
//! **Events not handled:** `SessionStart`, `SessionEnd`, `Stop`. The
//! installer subscribes only to `PreToolUse` and `PostToolUse` (see
//! `mdm/agents/augment.rs::AUGMENT_HOOK_EVENTS`), so these other events
//! should not normally reach this preset. We defensively reject any
//! event other than `PreToolUse`/`PostToolUse` with an explicit
//! `PresetError`.
//!
//! Transcript reading: not implemented for v1. Augment exposes
//! `conversation.agentCodeResponse[]` only on `Stop` when
//! `metadata.includeConversationData` is enabled, but no on-disk
//! transcript file is documented. `stream_source` is therefore set
//! to `None`. Hook-based attribution still works without it.
//!
//! ## v2 (`auggie-v2` / cosmos-agent) protocol — MVP (CSS-2302)
//!
//! Confirmed live against the real `auggie-v2` binary (see
//! `V2FeasibilityAssessment` in the CSS-2302 run). Payload shape
//! (`packages/pi-extensions/hooks/types.ts` upstream):
//!
//!   - `hook_type` — `"PreToolUse"`, `"PostToolUse"`, `"SessionStart"`,
//!     `"SessionEnd"`, `"Stop"`, `"Notification"`, `"PromptSubmit"`
//!   - `tool_name` — generic Claude-Code-style names: `read`, `bash`,
//!     `edit`, `write` (NOT Augment's v1 kebab-case names)
//!   - `tool_input` — `{"path": ..., "content": ...}` (`write`),
//!     `{"path": ..., "edits": [...]}` (`edit`), `{"command": ...}` (`bash`)
//!   - `tool_result` / `tool_is_error` — PostToolUse only (v1's equivalent
//!     fields are named `tool_output`/`tool_error`)
//!
//! **Absent from the v2 JSON** (verified at the type level and live):
//! `conversation_id`, `workspace_roots`, `context.modelName`, and any
//! `file_changes[]` array — but `tool_input.path` is present on both
//! Pre/PostToolUse, so the existing `extract_augment_file_paths` helper
//! (shared with v1, which already handles `path` / `file_paths[]` /
//! `apply_patch` patch text) already covers file resolution — including,
//! for free, the hypothetical case of a future v2 extension sending the
//! `apply_patch` tool name (classify_tool's `Agent::Augment` arm treats it
//! as `FileEdit` for both shapes; v2's confirmed default toolset does not
//! include it today).
//!
//! Everything the v1 preset needs is either present-but-renamed, or
//! independently derivable without any cosmos-agent change:
//!   - **Workspace root**: v2's hook subprocess inherits the already
//!     `chdir`'d CLI process cwd, so `std::env::current_dir()` recovers it
//!     for free (confirmed live) — simpler than v1's JSON field read.
//!   - **Session id / model (MVP tier)**: best-effort mining of the
//!     on-disk `~/.augment/sessions-v2/--<cwd-slug>--/*.jsonl` session
//!     file (header `id` for the session id, tail-scanned
//!     `model_change`/message `model` for the live model), but ONLY when
//!     exactly one `*.jsonl` file exists for the workspace. The v2 hook
//!     payload carries no session identifier (see above), so if more
//!     than one session file is present — e.g. two concurrently active
//!     `auggie-v2` sessions in the same workspace, or leftover history —
//!     there is no way to tell which file belongs to the *current*
//!     invocation. Picking the newest-mtime file in that case would risk
//!     silently attributing this hook to a DIFFERENT, concurrently
//!     active session (wrong `external_session_id` and model), so mining
//!     fails closed instead of guessing whenever the candidate file is
//!     ambiguous. This is undocumented internal file format, not a
//!     contract — mining is strictly best-effort, bounded (sole file
//!     only, tail-only read, never an unbounded scan), and NEVER blocks
//!     or slows the parse path: any failure or ambiguity degrades to a
//!     stable `generate_session_id(cwd, "augment")` hash and
//!     `model: "unknown"`, mirroring the v1 preset's own
//!     missing-metadata fallback.
//!   - **Tool classification**: v2's default toolset names happen to
//!     coincide with Claude's (`write`/`edit`/`bash`) — added directly to
//!     the existing `Agent::Augment` arm in `classify_tool` (no v1/v2
//!     name collisions), not a documented guarantee (a future release or
//!     an enabled extension could rename tools invisibly to git-ai).
//!
//! **Events not handled:** `SessionStart`, `SessionEnd`, `Stop`,
//! `Notification`, `PromptSubmit` — rejected with `PresetError`, same
//! fail-closed policy as v1 (never fabricate a checkpoint for a
//! lifecycle event).

use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct AugmentPreset;

fn extract_augment_file_paths(data: &serde_json::Value, workspace_root: &str) -> Vec<PathBuf> {
    let tool_input = match data.get("tool_input") {
        Some(ti) => ti,
        None => return vec![],
    };

    // `remove-files` sends `file_paths` as an array.
    if let Some(arr) = tool_input.get("file_paths").and_then(|v| v.as_array()) {
        let paths: Vec<PathBuf> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|p| parse::resolve_absolute(p, workspace_root))
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }

    // `save-file` and `str-replace-editor` send `path`.
    if let Some(path) = tool_input.get("path").and_then(|v| v.as_str())
        && !path.is_empty()
    {
        return vec![parse::resolve_absolute(path, workspace_root)];
    }

    // `apply_patch` sends the whole patch as `input`; the file path(s) are
    // embedded in `*** Add/Update/Delete File: <path>` marker lines.
    if let Some(patch) = tool_input.get("input").and_then(|v| v.as_str()) {
        let mut raw_paths: Vec<String> = Vec::new();
        parse::collect_apply_patch_paths_from_text(patch, &mut raw_paths);
        let paths: Vec<PathBuf> = raw_paths
            .iter()
            .map(|p| parse::resolve_absolute(p, workspace_root))
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }

    vec![]
}

impl AgentPreset for AugmentPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        // Autodetect the protocol generation from payload shape alone
        // (never from config/env/flags) so one preset serves both `auggie`
        // (v1) and `auggie-v2` (v2) with zero installer changes.
        //
        // Detection is by KEY PRESENCE, not by value type (CSS-2302): a
        // payload carrying both `hook_event_name` and `hook_type` is
        // ambiguous regardless of whether one of them holds a non-string
        // value (e.g. a stray `"hook_event_name":123`). Silently treating a
        // type-confused discriminator as "absent" would let such a payload
        // slip past the ambiguous-shape rejection and misroute to a single
        // generation instead of failing closed. A JSON `null` is the one
        // exception: it is the conventional way to say "not provided" and
        // is treated as absent, same as the key being missing entirely.
        // Once routed, parse_v1/parse_v2 separately validate that their
        // discriminator is actually a string (not just present) and return
        // a clear PresetError otherwise -- never silence.
        let has_v1_shape = data.get("hook_event_name").is_some_and(|v| !v.is_null());
        let has_v2_shape = data.get("hook_type").is_some_and(|v| !v.is_null());

        match (has_v1_shape, has_v2_shape) {
            (true, false) => parse_v1(&data, trace_id),
            (false, true) => parse_v2(&data, trace_id),
            (true, true) => Err(GitAiError::PresetError(
                "Ambiguous Augment hook_input: both hook_event_name (v1) and hook_type (v2) present"
                    .to_string(),
            )),
            (false, false) => Err(GitAiError::PresetError(
                "Unrecognized Augment hook_input: neither hook_event_name (v1) nor hook_type (v2) present"
                    .to_string(),
            )),
        }
    }
}

/// Human-readable JSON value type name for actionable type-mismatch errors
/// (e.g. "a stray discriminator must be a string, got number") (CSS-2302).
fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Parses the v1 (`auggie`) hook payload shape. See the module docs for the
/// full schema.
fn parse_v1(data: &serde_json::Value, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    {
        // `hook_event_name` is known to be present (non-null) here -- that's
        // what routed us to parse_v1 -- but presence alone doesn't mean it's
        // a string. Validate the type explicitly so a type-confused value
        // (e.g. a stray number/object) fails closed with an actionable
        // error instead of silently falling through as "unsupported event"
        // (CSS-2302).
        if let Some(v) = data.get("hook_event_name")
            && v.as_str().is_none()
        {
            return Err(GitAiError::PresetError(format!(
                "Augment hook_event_name must be a string, got {}",
                json_type_name(v)
            )));
        }

        let conversation_id = parse::required_str(data, "conversation_id")?.to_string();

        // workspace_roots is required; use the first entry as the canonical cwd.
        let workspace_root = data
            .get("workspace_roots")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                GitAiError::PresetError(
                    "workspace_roots[0] not found in Augment hook_input".to_string(),
                )
            })?
            .to_string();

        let tool_name = parse::optional_str(data, "tool_name");
        let hook_event = parse::optional_str(data, "hook_event_name");
        // Augment does not document a per-tool-call id field, so default
        // to "bash" for bash tools (matching the Claude/Codex pattern)
        // and "unknown" for file-edit events.
        let tool_class = tool_name
            .map(|n| bash_tool::classify_tool(Agent::Augment, n))
            .unwrap_or(ToolClass::Skip);
        let is_bash = tool_class == ToolClass::Bash;
        let is_file_edit = tool_class == ToolClass::FileEdit;

        // Augment exposes `context.modelName` only when
        // `metadata.includeUserContext: true` is set in the hook config.
        // Default to "unknown" when absent rather than reading agent
        // config files.
        let model = data
            .get("context")
            .and_then(|c| c.get("modelName"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string();

        let context = PresetContext {
            agent_id: AgentId {
                tool: "augment".to_string(),
                id: conversation_id.clone(),
                model,
            },
            external_session_id: conversation_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(&workspace_root),
            metadata: HashMap::new(),
        };

        // Mirror the codex/kimi-code style: explicit error on unknown events
        // so we never fabricate spurious file-edit checkpoints when Augment
        // fires SessionStart/SessionEnd/Stop.
        let event = match hook_event {
            Some("PreToolUse") => {
                if is_bash {
                    ParsedHookEvent::PreBashCall(PreBashCall {
                        context,
                        tool_use_id: "bash".to_string(),
                        command: parse::bash_command_from_hook_input(data),
                    })
                } else if is_file_edit {
                    ParsedHookEvent::PreFileEdit(PreFileEdit {
                        context,
                        file_paths: extract_augment_file_paths(data, &workspace_root),
                        dirty_files: None,
                        tool_use_id: None,
                    })
                } else {
                    return Err(GitAiError::PresetError(format!(
                        "Skipping Augment PreToolUse for unsupported tool {}",
                        tool_name.unwrap_or("unknown")
                    )));
                }
            }
            Some("PostToolUse") => {
                if is_bash {
                    ParsedHookEvent::PostBashCall(PostBashCall {
                        context,
                        tool_use_id: "bash".to_string(),
                        command: parse::bash_command_from_hook_input(data),
                        // Transcript reader for Augment's conversation
                        // history is not yet implemented; setting None
                        // avoids feeding an unsupported format to the
                        // existing readers.
                        stream_source: None,
                    })
                } else if is_file_edit {
                    // Prefer file_changes[].path on PostToolUse (present
                    // when Augment captured the actual mutation) over
                    // tool_input.path. Falling back to tool_input keeps
                    // us correct when file_changes is absent.
                    let post_paths = extract_post_file_paths(data, &workspace_root);
                    let file_paths = if post_paths.is_empty() {
                        extract_augment_file_paths(data, &workspace_root)
                    } else {
                        post_paths
                    };
                    ParsedHookEvent::PostFileEdit(PostFileEdit {
                        context,
                        file_paths,
                        dirty_files: None,
                        stream_source: None,
                        tool_use_id: None,
                    })
                } else {
                    return Err(GitAiError::PresetError(format!(
                        "Skipping Augment PostToolUse for unsupported tool {}",
                        tool_name.unwrap_or("unknown")
                    )));
                }
            }
            _ => {
                return Err(GitAiError::PresetError(format!(
                    "Unsupported Augment hook_event_name: {}",
                    hook_event.unwrap_or("<missing>")
                )));
            }
        };

        Ok(vec![event])
    }
}

fn extract_post_file_paths(data: &serde_json::Value, workspace_root: &str) -> Vec<PathBuf> {
    data.get("file_changes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|change| change.get("path").and_then(|p| p.as_str()))
                .filter(|s| !s.is_empty())
                .map(|p| parse::resolve_absolute(p, workspace_root))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// v2 (`auggie-v2` / cosmos-agent) parsing
// ---------------------------------------------------------------------------

/// Parses the v2 (`auggie-v2` / cosmos-agent) hook payload shape. See the
/// module docs for the full schema and the MVP session/model derivation
/// strategy.
fn parse_v2(data: &serde_json::Value, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    // `hook_type` is known to be present (non-null) here -- that's what
    // routed us to parse_v2 -- but presence alone doesn't mean it's a
    // string. Validate the type explicitly so a type-confused value (e.g. a
    // stray number/object) fails closed with an actionable error instead of
    // silently falling through as "unsupported hook_type" (CSS-2302).
    if let Some(v) = data.get("hook_type")
        && v.as_str().is_none()
    {
        return Err(GitAiError::PresetError(format!(
            "Augment hook_type must be a string, got {}",
            json_type_name(v)
        )));
    }

    let hook_type = parse::optional_str(data, "hook_type");
    let tool_name = parse::optional_str(data, "tool_name");

    // v2 never sends workspace_roots; the hook subprocess's own cwd IS the
    // workspace root (auggie-v2 chdirs there before running tools).
    let workspace_root_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let workspace_root = workspace_root_path.to_string_lossy().to_string();

    // v2's default toolset names coincide with Claude's; both are matched
    // by the shared Agent::Augment arm in classify_tool.
    let tool_class = tool_name
        .map(|n| bash_tool::classify_tool(Agent::Augment, n))
        .unwrap_or(ToolClass::Skip);
    let is_bash = tool_class == ToolClass::Bash;
    let is_file_edit = tool_class == ToolClass::FileEdit;

    // MVP session id / model: best-effort session-file mining with a
    // graceful, immediate degrade to a stable cwd-derived hash + "unknown"
    // model on any miss. Mining never blocks or slows the parse path.
    let mined = mine_v2_session_info(&workspace_root_path);
    let session_id = mined
        .session_id
        .unwrap_or_else(|| generate_session_id(&workspace_root, "augment"));
    let model = mined.model.unwrap_or_else(|| "unknown".to_string());

    let context = PresetContext {
        agent_id: AgentId {
            tool: "augment".to_string(),
            id: session_id.clone(),
            model,
        },
        external_session_id: session_id,
        trace_id: trace_id.to_string(),
        cwd: workspace_root_path,
        metadata: HashMap::new(),
    };

    // Same fail-closed policy as v1: explicit error on unknown/lifecycle
    // events and unsupported tools, never a fabricated checkpoint.
    let event = match hook_type {
        Some("PreToolUse") => {
            if is_bash {
                ParsedHookEvent::PreBashCall(PreBashCall {
                    context,
                    tool_use_id: "bash".to_string(),
                    command: parse::bash_command_from_hook_input(data),
                })
            } else if is_file_edit {
                ParsedHookEvent::PreFileEdit(PreFileEdit {
                    context,
                    // v2 has no file_changes[]; tool_input.path is present
                    // on Pre/PostToolUse for both `write` and `edit`. Reuse
                    // the v1 extractor (superset: path / file_paths[] /
                    // apply_patch text) rather than the narrower cross-
                    // preset helper, so that IF a future v2 extension ever
                    // sends the classify_tool-shared "apply_patch" tool
                    // name, its patch-text path is still resolved instead
                    // of silently yielding zero file paths.
                    file_paths: extract_augment_file_paths(data, &workspace_root),
                    dirty_files: None,
                    tool_use_id: None,
                })
            } else {
                return Err(GitAiError::PresetError(format!(
                    "Skipping Augment v2 PreToolUse for unsupported tool {}",
                    tool_name.unwrap_or("unknown")
                )));
            }
        }
        Some("PostToolUse") => {
            if is_bash {
                ParsedHookEvent::PostBashCall(PostBashCall {
                    context,
                    tool_use_id: "bash".to_string(),
                    command: parse::bash_command_from_hook_input(data),
                    // No documented v2 transcript file; same as v1.
                    stream_source: None,
                })
            } else if is_file_edit {
                ParsedHookEvent::PostFileEdit(PostFileEdit {
                    context,
                    // Same extractor as PreToolUse above (superset of the
                    // generic cross-preset helper); v2's real toolset has
                    // no file_changes[], so tool_input.path/apply_patch
                    // text is the only source on Post as well as Pre.
                    file_paths: extract_augment_file_paths(data, &workspace_root),
                    dirty_files: None,
                    stream_source: None,
                    tool_use_id: None,
                })
            } else {
                return Err(GitAiError::PresetError(format!(
                    "Skipping Augment v2 PostToolUse for unsupported tool {}",
                    tool_name.unwrap_or("unknown")
                )));
            }
        }
        _ => {
            return Err(GitAiError::PresetError(format!(
                "Unsupported Augment v2 hook_type: {}",
                hook_type.unwrap_or("<missing>")
            )));
        }
    };

    Ok(vec![event])
}

/// Best-effort result of mining the on-disk v2 session file. Either field
/// may be `None` on any miss (dir/file not found, unreadable, malformed,
/// etc.) — callers must degrade gracefully, never propagate an error.
#[derive(Debug, Default, PartialEq, Eq)]
struct MinedV2SessionInfo {
    session_id: Option<String>,
    model: Option<String>,
}

/// Bound on how much of a session file's tail we'll read to look for the
/// most recent model info. Keeps the cost O(1) regardless of session
/// length — never an unbounded scan.
const V2_SESSION_TAIL_BYTES: u64 = 65536;

/// Slugifies a workspace root the same way cosmos-agent's session manager
/// does for its `sessions-v2/--<slug>--/` directory naming: strip the
/// leading path separator, then replace remaining separators with `-`.
/// Confirmed live against this VM's own `~/.augment/sessions-v2/` layout
/// (see `V2FeasibilityAssessment`). Undocumented internal format, MVP-only.
fn v2_workspace_slug(cwd: &str) -> String {
    cwd.trim_start_matches(['/', '\\'])
        .replace(['/', '\\'], "-")
}

/// Root directory v2 stores its agent state under, respecting the
/// `AUGMENT_CACHE_DIR` env var override (the only override visible to a
/// hook subprocess — a CLI-flag-only override is not, per the feasibility
/// assessment). Falls back to `~/.augment`.
fn v2_agent_dir() -> PathBuf {
    std::env::var("AUGMENT_CACHE_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::mdm::utils::home_dir().join(".augment"))
}

/// Hard cap on directory entries `sole_jsonl_in_dir` will examine per
/// call. A workspace's `sessions-v2/--<slug>--/` directory accumulates one
/// file per historical session and is never pruned, so an unbounded
/// `read_dir` pass would make cost grow with a workspace's entire session
/// history instead of staying O(1) per hook invocation. 512 comfortably
/// covers realistic day-to-day session counts for a single workspace
/// while still bounding worst-case work to a few hundred directory
/// entries; if a directory has more entries than this, mining bails out
/// (see below) rather than scan it — we cannot otherwise be sure no
/// second `*.jsonl` file exists further down the (unordered) listing.
const V2_SESSION_DIR_SCAN_CAP: usize = 512;

/// Returns the sole `*.jsonl` file directly inside `dir`, or `None` if
/// the directory is missing/empty/unreadable, holds more than
/// `V2_SESSION_DIR_SCAN_CAP` entries, or contains more than one `*.jsonl`
/// file. Single, non-recursive directory listing, bounded to constant
/// work regardless of how many historical session files have accumulated
/// for a workspace.
///
/// Deliberately does NOT fall back to "pick the newest by mtime" when
/// more than one candidate exists: the v2 hook payload carries no session
/// identifier, so with two-or-more session files present (e.g. a second,
/// concurrently active `auggie-v2` session in the same workspace) there
/// is no defensible way to tell which one belongs to the *current*
/// invocation. Picking by mtime risks silently attributing this hook to
/// a DIFFERENT session's `external_session_id`/model. Callers already
/// degrade gracefully on `None` (regenerated session id + "unknown"
/// model), so failing closed on ambiguity (or on a too-large directory,
/// where uniqueness can't be confirmed within the scan cap) is the safe
/// choice.
fn sole_jsonl_in_dir(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut found: Option<PathBuf> = None;
    for (count, entry) in entries.enumerate() {
        if count >= V2_SESSION_DIR_SCAN_CAP {
            // Directory has more entries than we're willing to scan;
            // bail out rather than let cost scale with session history.
            return None;
        }
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if found.is_some() {
            // A second candidate makes the choice ambiguous -- refuse to
            // guess which session file belongs to this invocation.
            return None;
        }
        found = Some(path);
    }
    found
}

/// Reads at most the last `max_bytes` of `path`, lossily decoded as UTF-8.
/// Returns `(tail, truncated)` where `truncated` indicates the read did
/// not start at byte 0 (so the first returned line may be a partial line
/// and should be discarded by the caller).
fn read_tail(path: &Path, max_bytes: u64) -> Option<(String, bool)> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some((String::from_utf8_lossy(&buf).into_owned(), start > 0))
}

/// Scans a tail-read chunk backwards (most-recent-first) for the live
/// model name, matching either a `model_change` event's `modelId` or a
/// `message` entry's `model` field.
fn extract_model_from_tail(tail: &str, truncated: bool) -> Option<String> {
    let mut lines: Vec<&str> = tail.lines().collect();
    if truncated && !lines.is_empty() {
        // The first line of a tail-seeked read may be a partial line cut
        // mid-JSON; drop it rather than risk a misleading parse.
        lines.remove(0);
    }

    for line in lines.iter().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let entry_type = value.get("type").and_then(|t| t.as_str());
        if entry_type == Some("model_change")
            && let Some(model_id) = value.get("modelId").and_then(|m| m.as_str())
            && !model_id.is_empty()
        {
            return Some(model_id.to_string());
        }
        if entry_type == Some("message")
            && let Some(model) = value.get("model").and_then(|m| m.as_str())
            && !model.is_empty()
        {
            return Some(model.to_string());
        }
    }
    None
}

/// Best-effort mining of `~/.augment/sessions-v2/--<cwd-slug>--/` for the
/// current workspace's session id and live model name. Never fails loudly:
/// any I/O error, missing directory, unreadable/ambiguous session file, or
/// malformed content simply yields `None` for the affected field. See the
/// module docs for the full rationale and the MVP-vs-full-parity tradeoff.
fn mine_v2_session_info(workspace_root: &Path) -> MinedV2SessionInfo {
    let cwd_str = workspace_root.to_string_lossy();
    let session_dir = v2_agent_dir()
        .join("sessions-v2")
        .join(format!("--{}--", v2_workspace_slug(&cwd_str)));

    // Only mine when exactly one session file exists for this workspace --
    // with two or more candidates (e.g. a concurrently active second
    // session) there is no way to tell which one this hook belongs to, so
    // fail closed to the caller's safe fallback rather than guess.
    let Some(session_file) = sole_jsonl_in_dir(&session_dir) else {
        return MinedV2SessionInfo::default();
    };

    // Bounded: read only the first line for the session header's `id`.
    let session_id = fs::File::open(&session_file).ok().and_then(|f| {
        let mut lines = BufReader::new(f).lines();
        let first_line = lines.next()?.ok()?;
        let value: serde_json::Value = serde_json::from_str(&first_line).ok()?;
        if value.get("type").and_then(|t| t.as_str()) != Some("session") {
            return None;
        }
        value
            .get("id")
            .and_then(|id| id.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    });

    // Bounded: read only the tail for the most recent model info.
    let model = read_tail(&session_file, V2_SESSION_TAIL_BYTES)
        .and_then(|(tail, truncated)| extract_model_from_tail(&tail, truncated));

    MinedV2SessionInfo { session_id, model }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::checkpoint_agent::presets::*;
    use serde_json::json;
    use serial_test::serial;
    use tempfile::TempDir;

    fn make_hook_input(event: &str, tool: &str, tool_input: serde_json::Value) -> String {
        json!({
            "hook_event_name": event,
            "conversation_id": "conv-xyz789",
            "workspace_roots": ["/Users/me/project"],
            "tool_name": tool,
            "tool_input": tool_input,
        })
        .to_string()
    }

    #[test]
    fn test_augment_pre_file_edit_save_file() {
        let input = make_hook_input(
            "PreToolUse",
            "save-file",
            json!({"path": "src/main.rs", "content": "fn main() {}"}),
        );
        let events = AugmentPreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "augment");
                assert_eq!(e.context.agent_id.id, "conv-xyz789");
                assert_eq!(e.context.agent_id.model, "unknown");
                assert_eq!(e.context.external_session_id, "conv-xyz789");
                assert_eq!(e.context.trace_id, "t_test123456789a");
                assert_eq!(e.context.cwd, PathBuf::from("/Users/me/project"));
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/Users/me/project/src/main.rs")]
                );
                assert!(e.dirty_files.is_none());
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_augment_post_file_edit_str_replace_editor() {
        let input = make_hook_input(
            "PostToolUse",
            "str-replace-editor",
            json!({
                "path": "src/lib.rs",
                "command": "str_replace",
                "old_str_1": "a",
                "new_str_1": "b",
            }),
        );
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "augment");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/Users/me/project/src/lib.rs")]
                );
                assert!(
                    e.stream_source.is_none(),
                    "Transcript reader not yet implemented; should be None"
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_augment_post_file_edit_uses_file_changes_when_present() {
        // Augment's PostToolUse can include file_changes[] which is the
        // authoritative list of mutated files. Prefer it over tool_input.
        let input = json!({
            "hook_event_name": "PostToolUse",
            "conversation_id": "conv-1",
            "workspace_roots": ["/Users/me/project"],
            "tool_name": "save-file",
            "tool_input": {"path": "src/old.rs"},
            "file_changes": [
                {"path": "src/new.rs", "changeType": "create"},
                {"path": "src/also.rs", "changeType": "modify"},
            ],
        })
        .to_string();
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![
                        PathBuf::from("/Users/me/project/src/new.rs"),
                        PathBuf::from("/Users/me/project/src/also.rs"),
                    ]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_augment_remove_files_uses_file_paths_array() {
        // remove-files uses `file_paths` (plural array), not `path`.
        let input = make_hook_input(
            "PostToolUse",
            "remove-files",
            json!({"file_paths": ["src/dead.rs", "src/old.rs"]}),
        );
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![
                        PathBuf::from("/Users/me/project/src/dead.rs"),
                        PathBuf::from("/Users/me/project/src/old.rs"),
                    ]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_augment_apply_patch_pre_tool_use_extracts_path_from_patch_text() {
        // Observed live against auggie CLI 0.36.0: `apply_patch` sends the
        // whole patch as `tool_input.input`, with no `path`/`file_paths`
        // field. PreToolUse must still resolve the target file so it can be
        // checkpointed before the edit lands.
        let input = make_hook_input(
            "PreToolUse",
            "apply_patch",
            json!({"input": "*** Begin Patch\n*** Add File: hello.py\n+def greet(name):\n+    return f'Hello, {name}!'\n*** End Patch"}),
        );
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/Users/me/project/hello.py")]
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_augment_apply_patch_post_tool_use_prefers_file_changes() {
        // PostToolUse carries `file_changes[].path` (authoritative) in
        // addition to the raw patch text; file_changes must win.
        let input = json!({
            "hook_event_name": "PostToolUse",
            "conversation_id": "conv-xyz789",
            "workspace_roots": ["/Users/me/project"],
            "tool_name": "apply_patch",
            "tool_input": {"input": "*** Begin Patch\n*** Add File: greet2.py\n+def add(a, b):\n+    return a + b\n*** End Patch"},
            "file_changes": [
                {"path": "greet2.py", "changeType": "create"},
            ],
        })
        .to_string();
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/Users/me/project/greet2.py")]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_augment_pre_bash_call() {
        let input = make_hook_input(
            "PreToolUse",
            "launch-process",
            json!({"command": "git status"}),
        );
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "augment");
                assert_eq!(e.tool_use_id, "bash");
                assert_eq!(e.command.as_deref(), Some("git status"));
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_augment_post_bash_call() {
        let input = make_hook_input("PostToolUse", "launch-process", json!({"command": "ls"}));
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "augment");
                assert_eq!(e.command.as_deref(), Some("ls"));
                assert!(e.stream_source.is_none());
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_augment_absolute_path_passes_through() {
        let input = make_hook_input(
            "PostToolUse",
            "save-file",
            json!({"path": "/etc/hosts", "content": ""}),
        );
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                // Already absolute — must not be re-rooted under workspace.
                assert_eq!(e.file_paths, vec![PathBuf::from("/etc/hosts")]);
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_augment_unsupported_tool_pretooluse_errors() {
        // Tools like view, grep-search, codebase-retrieval, web-fetch are
        // documented but we don't checkpoint for them — verify they error
        // rather than silently fabricating a checkpoint.
        let input = make_hook_input("PreToolUse", "view", json!({"path": "src/foo.rs"}));
        let result = AugmentPreset.parse(&input, "t_test");
        assert!(result.is_err());
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(
                    msg.contains("PreToolUse for unsupported tool"),
                    "expected unsupported-tool message, got: {}",
                    msg
                );
            }
            _ => panic!("Expected PresetError"),
        }
    }

    #[test]
    fn test_augment_unsupported_tool_posttooluse_errors() {
        let input = make_hook_input("PostToolUse", "web-fetch", json!({"url": "https://x"}));
        let result = AugmentPreset.parse(&input, "t_test");
        assert!(result.is_err());
    }

    #[test]
    fn test_augment_lifecycle_event_errors() {
        // SessionStart / SessionEnd / Stop must not silently fall through
        // to PostFileEdit.
        for event in ["SessionStart", "SessionEnd", "Stop"] {
            let input = json!({
                "hook_event_name": event,
                "conversation_id": "conv-1",
                "workspace_roots": ["/Users/me/project"],
            })
            .to_string();
            let result = AugmentPreset.parse(&input, "t_test");
            assert!(result.is_err(), "expected error for {event}");
            match result {
                Err(GitAiError::PresetError(msg)) => {
                    assert!(
                        msg.contains("Unsupported Augment hook_event_name"),
                        "expected unknown-event message for {event}, got: {}",
                        msg
                    );
                }
                _ => panic!("Expected PresetError for {event}"),
            }
        }
    }

    #[test]
    fn test_augment_missing_event_name_errors() {
        let input = json!({
            "conversation_id": "conv-1",
            "workspace_roots": ["/Users/me/project"],
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        assert!(result.is_err());
    }

    #[test]
    fn test_augment_invalid_json_errors() {
        let result = AugmentPreset.parse("not valid json", "t_test");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid JSON in hook_input")
        );
    }

    #[test]
    fn test_augment_missing_conversation_id_errors() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "workspace_roots": ["/Users/me/project"],
            "tool_name": "save-file",
            "tool_input": {"path": "x"},
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("conversation_id"));
    }

    #[test]
    fn test_augment_missing_workspace_roots_errors() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "conversation_id": "conv-1",
            "tool_name": "save-file",
            "tool_input": {"path": "x"},
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("workspace_roots"));
    }

    #[test]
    fn test_augment_extracts_model_when_context_present() {
        // Augment exposes context.modelName only with metadata.includeUserContext.
        let input = json!({
            "hook_event_name": "PostToolUse",
            "conversation_id": "conv-1",
            "workspace_roots": ["/Users/me/project"],
            "tool_name": "save-file",
            "tool_input": {"path": "src/main.rs"},
            "context": {"modelName": "claude-sonnet-4-5"},
        })
        .to_string();
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.model, "claude-sonnet-4-5");
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    // ========================================================================
    // v2 (`auggie-v2` / cosmos-agent) autodetection + parsing tests
    //
    // Fixtures below marked "[live]" are the exact payloads captured by
    // running the real auggie-v2 binary, per V2FeasibilityAssessment
    // (assessment.md §2) in the CSS-2302 run.
    // ========================================================================

    #[test]
    fn test_augment_autodetects_v2_shape_from_hook_type() {
        // [live] real captured PostToolUse write payload.
        let input = r#"{"hook_type":"PostToolUse","tool_name":"write","tool_input":{"path":"/tmp/proj/bar2.txt","content":"test2"},"tool_result":[{"type":"text","text":"Successfully wrote 5 bytes to bar2.txt"}],"tool_is_error":false}"#;
        let events = AugmentPreset.parse(input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "augment");
                assert_eq!(e.file_paths, vec![PathBuf::from("/tmp/proj/bar2.txt")]);
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_augment_autodetects_v1_shape_from_hook_event_name() {
        // v1 shape must still dispatch to parse_v1 even though v2 support
        // now exists in the same preset.
        let input = make_hook_input("PostToolUse", "save-file", json!({"path": "src/main.rs"}));
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.id, "conv-xyz789");
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_augment_ambiguous_shape_both_fields_present_errors() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "hook_type": "PostToolUse",
            "conversation_id": "conv-1",
            "workspace_roots": ["/tmp/proj"],
            "tool_name": "write",
            "tool_input": {"path": "x"},
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        assert!(result.is_err());
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(msg.contains("Ambiguous"), "got: {}", msg);
            }
            _ => panic!("Expected PresetError"),
        }
    }

    #[test]
    fn test_augment_neither_shape_field_present_errors() {
        let input = json!({"tool_name": "write", "tool_input": {"path": "x"}}).to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        assert!(result.is_err());
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(msg.contains("Unrecognized"), "got: {}", msg);
            }
            _ => panic!("Expected PresetError"),
        }
    }

    #[test]
    fn test_augment_v2_pre_write_extracts_path() {
        // [live] real captured PreToolUse write payload (path made absolute
        // for a deterministic assertion independent of the test process's
        // actual cwd).
        let input = r#"{"hook_type":"PreToolUse","tool_name":"write","tool_input":{"path":"/tmp/proj/bar2.txt","content":"test2"}}"#;
        let events = AugmentPreset.parse(input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "augment");
                assert_eq!(e.context.trace_id, "t_test123456789a");
                assert_eq!(e.file_paths, vec![PathBuf::from("/tmp/proj/bar2.txt")]);
                assert!(e.dirty_files.is_none());
                // No mining fixture is set up for this test's cwd, so the
                // MVP fallback path must produce a stable, non-empty
                // session id and default model.
                assert!(!e.context.agent_id.id.is_empty());
                assert_eq!(e.context.agent_id.model, "unknown");
                assert!(!e.context.cwd.as_os_str().is_empty());
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_augment_v2_post_edit_extracts_path() {
        // [live] real captured PostToolUse edit payload (tool_is_error:
        // true in the original capture -- still checkpointed the same as
        // a successful edit; actual attribution is diff-based, so a
        // failed edit that changed nothing produces no spurious lines).
        let input = r#"{"hook_type":"PostToolUse","tool_name":"edit","tool_input":{"path":"/tmp/proj/bar2.txt","edits":[{"oldText":"a","newText":"b"}]},"tool_result":[{"type":"text","text":"error"}],"tool_is_error":true}"#;
        let events = AugmentPreset.parse(input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.file_paths, vec![PathBuf::from("/tmp/proj/bar2.txt")]);
                assert!(e.stream_source.is_none());
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_augment_v2_apply_patch_hypothetical_extracts_path_from_patch_text() {
        // Not part of v2's confirmed default toolset (read/bash/edit/write)
        // -- but classify_tool's shared Agent::Augment arm would still
        // route a hypothetical future "apply_patch" tool_name to FileEdit
        // on the v2 path, and tool_input there has no "path", only a raw
        // patch-format "input" string (v1's apply_patch shape). Guards
        // against silently yielding zero file paths in that case.
        let input = json!({
            "hook_type": "PreToolUse",
            "tool_name": "apply_patch",
            "tool_input": {"input": "*** Begin Patch\n*** Add File: hello.py\n+def greet():\n+    pass\n*** End Patch"},
        })
        .to_string();
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert!(
                    e.file_paths[0].ends_with("hello.py"),
                    "expected hello.py, got {:?}",
                    e.file_paths
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_augment_v2_read_tool_pretooluse_errors() {
        // [live] real captured PreToolUse read payload -- read is
        // intentionally not checkpointed (ToolClass::Skip).
        let input =
            r#"{"hook_type":"PreToolUse","tool_name":"read","tool_input":{"path":"foo.txt"}}"#;
        let result = AugmentPreset.parse(input, "t_test");
        assert!(result.is_err());
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(
                    msg.contains("PreToolUse for unsupported tool"),
                    "got: {}",
                    msg
                );
            }
            _ => panic!("Expected PresetError"),
        }
    }

    #[test]
    fn test_augment_v2_read_tool_posttooluse_errors() {
        // [live] real captured PostToolUse read payload.
        let input = r#"{"hook_type":"PostToolUse","tool_name":"read","tool_input":{"path":"foo.txt"},"tool_result":[{"type":"text","text":"hello world
"}],"tool_is_error":false}"#;
        let result = AugmentPreset.parse(input, "t_test");
        assert!(result.is_err());
    }

    #[test]
    fn test_augment_v2_bash_tool_pre_and_post() {
        let pre = json!({
            "hook_type": "PreToolUse",
            "tool_name": "bash",
            "tool_input": {"command": "git status"},
        })
        .to_string();
        let events = AugmentPreset.parse(&pre, "t_test").unwrap();
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
        let events = AugmentPreset.parse(&post, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.command.as_deref(), Some("git status"));
                assert!(e.stream_source.is_none());
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_augment_v2_lifecycle_events_error() {
        // [live] real captured minimal lifecycle payloads -- must not
        // fall through to a fabricated checkpoint.
        for payload in [
            r#"{"hook_type":"SessionStart"}"#,
            r#"{"hook_type":"SessionEnd"}"#,
            r#"{"hook_type":"Stop"}"#,
            r#"{"hook_type":"Notification","notification_type":"info","notification_message":"hi"}"#,
            r#"{"hook_type":"PromptSubmit","user_prompt":"hello"}"#,
        ] {
            let result = AugmentPreset.parse(payload, "t_test");
            assert!(result.is_err(), "expected error for {payload}, got Ok");
            match result {
                Err(GitAiError::PresetError(msg)) => {
                    assert!(
                        msg.contains("Unsupported Augment v2 hook_type"),
                        "got: {}",
                        msg
                    );
                }
                _ => panic!("Expected PresetError"),
            }
        }
    }

    #[test]
    fn test_augment_v2_malformed_json_errors() {
        let result = AugmentPreset.parse("not valid json", "t_test");
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // v2 session-file mining (best-effort, bounded)
    // ------------------------------------------------------------------

    #[test]
    fn test_v2_workspace_slug_strips_leading_slash_and_replaces_separators() {
        assert_eq!(
            v2_workspace_slug("/tmp/hooktest/workspace"),
            "tmp-hooktest-workspace"
        );
        assert_eq!(v2_workspace_slug("/workspace"), "workspace");
    }

    #[test]
    fn test_extract_model_from_tail_prefers_most_recent_model_change() {
        let tail = "{\"type\":\"model_change\",\"modelId\":\"model-a\"}\n{\"type\":\"model_change\",\"modelId\":\"model-b\"}\n";
        assert_eq!(
            extract_model_from_tail(tail, false),
            Some("model-b".to_string())
        );
    }

    #[test]
    fn test_extract_model_from_tail_falls_back_to_message_model() {
        let tail = "{\"type\":\"message\",\"model\":\"prism_tenant_custom\"}\n";
        assert_eq!(
            extract_model_from_tail(tail, false),
            Some("prism_tenant_custom".to_string())
        );
    }

    #[test]
    fn test_extract_model_from_tail_discards_truncated_first_line() {
        // Simulate a tail-seek landing mid-line: the first "line" is a
        // partial fragment that must not be parsed as JSON.
        let tail =
            "\"modelId\":\"garbage\"}\n{\"type\":\"model_change\",\"modelId\":\"model-c\"}\n";
        assert_eq!(
            extract_model_from_tail(tail, true),
            Some("model-c".to_string())
        );
    }

    #[test]
    fn test_extract_model_from_tail_returns_none_when_absent() {
        let tail = "{\"type\":\"session\",\"id\":\"abc\"}\n";
        assert_eq!(extract_model_from_tail(tail, false), None);
    }

    #[test]
    fn test_mine_v2_session_info_missing_dir_returns_defaults() {
        let mined = mine_v2_session_info(Path::new("/nonexistent/workspace/for/this/test"));
        assert_eq!(mined, MinedV2SessionInfo::default());
    }

    #[test]
    #[serial]
    fn test_mine_v2_session_info_reads_header_id_and_tail_model() {
        let cache_dir = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let workspace_path = workspace.path().canonicalize().unwrap();
        let slug = v2_workspace_slug(&workspace_path.to_string_lossy());
        let session_dir = cache_dir
            .path()
            .join(".augment")
            .join("sessions-v2")
            .join(format!("--{}--", slug));
        fs::create_dir_all(&session_dir).unwrap();
        let session_file = session_dir.join("4f2ba0-session.jsonl");
        fs::write(
            &session_file,
            format!(
                "{{\"type\":\"session\",\"id\":\"real-session-id-123\",\"cwd\":\"{}\"}}\n{{\"type\":\"model_change\",\"provider\":\"anthropic\",\"modelId\":\"prism_tenant_custom\"}}\n",
                workspace_path.to_string_lossy()
            ),
        )
        .unwrap();

        unsafe {
            std::env::set_var("AUGMENT_CACHE_DIR", cache_dir.path().join(".augment"));
        }
        let mined = mine_v2_session_info(&workspace_path);
        unsafe {
            std::env::remove_var("AUGMENT_CACHE_DIR");
        }

        assert_eq!(mined.session_id, Some("real-session-id-123".to_string()));
        assert_eq!(mined.model, Some("prism_tenant_custom".to_string()));
    }

    #[test]
    #[serial]
    fn test_mine_v2_session_info_refuses_to_guess_between_competing_same_workspace_sessions() {
        // Regression for discussion_r3942067002: two session files in the
        // same workspace directory simulate a second, concurrently active
        // `auggie-v2` session (or stale leftover history) alongside the
        // "current" one. The v2 hook payload carries no session
        // identifier, so there is no defensible way to know which file
        // belongs to *this* invocation. Picking by newest mtime (the old,
        // pre-fix behavior) would silently attribute this hook to
        // whichever session happened to write most recently -- merging
        // unrelated checkpoints under the wrong `external_session_id`/
        // model. The fix must fail closed to the safe MVP fallback
        // (`None`/`None`, i.e. regenerated id + "unknown" model) instead.
        let cache_dir = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let workspace_path = workspace.path().canonicalize().unwrap();
        let slug = v2_workspace_slug(&workspace_path.to_string_lossy());
        let session_dir = cache_dir
            .path()
            .join(".augment")
            .join("sessions-v2")
            .join(format!("--{}--", slug));
        fs::create_dir_all(&session_dir).unwrap();

        // "other" is the concurrently active session that happens to have
        // the newest mtime -- the exact case that used to win under the
        // old newest-mtime selection.
        let mine = session_dir.join("mine.jsonl");
        fs::write(
            &mine,
            "{\"type\":\"session\",\"id\":\"my-real-session-id\"}\n",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let other = session_dir.join("other.jsonl");
        fs::write(
            &other,
            "{\"type\":\"session\",\"id\":\"someone-elses-session-id\"}\n",
        )
        .unwrap();

        unsafe {
            std::env::set_var("AUGMENT_CACHE_DIR", cache_dir.path().join(".augment"));
        }
        let mined = mine_v2_session_info(&workspace_path);
        unsafe {
            std::env::remove_var("AUGMENT_CACHE_DIR");
        }

        // Must NOT pick either session's id/model -- ambiguous means
        // "unknown", never a guess.
        assert_eq!(mined, MinedV2SessionInfo::default());
    }

    #[test]
    #[serial]
    fn test_augment_v2_falls_back_to_stable_session_id_when_mining_fails() {
        let cache_dir = TempDir::new().unwrap();
        unsafe {
            std::env::set_var("AUGMENT_CACHE_DIR", cache_dir.path().join(".augment"));
        }
        let input = r#"{"hook_type":"PreToolUse","tool_name":"write","tool_input":{"path":"/tmp/proj/x.txt"}}"#;
        let events = AugmentPreset.parse(input, "t_test").unwrap();
        unsafe {
            std::env::remove_var("AUGMENT_CACHE_DIR");
        }

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let expected_id = generate_session_id(&cwd.to_string_lossy(), "augment");
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.id, expected_id);
                assert_eq!(e.context.agent_id.model, "unknown");
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    // ========================================================================
    // Adversarial autodetection edge cases (CSS-2302 verification pass)
    // ========================================================================

    #[test]
    fn test_augment_hook_event_name_null_routes_as_v2() {
        // A `null` hook_event_name is not a string, so `has_v1_shape` must be
        // false -- the payload should route as v2, not be treated as v1-shaped
        // or ambiguous.
        let input = json!({
            "hook_event_name": null,
            "hook_type": "PreToolUse",
            "tool_name": "write",
            "tool_input": {"path": "/tmp/proj/a.rs"},
        })
        .to_string();
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(_) => {}
            _ => panic!("Expected PreFileEdit (v2 route)"),
        }
    }

    // ========================================================================
    // Both-keys type-confusion matrix (CSS-2302 review blocker)
    //
    // Detection is by KEY PRESENCE, not value type: once BOTH
    // `hook_event_name` and `hook_type` are present (non-null), the payload
    // is ambiguous regardless of which one (or both) holds a non-string
    // value. Silently ignoring a type-confused discriminator and treating it
    // as "absent" is exactly the bug this matrix guards against -- it would
    // let a payload carrying both keys slip past the ambiguous-shape
    // rejection and misroute to a single generation instead of failing
    // closed.
    // ========================================================================

    #[test]
    fn test_augment_both_keys_number_and_string_is_ambiguous() {
        // Regression for the exact review repro: a stray non-string
        // hook_event_name alongside a valid hook_type must still be
        // rejected as ambiguous, not misrouted to v2.
        let input = json!({
            "hook_event_name": 123,
            "hook_type": "PostToolUse",
            "conversation_id": "conv-1",
            "workspace_roots": ["/tmp/proj"],
            "tool_name": "write",
            "tool_input": {"path": "src/lib.rs"},
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(msg.contains("Ambiguous"), "got: {}", msg);
            }
            other => panic!("Expected ambiguous-shape PresetError, got: {:?}", other),
        }
    }

    #[test]
    fn test_augment_both_keys_string_and_number_is_ambiguous() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "hook_type": 999,
            "conversation_id": "conv-1",
            "workspace_roots": ["/tmp/proj"],
            "tool_name": "write",
            "tool_input": {"path": "a.rs"},
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(msg.contains("Ambiguous"), "got: {}", msg);
            }
            other => panic!("Expected ambiguous-shape PresetError, got: {:?}", other),
        }
    }

    #[test]
    fn test_augment_both_keys_string_and_object_is_ambiguous() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "hook_type": {"nested": true},
            "conversation_id": "conv-1",
            "workspace_roots": ["/tmp/proj"],
            "tool_name": "save-file",
            "tool_input": {"path": "a.rs"},
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(msg.contains("Ambiguous"), "got: {}", msg);
            }
            other => panic!("Expected ambiguous-shape PresetError, got: {:?}", other),
        }
    }

    #[test]
    fn test_augment_both_keys_object_and_string_is_ambiguous() {
        let input = json!({
            "hook_event_name": {"x": 1},
            "hook_type": "PreToolUse",
            "tool_name": "write",
            "tool_input": {"path": "a.rs"},
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(msg.contains("Ambiguous"), "got: {}", msg);
            }
            other => panic!("Expected ambiguous-shape PresetError, got: {:?}", other),
        }
    }

    #[test]
    fn test_augment_both_keys_bool_and_string_is_ambiguous() {
        let input = json!({
            "hook_event_name": true,
            "hook_type": "PostToolUse",
            "tool_name": "bash",
            "tool_input": {"command": "ls"},
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(msg.contains("Ambiguous"), "got: {}", msg);
            }
            other => panic!("Expected ambiguous-shape PresetError, got: {:?}", other),
        }
    }

    #[test]
    fn test_augment_hook_type_present_wrong_type_single_key_errors_not_silent() {
        // Single-key case: hook_event_name is absent (JSON null), hook_type
        // is present but not a string. This must reach parse_v2's own
        // discriminator-type validation and fail closed with an actionable
        // message -- not silently fall through as "unsupported hook_type".
        let input = json!({
            "hook_event_name": null,
            "hook_type": 42,
            "tool_name": "write",
            "tool_input": {"path": "a.rs"},
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(
                    msg.contains("hook_type") && msg.contains("string"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("Expected a type-confusion PresetError, got: {:?}", other),
        }
    }

    #[test]
    fn test_augment_hook_event_name_present_wrong_type_single_key_errors_not_silent() {
        // Symmetric single-key case: hook_type is absent (JSON null),
        // hook_event_name is present but not a string.
        let input = json!({
            "hook_event_name": 42,
            "hook_type": null,
            "conversation_id": "conv-1",
            "workspace_roots": ["/tmp/proj"],
            "tool_name": "write",
            "tool_input": {"path": "a.rs"},
        })
        .to_string();
        let result = AugmentPreset.parse(&input, "t_test");
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(
                    msg.contains("hook_event_name") && msg.contains("string"),
                    "got: {}",
                    msg
                );
            }
            other => panic!("Expected a type-confusion PresetError, got: {:?}", other),
        }
    }

    // ========================================================================
    // Adversarial v1 apply_patch coverage
    // ========================================================================

    #[test]
    fn test_augment_apply_patch_multi_file_extracts_all_paths() {
        // A single apply_patch call can add/update/delete several files in
        // one patch; all target paths must be extracted (not just the
        // first).
        let patch = "\
*** Begin Patch
*** Add File: new.py
+print('new')
*** Update File: existing.py
@@
-old
+new
*** Delete File: gone.py
*** End Patch";
        let input = make_hook_input("PreToolUse", "apply_patch", json!({"input": patch}));
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![
                        PathBuf::from("/Users/me/project/new.py"),
                        PathBuf::from("/Users/me/project/existing.py"),
                        PathBuf::from("/Users/me/project/gone.py"),
                    ]
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_augment_apply_patch_malformed_text_yields_empty_paths_not_error() {
        // Patch text with none of the recognized `*** ... File:` markers
        // must degrade to an empty file_paths list rather than erroring or
        // panicking -- the tool is still a recognized file-edit tool, so we
        // fail open on path *extraction* only, matching the pattern used by
        // other apply_patch-style presets (Codex/Cursor/Droid).
        let input = make_hook_input(
            "PreToolUse",
            "apply_patch",
            json!({"input": "this is not a patch at all, just prose"}),
        );
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert!(e.file_paths.is_empty());
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_augment_extract_file_paths_precedence_file_paths_array_wins() {
        // If a tool_input ever carries both `file_paths` (array) and `path`
        // (scalar) -- not observed live, but defensively specified -- the
        // array must take precedence, matching the checked order in
        // extract_augment_file_paths.
        let input = make_hook_input(
            "PostToolUse",
            "remove-files",
            json!({"file_paths": ["a.rs", "b.rs"], "path": "c.rs"}),
        );
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![
                        PathBuf::from("/Users/me/project/a.rs"),
                        PathBuf::from("/Users/me/project/b.rs"),
                    ]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_augment_post_file_changes_empty_array_falls_back_to_tool_input() {
        // An empty (but present) file_changes[] array must not win over a
        // usable tool_input.path -- extract_post_file_paths returns empty,
        // and the caller must fall back to extract_augment_file_paths.
        let input = json!({
            "hook_event_name": "PostToolUse",
            "conversation_id": "conv-1",
            "workspace_roots": ["/Users/me/project"],
            "tool_name": "save-file",
            "tool_input": {"path": "src/fallback.rs"},
            "file_changes": [],
        })
        .to_string();
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/Users/me/project/src/fallback.rs")]
                );
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    // ========================================================================
    // Adversarial v2 coverage
    // ========================================================================

    #[test]
    fn test_augment_v2_missing_tool_input_path_yields_empty_file_paths_not_error() {
        // A malformed/partial v2 write payload missing tool_input.path must
        // not error -- it degrades to an empty file_paths list, consistent
        // with the shared file_paths_from_tool_input contract used by other
        // presets (e.g. claude.rs).
        let input = json!({
            "hook_type": "PreToolUse",
            "tool_name": "write",
            "tool_input": {"content": "no path field here"},
        })
        .to_string();
        let events = AugmentPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert!(e.file_paths.is_empty());
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_augment_v2_absolute_path_passes_through_unchanged() {
        // v2's workspace root is env::current_dir(); an already-absolute
        // tool_input.path must not be re-rooted under it.
        let input = r#"{"hook_type":"PreToolUse","tool_name":"write","tool_input":{"path":"/etc/hosts","content":""}}"#;
        let events = AugmentPreset.parse(input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.file_paths, vec![PathBuf::from("/etc/hosts")]);
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_augment_v2_unknown_tool_name_pretooluse_errors_not_skipped_silently() {
        // A v2 tool name outside the known write/edit/bash/read set must
        // fail closed (PresetError), same as v1's unsupported-tool policy --
        // never silently fabricate or silently drop the event.
        let input = r#"{"hook_type":"PreToolUse","tool_name":"totally-unknown-tool","tool_input":{"path":"x"}}"#;
        let result = AugmentPreset.parse(input, "t_test");
        assert!(result.is_err());
        match result {
            Err(GitAiError::PresetError(msg)) => {
                assert!(
                    msg.contains("PreToolUse for unsupported tool"),
                    "got: {}",
                    msg
                );
            }
            _ => panic!("Expected PresetError"),
        }
    }

    // ========================================================================
    // Adversarial v2 session-file mining coverage
    // ========================================================================

    #[test]
    fn test_sole_jsonl_in_dir_empty_dir_returns_none() {
        let dir = TempDir::new().unwrap();
        assert_eq!(sole_jsonl_in_dir(dir.path()), None);
    }

    #[test]
    fn test_sole_jsonl_in_dir_single_file_returns_it() {
        let dir = TempDir::new().unwrap();
        let only = dir.path().join("session.jsonl");
        fs::write(&only, "").unwrap();
        assert_eq!(sole_jsonl_in_dir(dir.path()), Some(only));
    }

    #[test]
    fn test_sole_jsonl_in_dir_multiple_files_returns_none() {
        // Regression for discussion_r3942067002: more than one `*.jsonl`
        // candidate (e.g. a second, concurrently active session in the
        // same workspace) is ambiguous -- the v2 hook payload carries no
        // session identifier to disambiguate them, so mining must refuse
        // to guess (never fall back to "pick the newest by mtime").
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.jsonl"), "").unwrap();
        fs::write(dir.path().join("b.jsonl"), "").unwrap();
        assert_eq!(sole_jsonl_in_dir(dir.path()), None);
    }

    #[test]
    fn test_sole_jsonl_in_dir_bails_out_past_scan_cap() {
        // Proves the directory scan is capped, not proportional to
        // historical session count: with more entries than
        // V2_SESSION_DIR_SCAN_CAP present, mining must give up (None)
        // rather than pay an unbounded read_dir pass, even when there is
        // only one genuine `*.jsonl` candidate among the entries -- past
        // the cap we can no longer be sure a second candidate isn't
        // lurking further down the (unordered) listing, so we bail rather
        // than risk silently trusting a false "sole file" result.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("session.jsonl"), "").unwrap();
        for i in 0..V2_SESSION_DIR_SCAN_CAP {
            fs::write(dir.path().join(format!("other-{i}.txt")), "").unwrap();
        }
        assert_eq!(sole_jsonl_in_dir(dir.path()), None);
    }

    #[test]
    fn test_sole_jsonl_in_dir_scans_normally_at_or_under_cap() {
        // Preserves existing behavior for realistic (small/normal)
        // directories: at or under the cap, the sole `*.jsonl` file is
        // still correctly identified rather than spuriously bailing out.
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("session.jsonl");
        fs::write(&target, "").unwrap();
        for i in 0..(V2_SESSION_DIR_SCAN_CAP - 1) {
            fs::write(dir.path().join(format!("other-{i}.txt")), "").unwrap();
        }
        assert_eq!(sole_jsonl_in_dir(dir.path()), Some(target));
    }

    #[test]
    #[serial]
    fn test_mine_v2_session_info_corrupt_first_line_still_mines_model_from_tail() {
        // A corrupt/non-JSON header line must not abort mining entirely --
        // session_id degrades to None (mirroring the outer caller's
        // stable-hash fallback), but the model can still be recovered from
        // the tail scan.
        let cache_dir = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let workspace_path = workspace.path().canonicalize().unwrap();
        let slug = v2_workspace_slug(&workspace_path.to_string_lossy());
        let session_dir = cache_dir
            .path()
            .join(".augment")
            .join("sessions-v2")
            .join(format!("--{}--", slug));
        fs::create_dir_all(&session_dir).unwrap();
        let session_file = session_dir.join("corrupt-session.jsonl");
        fs::write(
            &session_file,
            "not even json\n{\"type\":\"model_change\",\"modelId\":\"recovered-model\"}\n",
        )
        .unwrap();

        unsafe {
            std::env::set_var("AUGMENT_CACHE_DIR", cache_dir.path().join(".augment"));
        }
        let mined = mine_v2_session_info(&workspace_path);
        unsafe {
            std::env::remove_var("AUGMENT_CACHE_DIR");
        }

        assert_eq!(mined.session_id, None);
        assert_eq!(mined.model, Some("recovered-model".to_string()));
    }

    #[test]
    #[serial]
    fn test_mine_v2_session_info_respects_tail_bound_ignores_head_only_model() {
        // Proves the tail bound is real (not an accidental full-file read):
        // a model_change entry placed only in the file's head, followed by
        // enough padding to push it beyond V2_SESSION_TAIL_BYTES from the
        // end, must NOT be found -- if the implementation regressed to a
        // full-file scan, this would incorrectly return the head model.
        let cache_dir = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let workspace_path = workspace.path().canonicalize().unwrap();
        let slug = v2_workspace_slug(&workspace_path.to_string_lossy());
        let session_dir = cache_dir
            .path()
            .join(".augment")
            .join("sessions-v2")
            .join(format!("--{}--", slug));
        fs::create_dir_all(&session_dir).unwrap();
        let session_file = session_dir.join("oversized-session.jsonl");

        let mut content = String::new();
        content.push_str("{\"type\":\"session\",\"id\":\"big-session\"}\n");
        content.push_str("{\"type\":\"model_change\",\"modelId\":\"should-be-invisible\"}\n");
        // Pad well past the tail bound with lines carrying no model info,
        // so the only model_change entry ends up strictly in the head.
        let filler_line = format!(
            "{{\"type\":\"message\",\"text\":\"{}\"}}\n",
            "x".repeat(200)
        );
        let filler_bytes_needed = (V2_SESSION_TAIL_BYTES as usize) + 8192;
        while content.len() < filler_bytes_needed {
            content.push_str(&filler_line);
        }
        fs::write(&session_file, &content).unwrap();

        unsafe {
            std::env::set_var("AUGMENT_CACHE_DIR", cache_dir.path().join(".augment"));
        }
        let mined = mine_v2_session_info(&workspace_path);
        unsafe {
            std::env::remove_var("AUGMENT_CACHE_DIR");
        }

        // Header id is still readable (only the first line is read for it).
        assert_eq!(mined.session_id, Some("big-session".to_string()));
        // The head-only model_change fell outside the tail window.
        assert_eq!(mined.model, None);
    }
}
