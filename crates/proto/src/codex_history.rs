//! Codex rollout history-import wire types.
//!
//! Codex's JSONL files remain the source of truth. Import copies their visible
//! transcript into a native Zeron session doc and preserves the thread id so
//! the next Zeron turn resumes the same Codex thread.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexThreadSummary {
    /// Path relative to `CODEX_HOME`. Only `sessions/**.jsonl` and
    /// `archived_sessions/**.jsonl` keys are accepted back by the engine.
    pub source_key: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub project: String,
    pub modified_at: i64,
    pub size_bytes: u64,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexHistoryListing {
    pub root: String,
    pub available: bool,
    pub threads: Vec<CodexThreadSummary>,
    /// True when more primary rollout files exist than this bounded page contains.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexImportResult {
    pub chat_id: String,
    pub space_id: String,
    pub source_key: String,
    pub source_sha256: String,
    pub source_bytes: u64,
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    pub imported_messages: usize,
    pub imported_parts: usize,
    pub source_records: usize,
    pub invalid_lines: usize,
    pub archived: bool,
}
