//! Read-only Codex rollout discovery and transcript replay.
//!
//! Safety boundary: this module only opens JSONL files beneath
//! `$CODEX_HOME/{sessions,archived_sessions}`. It never invokes Codex, edits a
//! rollout, or writes anywhere beneath `CODEX_HOME`.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use chrono::DateTime;
use serde_json::Value;
use sha2::{Digest, Sha256};

use comet_doc::{
    MessagePart, MessageRole, MessageStatus, SessionMessageEntry, continuation_id,
    fold_event_into_parts, sanitize_tool_call, split_parts,
};
use comet_proto::{AgentEvent, CodexHistoryListing, CodexThreadSummary, ToolCall};

use crate::EngineError;

const SUMMARY_HEAD_BYTES: u64 = 256 * 1024;
pub const DEFAULT_LIST_LIMIT: usize = 100;
pub const MAX_LIST_LIMIT: usize = 500;

#[derive(Clone, Debug)]
pub struct CodexHistory {
    root: Arc<PathBuf>,
}

#[derive(Debug)]
pub struct ParsedCodexThread {
    pub source_key: String,
    pub source_sha256: String,
    pub source_bytes: u64,
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    pub entries: Vec<SessionMessageEntry>,
    pub logical_messages: usize,
    pub part_count: usize,
    pub source_records: usize,
    pub invalid_lines: usize,
    pub first_message_at: Option<i64>,
    pub last_message_at: Option<i64>,
    pub archived: bool,
}

#[derive(Debug, Default)]
struct SummaryFields {
    session_id: Option<String>,
    cwd: Option<String>,
    title: Option<String>,
    primary: bool,
    saw_session_meta: bool,
}

#[derive(Debug)]
struct SourceFile {
    key: String,
    path: PathBuf,
    size: u64,
    modified_at: i64,
    archived: bool,
}

#[derive(Debug)]
enum ReplayRecord {
    User {
        id: String,
        created_at: i64,
        text: String,
    },
    AssistantText {
        id: String,
        created_at: i64,
        text: String,
    },
    ToolCall {
        id: String,
        created_at: i64,
        call: ToolCall,
    },
    ToolResult {
        id: String,
        created_at: i64,
        is_error: bool,
    },
    Error {
        id: String,
        created_at: i64,
        message: String,
    },
}

#[derive(Debug)]
struct AssistantTurn {
    id: String,
    created_at: i64,
    parts: Vec<MessagePart>,
}

