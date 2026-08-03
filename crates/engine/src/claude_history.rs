//! Read-only Claude Code JSONL discovery and active-branch replay.
//!
//! Safety boundary: this module only opens files beneath the configured
//! `projects` root with [`std::fs::File::open`]. It never invokes `claude`,
//! rewrites a JSONL, or creates anything in Claude's state directory.
//!
//! Stored Claude history is not the live stream-json protocol. In particular,
//! assistant prose is present as complete `text` blocks, and one visible
//! assistant turn is usually spread over many assistant/tool-result records.
//! The importer therefore reconstructs the UUID parent graph (including
//! `compact_boundary.logicalParentUuid` links), selects the current leaf, and
//! folds records into Zeron's native message parts.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use chrono::DateTime;
use serde_json::Value;
use sha2::{Digest, Sha256};

use comet_doc::{
    MessagePart, MessageRole, MessageStatus, SessionMessageEntry, continuation_id,
    fold_event_into_parts, sanitize_tool_call, split_parts,
};
use comet_proto::{AgentEvent, ClaudeHistoryListing, ClaudeThreadSummary};

use crate::EngineError;

const SUMMARY_HEAD_BYTES: u64 = 64 * 1024;
const SUMMARY_TAIL_BYTES: u64 = 256 * 1024;
pub const DEFAULT_LIST_LIMIT: usize = 100;
pub const MAX_LIST_LIMIT: usize = 500;

#[derive(Clone, Debug)]
pub struct ClaudeHistory {
    root: Arc<PathBuf>,
}

#[derive(Debug)]
pub struct ParsedClaudeThread {
    pub source_key: String,
    pub source_sha256: String,
    pub source_bytes: u64,
    pub session_id: String,
    pub leaf_uuid: String,
    pub title: String,
    pub cwd: String,
    /// Physical doc rows; large logical messages may have continuation rows.
    pub entries: Vec<SessionMessageEntry>,
    pub logical_messages: usize,
    pub part_count: usize,
    pub branch_records: usize,
    pub invalid_lines: usize,
    pub first_message_at: Option<i64>,
    pub last_message_at: Option<i64>,
}

#[derive(Debug, Default)]
struct SummaryFields {
    session_id: Option<String>,
    title: Option<String>,
    cwd: Option<String>,
}

#[derive(Debug)]
struct SourceFile {
    key: String,
    path: PathBuf,
    size: u64,
    modified_at: i64,
}

#[derive(Debug)]
struct IndexRecord {
    parent_uuid: Option<String>,
    logical_parent_uuid: Option<String>,
    kind: String,
    subtype: Option<String>,
    sidechain: bool,
}

#[derive(Debug)]
enum AssistantOp {
    Text(String),
    Tool {
        id: String,
        call: comet_proto::ToolCall,
    },
    Error(String),
}

#[derive(Debug)]
enum ReplayRecord {
    Assistant {
        uuid: String,
        created_at: i64,
        ops: Vec<AssistantOp>,
    },
    User {
        uuid: String,
        created_at: i64,
        text: Option<String>,
        tool_results: Vec<(String, bool)>,
    },
}

#[derive(Debug)]
struct AssistantTurn {
    id: String,
    created_at: i64,
    parts: Vec<MessagePart>,
}

