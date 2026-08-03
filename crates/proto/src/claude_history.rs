//! Claude Code history-import wire types.
//!
//! Claude's JSONL files remain the source of truth. Import copies their visible
//! active-branch transcript into a native Zeron session doc and preserves the
//! original session id so the next Zeron turn continues the same thread.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeThreadSummary {
    /// Path relative to the configured Claude projects root. This is the only
    /// source identifier accepted back by the engine (absolute paths are never
    /// accepted over RPC).
    pub source_key: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub project: String,
    pub modified_at: i64,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeHistoryListing {
    pub root: String,
    pub available: bool,
    pub threads: Vec<ClaudeThreadSummary>,
    /// True when more files exist than this bounded page contains.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeImportResult {
    pub chat_id: String,
    pub space_id: String,
    pub source_key: String,
    pub source_sha256: String,
    pub source_bytes: u64,
    pub session_id: String,
    pub leaf_uuid: String,
    pub title: String,
    pub cwd: String,
    pub imported_messages: usize,
    pub imported_parts: usize,
    pub branch_records: usize,
    pub invalid_lines: usize,
}