impl CodexHistory {
    pub fn detect() -> Self {
        if let Some(root) = std::env::var_os("COMET_CODEX_HOME").filter(|value| !value.is_empty()) {
            return Self::new(root);
        }
        if let Some(root) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
            return Self::new(root);
        }
        let root = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".codex");
        Self::new(root)
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Enumerate newest primary rollouts. Spawned subagent rollouts have an
    /// object-valued `session_meta.payload.source` and are deliberately omitted
    /// from the user-facing thread list.
    pub fn list(&self, limit: usize) -> Result<CodexHistoryListing, EngineError> {
        let root_display = self.root.to_string_lossy().to_string();
        let active = self.root.join("sessions");
        let archived = self.root.join("archived_sessions");
        if !active.is_dir() && !archived.is_dir() {
            return Ok(CodexHistoryListing {
                root: root_display,
                available: false,
                threads: Vec::new(),
                truncated: false,
            });
        }

        let mut files = Vec::new();
        if active.is_dir() {
            collect_jsonl_files(&self.root, &active, false, &mut files)?;
        }
        if archived.is_dir() {
            collect_jsonl_files(&self.root, &archived, true, &mut files)?;
        }
        files.retain(|file| primary_metadata(&file.path).is_ok_and(|fields| fields.primary));
        files.sort_by(|a, b| {
            b.modified_at
                .cmp(&a.modified_at)
                .then_with(|| b.size.cmp(&a.size))
                .then_with(|| a.key.cmp(&b.key))
        });

        let limit = limit.clamp(1, MAX_LIST_LIMIT);
        let truncated = files.len() > limit;
        let threads = files
            .into_iter()
            .take(limit)
            .map(|file| summarize_file(&file))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CodexHistoryListing {
            root: root_display,
            available: true,
            threads,
            truncated,
        })
    }

    pub fn parse(
        &self,
        source_key: &str,
        device_id: &str,
    ) -> Result<ParsedCodexThread, EngineError> {
        let path = self.resolve_source(source_key)?;
        let source_bytes = path.metadata()?.len();
        let archived = source_key.starts_with("archived_sessions/");
        let mut reader = BufReader::new(File::open(&path)?);
        let mut line = Vec::new();
        let mut hash = Sha256::new();
        let mut read_bytes = 0u64;
        let mut invalid_lines = 0usize;
        let mut source_records = 0usize;
        let mut summary = SummaryFields::default();
        let mut records = Vec::new();
        let mut line_number = 0usize;

        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            line_number += 1;
            read_bytes = read_bytes.saturating_add(read as u64);
            hash.update(&line);
            let value: Value = match serde_json::from_slice(&line) {
                Ok(value) => value,
                Err(_) => {
                    invalid_lines += 1;
                    continue;
                }
            };
            source_records += 1;
            apply_summary_value(&mut summary, &value);
            if let Some(record) = replay_record(&value, line_number) {
                records.push(record);
            }
        }
        if read_bytes != source_bytes {
            return Err(EngineError::Other(
                "Codex rollout changed while it was being read; retry the import".into(),
            ));
        }
        if !summary.saw_session_meta {
            return Err(EngineError::Other(
                "Codex rollout has no session metadata".into(),
            ));
        }
        if !summary.primary {
            return Err(EngineError::Other(
                "Codex subagent rollouts cannot be imported as top-level threads".into(),
            ));
        }

        let source_digest = hash.finalize();
        let mut verify = BufReader::new(File::open(&path)?);
        let mut verification_hash = Sha256::new();
        loop {
            line.clear();
            if verify.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            verification_hash.update(&line);
        }
        if verification_hash.finalize().as_slice() != source_digest.as_slice() {
            return Err(EngineError::Other(
                "Codex rollout changed while it was being read; retry the import".into(),
            ));
        }

        let cwd = summary
            .cwd
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(default_cwd);
        let session_id = summary
            .session_id
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| session_id_from_path(&path));
        let mut logical = fold_records(records.into_iter(), device_id);
        let title = summary
            .title
            .filter(|value| !value.trim().is_empty())
            .or_else(|| first_user_title(&logical))
            .unwrap_or_else(|| format!("Codex {session_id}"));
        let logical_messages = logical.len();
        let part_count = logical.iter().map(|entry| entry.parts.len()).sum();
        let first_message_at = logical.first().map(|entry| entry.created_at);
        let last_message_at = logical.last().map(|entry| entry.created_at);
        let entries = split_message_entries(std::mem::take(&mut logical));

        Ok(ParsedCodexThread {
            source_key: source_key.to_string(),
            source_sha256: hex_digest(source_digest.as_slice()),
            source_bytes,
            session_id,
            title,
            cwd,
            entries,
            logical_messages,
            part_count,
            source_records,
            invalid_lines,
            first_message_at,
            last_message_at,
            archived,
        })
    }

    fn resolve_source(&self, source_key: &str) -> Result<PathBuf, EngineError> {
        let relative = Path::new(source_key);
        let first = relative.components().next();
        let valid_root = matches!(
            first,
            Some(Component::Normal(name))
                if name == OsStr::new("sessions") || name == OsStr::new("archived_sessions")
        );
        if source_key.trim().is_empty()
            || relative.is_absolute()
            || !valid_root
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
            || relative.extension().and_then(|value| value.to_str()) != Some("jsonl")
        {
            return Err(EngineError::Other(
                "invalid Codex history source key".into(),
            ));
        }
        let root = self.root.canonicalize().map_err(|error| {
            EngineError::Other(format!(
                "Codex home {} is unavailable: {error}",
                self.root.display()
            ))
        })?;
        let path = root.join(relative).canonicalize().map_err(|error| {
            EngineError::Other(format!("Codex history source is unavailable: {error}"))
        })?;
        if !path.starts_with(&root) || !path.is_file() {
            return Err(EngineError::Other(
                "Codex history source escaped the configured root".into(),
            ));
        }
        Ok(path)
    }
}