impl ClaudeHistory {
    pub fn detect() -> Self {
        if let Some(root) =
            std::env::var_os("COMET_CLAUDE_HISTORY_DIR").filter(|value| !value.is_empty())
        {
            return Self::new(PathBuf::from(root));
        }
        let root = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude")
            .join("projects");
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

    /// Enumerate newest JSONLs, then inspect only the bounded page. Discovery
    /// therefore costs directory metadata plus at most 320 KiB per returned
    /// thread, regardless of the size of the user's history archive.
    pub fn list(&self, limit: usize) -> Result<ClaudeHistoryListing, EngineError> {
        let root_display = self.root.to_string_lossy().to_string();
        if !self.root.is_dir() {
            return Ok(ClaudeHistoryListing {
                root: root_display,
                available: false,
                threads: Vec::new(),
                truncated: false,
            });
        }
        let mut files = Vec::new();
        collect_jsonl_files(&self.root, &self.root, &mut files)?;
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
        Ok(ClaudeHistoryListing {
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
    ) -> Result<ParsedClaudeThread, EngineError> {
        let path = self.resolve_source(source_key)?;
        let source_bytes = path.metadata()?.len();
        let mut reader = BufReader::new(File::open(&path)?);
        let mut line = Vec::new();
        let mut hash = Sha256::new();
        let mut first_read_bytes = 0u64;
        let mut index: HashMap<String, IndexRecord> = HashMap::new();
        let mut uuid_sequence = Vec::new();
        let mut compaction_leaves = Vec::new();
        let mut last_leaf: Option<String> = None;
        let mut last_main_uuid: Option<String> = None;
        let mut summary = SummaryFields::default();
        let mut invalid_lines = 0usize;

        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            first_read_bytes = first_read_bytes.saturating_add(read as u64);
            hash.update(&line);
            let value: Value = match serde_json::from_slice(&line) {
                Ok(value) => value,
                Err(_) => {
                    invalid_lines += 1;
                    continue;
                }
            };
            apply_summary_value(&mut summary, &value);
            if value.get("type").and_then(Value::as_str) == Some("last-prompt")
                && let Some(leaf) = string_field(&value, "leafUuid")
            {
                last_leaf = Some(leaf);
            }
            let Some(uuid) = string_field(&value, "uuid") else {
                continue;
            };
            let sidechain = value
                .get("isSidechain")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !sidechain {
                last_main_uuid = Some(uuid.clone());
            }
            if !sidechain
                && value.get("type").and_then(Value::as_str) == Some("system")
                && value.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                && let Some(logical_parent) = string_field(&value, "logicalParentUuid")
            {
                compaction_leaves.push(logical_parent);
            }
            uuid_sequence.push(uuid.clone());
            index.insert(
                uuid,
                IndexRecord {
                    parent_uuid: string_field(&value, "parentUuid"),
                    logical_parent_uuid: string_field(&value, "logicalParentUuid"),
                    kind: string_field(&value, "type").unwrap_or_default(),
                    subtype: string_field(&value, "subtype"),
                    sidechain,
                },
            );
        }

        let leaf_uuid = last_leaf
            .filter(|leaf| index.contains_key(leaf))
            .or(last_main_uuid)
            .ok_or_else(|| EngineError::Other("Claude thread has no UUID records".into()))?;
        let source_digest = hash.finalize();
        if first_read_bytes != source_bytes {
            return Err(EngineError::Other(
                "Claude thread changed while it was being read; retry the import".into(),
            ));
        }
        let branch = active_history_branch(&index, &uuid_sequence, &leaf_uuid, &compaction_leaves);
        let positions: HashMap<&str, usize> = branch
            .iter()
            .enumerate()
            .map(|(position, uuid)| (uuid.as_str(), position))
            .collect();
        let mut ordered: Vec<Option<ReplayRecord>> =
            std::iter::repeat_with(|| None).take(branch.len()).collect();
        let mut branch_cwd: Option<String> = None;

        let mut reader = BufReader::new(File::open(&path)?);
        let mut verification_hash = Sha256::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            verification_hash.update(&line);
            let value: Value = match serde_json::from_slice(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(uuid) = value.get("uuid").and_then(Value::as_str) else {
                continue;
            };
            let Some(&position) = positions.get(uuid) else {
                continue;
            };
            if let Some(cwd) = value
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.trim().is_empty())
            {
                branch_cwd = Some(cwd.to_string());
            }
            ordered[position] = replay_record(&value);
        }
        if verification_hash.finalize().as_slice() != source_digest.as_slice() {
            return Err(EngineError::Other(
                "Claude thread changed while it was being read; retry the import".into(),
            ));
        }

        let cwd = branch_cwd
            .or(summary.cwd)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .to_string_lossy()
                    .to_string()
            });
        let session_id = summary
            .session_id
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| session_id_from_path(&path));
        let mut logical = fold_records(ordered.into_iter().flatten(), device_id);
        let title = summary
            .title
            .filter(|value| !value.trim().is_empty())
            .or_else(|| first_user_title(&logical))
            .unwrap_or_else(|| format!("Claude {session_id}"));
        let logical_messages = logical.len();
        let part_count = logical.iter().map(|entry| entry.parts.len()).sum();
        let first_message_at = logical.first().map(|entry| entry.created_at);
        let last_message_at = logical.last().map(|entry| entry.created_at);
        let entries = split_message_entries(std::mem::take(&mut logical));

        Ok(ParsedClaudeThread {
            source_key: source_key.to_string(),
            source_sha256: hex_digest(source_digest.as_slice()),
            source_bytes,
            session_id,
            leaf_uuid,
            title,
            cwd,
            entries,
            logical_messages,
            part_count,
            branch_records: branch.len(),
            invalid_lines,
            first_message_at,
            last_message_at,
        })
    }

    fn resolve_source(&self, source_key: &str) -> Result<PathBuf, EngineError> {
        let relative = Path::new(source_key);
        if source_key.trim().is_empty()
            || relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                ) || matches!(component, Component::Normal(name) if name == OsStr::new("subagents"))
            })
            || relative.extension().and_then(|value| value.to_str()) != Some("jsonl")
        {
            return Err(EngineError::Other(
                "invalid Claude history source key".into(),
            ));
        }
        let root = self.root.canonicalize().map_err(|error| {
            EngineError::Other(format!(
                "Claude history root {} is unavailable: {error}",
                self.root.display()
            ))
        })?;
        let path = root.join(relative).canonicalize().map_err(|error| {
            EngineError::Other(format!("Claude history source is unavailable: {error}"))
        })?;
        if !path.starts_with(&root) || !path.is_file() {
            return Err(EngineError::Other(
                "Claude history source escaped the configured root".into(),
            ));
        }
        Ok(path)
    }
}

fn collect_jsonl_files(
    root: &Path,
    directory: &Path,
    out: &mut Vec<SourceFile>,
) -> Result<(), EngineError> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            if entry.file_name() == OsStr::new("subagents") {
                continue;
            }
            collect_jsonl_files(root, &path, out)?;
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

fn summarize_file(file: &SourceFile) -> Result<ClaudeThreadSummary, EngineError> {
    let fields = summary_fields(&file.path, file.size)?;
    let project = Path::new(&file.key)
        .parent()
        .map(|path| path.to_string_lossy().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Claude Code".into());
    Ok(ClaudeThreadSummary {
        source_key: file.key.clone(),
        session_id: fields
            .session_id
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| session_id_from_path(&file.path)),
        title: fields.title.filter(|value| !value.trim().is_empty()),
        cwd: fields.cwd.filter(|value| !value.trim().is_empty()),
        project,
        modified_at: file.modified_at,
        size_bytes: file.size,
    })
}

fn summary_fields(path: &Path, size: u64) -> Result<SummaryFields, EngineError> {
    let mut fields = SummaryFields::default();
    scan_summary_window(path, 0, SUMMARY_HEAD_BYTES, false, &mut fields)?;
    if size > SUMMARY_HEAD_BYTES {
        let start = size.saturating_sub(SUMMARY_TAIL_BYTES);
        scan_summary_window(path, start, SUMMARY_TAIL_BYTES, start > 0, &mut fields)?;
    }
    Ok(fields)
}

fn scan_summary_window(
    path: &Path,
    start: u64,
    max_bytes: u64,
    discard_partial_first_line: bool,
    fields: &mut SummaryFields,
) -> Result<(), EngineError> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(max_bytes));
    let mut line = Vec::new();
    if discard_partial_first_line {
        reader.read_until(b'\n', &mut line)?;
    }
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&line) {
            apply_summary_value(fields, &value);
        }
    }
    Ok(())
}

fn apply_summary_value(fields: &mut SummaryFields, value: &Value) {
    if let Some(session_id) = string_field(value, "sessionId") {
        fields.session_id = Some(session_id);
    }
    if let Some(cwd) = string_field(value, "cwd") {
        fields.cwd = Some(cwd);
    }
    for key in ["customTitle", "title", "aiTitle"] {
        if let Some(title) = string_field(value, key) {
            fields.title = Some(title);
        }
    }
}