fn collect_jsonl_files(
    root: &Path,
    directory: &Path,
    archived: bool,
    out: &mut Vec<SourceFile>,
) -> Result<(), EngineError> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_jsonl_files(root, &path, archived, out)?;
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
        {
            let metadata = entry.metadata()?;
            let key = path
                .strip_prefix(root)
                .map_err(|error| EngineError::Other(error.to_string()))?
                .to_string_lossy()
                .to_string();
            out.push(SourceFile {
                key,
                path,
                size: metadata.len(),
                modified_at: modified_ms(&metadata),
                archived,
            });
        }
    }
    Ok(())
}

fn modified_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn primary_metadata(path: &Path) -> Result<SummaryFields, EngineError> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    for _ in 0..8 {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let mut fields = SummaryFields::default();
        apply_summary_value(&mut fields, &value);
        if fields.saw_session_meta {
            return Ok(fields);
        }
    }
    Err(EngineError::Other(format!(
        "{} has no Codex session metadata",
        path.display()
    )))
}

fn summarize_file(file: &SourceFile) -> Result<CodexThreadSummary, EngineError> {
    let fields = summary_fields(&file.path)?;
    let cwd = fields.cwd.filter(|value| !value.trim().is_empty());
    let project = cwd
        .as_deref()
        .and_then(|value| Path::new(value).file_name())
        .and_then(OsStr::to_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("Codex")
        .to_string();
    Ok(CodexThreadSummary {
        source_key: file.key.clone(),
        session_id: fields
            .session_id
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| session_id_from_path(&file.path)),
        title: fields.title.filter(|value| !value.trim().is_empty()),
        cwd,
        project,
        modified_at: file.modified_at,
        size_bytes: file.size,
        archived: file.archived,
    })
}

fn summary_fields(path: &Path) -> Result<SummaryFields, EngineError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file.take(SUMMARY_HEAD_BYTES));
    let mut fields = SummaryFields::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&line) {
            apply_summary_value(&mut fields, &value);
            if fields.saw_session_meta && fields.title.is_some() {
                break;
            }
        }
    }
    Ok(fields)
}

fn apply_summary_value(fields: &mut SummaryFields, value: &Value) {
    match value.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            fields.saw_session_meta = true;
            fields.primary = !payload.get("source").is_some_and(Value::is_object);
            fields.session_id = string_field(payload, &["session_id", "id"]);
            fields.cwd = string_field(payload, &["cwd"]);
        }
        Some("response_item") if fields.title.is_none() => {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            if payload.get("type").and_then(Value::as_str) == Some("message")
                && payload.get("role").and_then(Value::as_str) == Some("user")
                && let Some(text) = message_text(payload)
                && !internal_user_text(&text)
            {
                fields.title = compact_title(&text);
            }
        }
        _ => {}
    }
}

fn replay_record(value: &Value, line_number: usize) -> Option<ReplayRecord> {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = value.get("payload")?;
    let created_at = timestamp_ms(value).unwrap_or(0);
    let kind = payload.get("type").and_then(Value::as_str)?;
    match kind {
        "message" => {
            let role = payload.get("role").and_then(Value::as_str)?;
            let text = message_text(payload)?;
            if text.trim().is_empty() {
                return None;
            }
            let id = record_id(payload, line_number, role);
            match role {
                "user" if !internal_user_text(&text) => Some(ReplayRecord::User {
                    id,
                    created_at,
                    text,
                }),
                "assistant" => Some(ReplayRecord::AssistantText {
                    id,
                    created_at,
                    text,
                }),
                _ => None,
            }
        }
        "agent_message" => {
            string_field(payload, &["message", "text"]).map(|text| ReplayRecord::AssistantText {
                id: record_id(payload, line_number, "assistant"),
                created_at,
                text,
            })
        }
        "function_call" | "custom_tool_call" | "local_shell_call" | "web_search_call" => {
            let id = string_field(payload, &["call_id", "id"])
                .unwrap_or_else(|| record_id(payload, line_number, "tool"));
            Some(ReplayRecord::ToolCall {
                id,
                created_at,
                call: history_tool_call(payload),
            })
        }
        "function_call_output" | "custom_tool_call_output" => {
            let id = string_field(payload, &["call_id", "id"])
                .unwrap_or_else(|| record_id(payload, line_number, "tool"));
            Some(ReplayRecord::ToolResult {
                id,
                created_at,
                is_error: tool_output_is_error(payload),
            })
        }
        "error" => {
            string_field(payload, &["message", "error"]).map(|message| ReplayRecord::Error {
                id: record_id(payload, line_number, "error"),
                created_at,
                message,
            })
        }
        _ => None,
    }
}

fn message_text(payload: &Value) -> Option<String> {
    let content = payload.get("content")?.as_array()?;
    let blocks = content
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => item.get("text").and_then(Value::as_str),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    (!blocks.is_empty()).then(|| blocks.join("\n"))
}

fn history_tool_call(payload: &Value) -> ToolCall {
    let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "local_shell_call" {
        let action = payload.get("action").unwrap_or(&Value::Null);
        let command = action
            .get("command")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .or_else(|| string_field(action, &["command"]))
            .unwrap_or_default();
        return ToolCall::Exec { command };
    }
    if kind == "web_search_call" {
        return ToolCall::WebSearch {
            query: string_field(payload, &["query"])
                .or_else(|| {
                    payload
                        .pointer("/action/query")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default(),
        };
    }

    let name = string_field(payload, &["name"]).unwrap_or_else(|| kind.to_string());
    let arguments = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .and_then(|value| match value {
            Value::String(text) => serde_json::from_str::<Value>(text).ok(),
            other => Some(other.clone()),
        });
    match name.as_str() {
        "exec_command" => ToolCall::Exec {
            command: arguments
                .as_ref()
                .and_then(|value| string_field(value, &["cmd", "command"]))
                .unwrap_or_default(),
        },
        "apply_patch" => ToolCall::ApplyPatch { path: None },
        "view_image" => ToolCall::ReadFile {
            path: arguments
                .as_ref()
                .and_then(|value| string_field(value, &["path"]))
                .unwrap_or_default(),
        },
        _ => ToolCall::Unknown {
            name,
            input: arguments,
        },
    }
}

fn tool_output_is_error(payload: &Value) -> bool {
    if matches!(
        payload.get("status").and_then(Value::as_str),
        Some("failed" | "error")
    ) {
        return true;
    }
    let output = payload.get("output").unwrap_or(&Value::Null);
    output.get("isError").and_then(Value::as_bool) == Some(true)
        || output.as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("isError").and_then(Value::as_bool) == Some(true))
        })
}

fn fold_records(
    records: impl Iterator<Item = ReplayRecord>,
    device_id: &str,
) -> Vec<SessionMessageEntry> {
    let mut entries = Vec::new();
    let mut assistant: Option<AssistantTurn> = None;
    let mut last_timestamp = 0i64;

    let flush_assistant = |assistant: &mut Option<AssistantTurn>,
                           entries: &mut Vec<SessionMessageEntry>| {
        let Some(turn) = assistant.take() else {
            return;
        };
        if turn.parts.is_empty() {
            return;
        }
        entries.push(SessionMessageEntry {
            id: turn.id,
            role: MessageRole::Assistant,
            parts: turn.parts,
            created_at: turn.created_at,
            device_id: device_id.to_string(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        });
    };

    for record in records {
        match record {
            ReplayRecord::User {
                id,
                created_at,
                text,
            } => {
                flush_assistant(&mut assistant, &mut entries);
                let created_at = normalized_timestamp(created_at, &mut last_timestamp);
                entries.push(SessionMessageEntry {
                    id,
                    role: MessageRole::User,
                    parts: vec![MessagePart::Text {
                        id: "t0".into(),
                        text,
                    }],
                    created_at,
                    device_id: device_id.to_string(),
                    status: Some(MessageStatus::Complete),
                    continuation_of: None,
                });
            }
            ReplayRecord::AssistantText {
                id,
                created_at,
                text,
            } => {
                let created_at = normalized_timestamp(created_at, &mut last_timestamp);
                let turn = assistant.get_or_insert_with(|| AssistantTurn {
                    id,
                    created_at,
                    parts: Vec::new(),
                });
                if turn.parts.iter().any(|part| {
                    matches!(part, MessagePart::Text { text: existing, .. } if existing.trim() == text.trim())
                }) {
                    continue;
                }
                let text = if matches!(turn.parts.last(), Some(MessagePart::Text { .. })) {
                    format!("\n\n{text}")
                } else {
                    text
                };
                fold_event_into_parts(&mut turn.parts, &AgentEvent::TextDelta { text });
            }
            ReplayRecord::ToolCall {
                id,
                created_at,
                call,
            } => {
                let created_at = normalized_timestamp(created_at, &mut last_timestamp);
                let turn = assistant.get_or_insert_with(|| AssistantTurn {
                    id: format!("assistant-{id}"),
                    created_at,
                    parts: Vec::new(),
                });
                fold_event_into_parts(
                    &mut turn.parts,
                    &AgentEvent::ToolCall {
                        id,
                        call: sanitize_tool_call(&call),
                    },
                );
            }
            ReplayRecord::ToolResult {
                id,
                created_at,
                is_error,
            } => {
                let created_at = normalized_timestamp(created_at, &mut last_timestamp);
                let turn = assistant.get_or_insert_with(|| AssistantTurn {
                    id: format!("assistant-{id}"),
                    created_at,
                    parts: Vec::new(),
                });
                fold_event_into_parts(
                    &mut turn.parts,
                    &AgentEvent::ToolResult {
                        id,
                        is_error,
                        output: None,
                        diff: None,
                    },
                );
            }
            ReplayRecord::Error {
                id,
                created_at,
                message,
            } => {
                let created_at = normalized_timestamp(created_at, &mut last_timestamp);
                let turn = assistant.get_or_insert_with(|| AssistantTurn {
                    id,
                    created_at,
                    parts: Vec::new(),
                });
                fold_event_into_parts(&mut turn.parts, &AgentEvent::Error { message });
            }
        }
    }
    flush_assistant(&mut assistant, &mut entries);
    entries
}

fn split_message_entries(entries: Vec<SessionMessageEntry>) -> Vec<SessionMessageEntry> {
    let mut split = Vec::new();
    for entry in entries {
        let chunks = split_parts(&entry.parts);
        for (index, parts) in chunks.into_iter().enumerate() {
            let mut chunk = entry.clone();
            chunk.parts = parts;
            if index > 0 {
                chunk.id = continuation_id(&entry.id, index);
                chunk.continuation_of = Some(entry.id.clone());
            }
            split.push(chunk);
        }
    }
    split
}

fn normalized_timestamp(candidate: i64, last: &mut i64) -> i64 {
    let value = if candidate > 0 {
        candidate
    } else {
        last.saturating_add(1)
    };
    *last = (*last).max(value);
    value
}

fn timestamp_ms(value: &Value) -> Option<i64> {
    let timestamp = value.get("timestamp")?.as_str()?;
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|value| value.timestamp_millis())
}