fn active_branch(index: &HashMap<String, IndexRecord>, leaf_uuid: &str) -> Vec<String> {
    let mut reversed = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(leaf_uuid.to_string());
    while let Some(uuid) = current {
        if !seen.insert(uuid.clone()) {
            // Real Claude histories can contain a compaction logical-parent
            // link back into a chain already reached through normal parents.
            // The unique prefix is still a valid active transcript; stop at
            // the repeated node instead of rejecting the entire import.
            tracing::warn!(uuid = %uuid, "stopping at cycle in Claude history graph");
            break;
        }
        let Some(record) = index.get(&uuid) else {
            break;
        };
        reversed.push(uuid);
        current = record.parent_uuid.clone().or_else(|| {
            (record.kind == "system" && record.subtype.as_deref() == Some("compact_boundary"))
                .then(|| record.logical_parent_uuid.clone())
                .flatten()
        });
    }
    reversed.reverse();
    reversed
}

/// Claude normally connects compactions with `logicalParentUuid`, but real
/// files can point that edge into a post-summary tool chain and form a cycle.
/// Every compact boundary still records the active leaf at that moment. Union
/// those captured ancestries with today's leaf, then preserve append-only file
/// order. This recovers pre-compaction history while excluding sidechains.
fn active_history_branch(
    index: &HashMap<String, IndexRecord>,
    uuid_sequence: &[String],
    leaf_uuid: &str,
    compaction_leaves: &[String],
) -> Vec<String> {
    let mut selected = HashSet::new();
    for leaf in compaction_leaves
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(leaf_uuid))
    {
        selected.extend(active_branch(index, leaf));
    }
    uuid_sequence
        .iter()
        .filter(|uuid| {
            selected.contains(uuid.as_str())
                && index
                    .get(uuid.as_str())
                    .is_some_and(|record| !record.sidechain)
        })
        .cloned()
        .collect()
}