fn first_user_title(entries: &[SessionMessageEntry]) -> Option<String> {
    entries
        .iter()
        .find(|entry| entry.role == MessageRole::User)
        .and_then(|entry| {
            entry.parts.iter().find_map(|part| match part {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
        })
        .and_then(compact_title)
}

fn compact_title(text: &str) -> Option<String> {
    let title = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| title.chars().take(80).collect())
}

fn internal_user_text(text: &str) -> bool {
    let text = text.trim_start();
    [
        "<environment_context>",
        "<permissions instructions>",
        "<recommended_plugins>",
        "<apps_instructions>",
        "<plugins_instructions>",
        "<skills_instructions>",
        "<collaboration_mode>",
        "<turn_aborted>",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn record_id(payload: &Value, line_number: usize, prefix: &str) -> String {
    string_field(payload, &["id", "call_id"])
        .unwrap_or_else(|| format!("codex-{prefix}-{line_number}"))
}

fn session_id_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown");
    stem.get(stem.len().saturating_sub(36)..)
        .filter(|candidate| {
            candidate
                .chars()
                .filter(|character| *character == '-')
                .count()
                == 4
        })
        .unwrap_or(stem)
        .to_string()
}

fn default_cwd() -> String {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .to_string_lossy()
        .to_string()
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filters_internal_user_context() {
        assert!(internal_user_text("<environment_context>\nfoo"));
        assert!(internal_user_text(" <recommended_plugins>foo"));
        assert!(!internal_user_text("please inspect <environment_context>"));
    }

    #[test]
    fn maps_known_and_unknown_tools() {
        assert_eq!(
            history_tool_call(&json!({
                "type": "function_call",
                "name": "exec_command",
                "arguments": "{\"cmd\":\"cargo test\"}"
            })),
            ToolCall::Exec {
                command: "cargo test".into()
            }
        );
        assert_eq!(
            sanitize_tool_call(&history_tool_call(&json!({
                "type": "custom_tool_call",
                "name": "exec",
                "input": "secret orchestration source"
            }))),
            ToolCall::Unknown {
                name: "exec".into(),
                input: None
            }
        );
    }
}