fn replay_record(value: &Value) -> Option<ReplayRecord> {
    if value
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let kind = value.get("type").and_then(Value::as_str)?;
    let uuid = value.get("uuid").and_then(Value::as_str)?.to_string();
    let created_at = timestamp_ms(value).unwrap_or(0);
    match kind {
        "assistant" => {
            let mut ops = Vec::new();
            let content = value.pointer("/message/content").unwrap_or(&Value::Null);
            match content {
                Value::String(text) if !text.is_empty() => {
                    ops.push(AssistantOp::Text(text.clone()));
                }
                Value::Array(blocks) => {
                    for (index, block) in blocks.iter().enumerate() {
                        match block.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text" => {
                                if let Some(text) = block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .filter(|text| !text.is_empty())
                                {
                                    ops.push(AssistantOp::Text(text.to_string()));
                                }
                            }
                            "tool_use" => {
                                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                                let input = block.get("input").unwrap_or(&Value::Null);
                                let id = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .filter(|id| !id.is_empty())
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| format!("{uuid}-tool-{index}"));
                                let call =
                                    comet_harness::claude::decode_history_tool_use(name, input);
                                ops.push(AssistantOp::Tool {
                                    id,
                                    call: sanitize_tool_call(&call),
                                });
                            }
                            // Thinking is intentionally not persisted by the live
                            // Comet fold either.
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if let Some(error) = value.get("error").and_then(Value::as_str) {
                ops.push(AssistantOp::Error(
                    comet_harness::claude::history_assistant_error_text(error),
                ));
            }
            Some(ReplayRecord::Assistant {
                uuid,
                created_at,
                ops,
            })
        }
        "user" => {
            let mut text_blocks = Vec::new();
            let mut tool_results = Vec::new();
            let content = value.pointer("/message/content").unwrap_or(&Value::Null);
            match content {
                Value::String(text) if !text.is_empty() => text_blocks.push(text.clone()),
                Value::Array(blocks) => {
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text" => {
                                if let Some(text) = block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .filter(|text| !text.is_empty())
                                {
                                    text_blocks.push(text.to_string());
                                }
                            }
                            "tool_result" => {
                                if let Some(id) = block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .filter(|id| !id.is_empty())
                                {
                                    tool_results.push((
                                        id.to_string(),
                                        block
                                            .get("is_error")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false),
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            let internal = value
                .get("isMeta")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || value
                    .get("isCompactSummary")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let joined = text_blocks.join("\n");
            let text = (!internal && !joined.trim().is_empty() && !internal_user_text(&joined))
                .then_some(joined);
            Some(ReplayRecord::User {
                uuid,
                created_at,
                text,
                tool_results,
            })
        }
        _ => None,
    }
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
            ReplayRecord::Assistant {
                uuid,
                created_at,
                ops,
            } => {
                let created_at = normalized_timestamp(created_at, &mut last_timestamp);
                let turn = assistant.get_or_insert_with(|| AssistantTurn {
                    id: uuid,
                    created_at,
                    parts: Vec::new(),
                });
                for op in ops {
                    let event = match op {
                        AssistantOp::Text(text) => AgentEvent::TextDelta { text },
                        AssistantOp::Tool { id, call } => AgentEvent::ToolCall { id, call },
                        AssistantOp::Error(message) => AgentEvent::Error { message },
                    };
                    fold_event_into_parts(&mut turn.parts, &event);
                }
            }
            ReplayRecord::User {
                uuid,
                created_at,
                text,
                tool_results,
            } => {
                if let Some(turn) = assistant.as_mut() {
                    for (id, is_error) in tool_results {
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
                }
                let Some(text) = text else {
                    continue;
                };
                flush_assistant(&mut assistant, &mut entries);
                let created_at = normalized_timestamp(created_at, &mut last_timestamp);
                entries.push(SessionMessageEntry {
                    id: uuid,
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
                MessagePart::Text { text, .. } => Some(text),
                _ => None,
            })
        })
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(80).collect())
}

fn internal_user_text(text: &str) -> bool {
    let text = text.trim_start();
    [
        "<local-command-caveat>",
        "<local-command-stdout>",
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "<system-reminder>",
        "<ide_opened_file>",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown")
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
    use comet_proto::ToolCall;
    use serde_json::json;

    fn write_jsonl(path: &Path, rows: &[Value]) -> Vec<u8> {
        let bytes = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(path, bytes.as_bytes()).unwrap();
        bytes.into_bytes()
    }

    #[test]
    fn reconstructs_branch_across_compaction_without_touching_source() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("-project");
        std::fs::create_dir(&project).unwrap();
        let path = project.join("session-1.jsonl");
        let rows = vec![
            json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"session-1","cwd":"/work","timestamp":"2026-01-01T00:00:00Z","message":{"content":"real prompt"}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"session-1","cwd":"/work","timestamp":"2026-01-01T00:00:01Z","message":{"content":[{"type":"text","text":"before compact"},{"type":"tool_use","id":"tool-1","name":"Write","input":{"file_path":"/tmp/x","content":"secret body"}}]}}),
            json!({"type":"user","uuid":"r1","parentUuid":"a1","sessionId":"session-1","cwd":"/work","timestamp":"2026-01-01T00:00:02Z","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":false,"content":"large result"}]}}),
            json!({"type":"assistant","uuid":"dead","parentUuid":"u1","sessionId":"session-1","cwd":"/work","timestamp":"2026-01-01T00:00:03Z","message":{"content":[{"type":"text","text":"abandoned sibling"}]}}),
            json!({"type":"system","subtype":"compact_boundary","uuid":"compact","parentUuid":null,"logicalParentUuid":"r1","sessionId":"session-1","cwd":"/work","timestamp":"2026-01-01T00:00:04Z"}),
            json!({"type":"user","uuid":"summary","parentUuid":"compact","sessionId":"session-1","cwd":"/work","isCompactSummary":true,"isVisibleInTranscriptOnly":true,"timestamp":"2026-01-01T00:00:05Z","message":{"content":"generated summary must not render"}}),
            json!({"type":"user","uuid":"meta","parentUuid":"summary","sessionId":"session-1","cwd":"/work","isMeta":true,"timestamp":"2026-01-01T00:00:06Z","message":{"content":[{"type":"text","text":"skill injection"}]}}),
            json!({"type":"user","uuid":"u2","parentUuid":"meta","sessionId":"session-1","cwd":"/work","timestamp":"2026-01-01T00:00:07Z","message":{"content":"after compact"}}),
            json!({"type":"assistant","uuid":"a2","parentUuid":"u2","sessionId":"session-1","cwd":"/work","timestamp":"2026-01-01T00:00:08Z","message":{"content":[{"type":"thinking","thinking":"private"},{"type":"text","text":"final answer"}]}}),
            json!({"type":"system","subtype":"turn_duration","uuid":"leaf","parentUuid":"a2","sessionId":"session-1","cwd":"/work","timestamp":"2026-01-01T00:00:09Z"}),
            json!({"type":"ai-title","sessionId":"session-1","aiTitle":"Imported title"}),
            json!({"type":"last-prompt","sessionId":"session-1","leafUuid":"leaf","lastPrompt":"after compact"}),
        ];
        let before = write_jsonl(&path, &rows);
        let parsed = ClaudeHistory::new(temp.path())
            .parse("-project/session-1.jsonl", "device")
            .unwrap();
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "source JSONL changed");
        assert_eq!(parsed.title, "Imported title");
        assert_eq!(parsed.logical_messages, 4);
        let texts = parsed
            .entries
            .iter()
            .flat_map(|entry| &entry.parts)
            .filter_map(|part| match part {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                "real prompt",
                "before compact",
                "after compact",
                "final answer"
            ]
        );
        let tool = parsed
            .entries
            .iter()
            .flat_map(|entry| &entry.parts)
            .find_map(|part| match part {
                MessagePart::Tool {
                    call,
                    resolved,
                    is_error,
                    ..
                } => Some((call, resolved, is_error)),
                _ => None,
            })
            .unwrap();
        assert_eq!(tool.1, &true);
        assert_eq!(tool.2, &false);
        assert_eq!(
            tool.0,
            &ToolCall::WriteFile {
                path: "/tmp/x".into(),
                content: None
            }
        );
    }

    #[test]
    fn source_keys_cannot_escape_root() {
        let temp = tempfile::tempdir().unwrap();
        let history = ClaudeHistory::new(temp.path());
        assert!(history.parse("../outside.jsonl", "d").is_err());
        assert!(history.parse("/tmp/outside.jsonl", "d").is_err());
        assert!(history.parse("not-json.txt", "d").is_err());
    }

    #[test]
    fn subagent_sources_are_hidden_and_cannot_be_imported_directly() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("-project");
        let subagents = project.join("session-1").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        write_jsonl(
            &project.join("session-1.jsonl"),
            &[json!({
                "type": "user",
                "uuid": "main-user",
                "sessionId": "session-1",
                "cwd": "/work",
                "message": { "content": "main thread" }
            })],
        );
        write_jsonl(
            &subagents.join("agent-1.jsonl"),
            &[json!({
                "type": "user",
                "uuid": "subagent-user",
                "sessionId": "agent-1",
                "cwd": "/work",
                "message": { "content": "subagent thread" }
            })],
        );

        let history = ClaudeHistory::new(temp.path());
        let listing = history.list(100).unwrap();
        assert_eq!(listing.threads.len(), 1);
        assert_eq!(listing.threads[0].source_key, "-project/session-1.jsonl");
        assert!(
            history
                .parse("-project/session-1/subagents/agent-1.jsonl", "device")
                .is_err()
        );
    }

    #[test]
    fn graph_cycles_stop_at_the_unique_prefix() {
        let index = HashMap::from([
            (
                "a".into(),
                IndexRecord {
                    parent_uuid: Some("b".into()),
                    logical_parent_uuid: None,
                    kind: "user".into(),
                    subtype: None,
                    sidechain: false,
                },
            ),
            (
                "b".into(),
                IndexRecord {
                    parent_uuid: Some("a".into()),
                    logical_parent_uuid: None,
                    kind: "assistant".into(),
                    subtype: None,
                    sidechain: false,
                },
            ),
        ]);
        let branch = active_branch(&index, "a");
        assert_eq!(branch.len(), 2);
        assert_eq!(branch.iter().collect::<HashSet<_>>().len(), 2);
    }
}
