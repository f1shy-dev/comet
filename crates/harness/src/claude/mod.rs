//! Claude Code catalog: models, effort ladders, and the Ultrathink prompt
//! convention. The protocol adapter itself is the shared ACP harness
//! ([`crate::AcpHarness::claude`], via the org-maintained `claude-agent-acp`
//! adapter on the Claude Agent SDK) — the bespoke stream-json harness this
//! module used to hold was retired with the ACP conversion
//! (docs/research/acp.md).

pub(crate) mod catalog;

use std::path::{Path, PathBuf};

use comet_proto::{TodoItem, ToolCall};
use serde_json::Value;

/// Locate the device's preferred Claude-compatible CLI. An explicit
/// `CLAUDE_CODE_EXECUTABLE` wins; otherwise KumiClaude is preferred over the
/// stock Claude binary across the process PATH, login-shell PATH, and known
/// install roots. The ACP adapter accepts this path through the same
/// environment variable.
pub(crate) fn resolve_claude_executable() -> Option<PathBuf> {
    let explicit = std::env::var_os("CLAUDE_CODE_EXECUTABLE");
    let path = std::env::var_os("PATH");
    let shell_path = crate::shell_env::login_shell_path();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let node_bins = crate::node_version_manager_bins();
    resolve_claude_executable_from(
        explicit.as_deref(),
        path.as_deref(),
        shell_path,
        home.as_deref(),
        &node_bins,
    )
}

fn resolve_claude_executable_from(
    explicit: Option<&std::ffi::OsStr>,
    path: Option<&std::ffi::OsStr>,
    shell_path: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
    node_bins: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(path) = explicit.filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }

    let claude_exe = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let kumi_exe = if cfg!(windows) {
        "kumiclaude.exe"
    } else {
        "kumiclaude"
    };
    let mut path_dirs = Vec::new();
    for value in [path, shell_path].into_iter().flatten() {
        path_dirs.extend(
            std::env::split_paths(value).filter(|directory| !directory.as_os_str().is_empty()),
        );
    }

    let mut candidates: Vec<PathBuf> = path_dirs.iter().map(|dir| dir.join(kumi_exe)).collect();
    if let Some(home) = home {
        candidates.push(home.join(".local").join("bin").join(kumi_exe));
        candidates.push(
            home.join("Development")
                .join("kumiclaude")
                .join("bin")
                .join(kumi_exe),
        );
    }
    candidates.extend(node_bins.iter().map(|dir| dir.join(kumi_exe)));
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        return Some(path);
    }

    let mut candidates: Vec<PathBuf> = path_dirs.iter().map(|dir| dir.join(claude_exe)).collect();
    if let Some(home) = home {
        candidates.push(home.join(".claude").join("local").join(claude_exe));
        candidates.push(home.join(".local").join("bin").join(claude_exe));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin").join(claude_exe));
    candidates.push(PathBuf::from("/usr/local/bin").join(claude_exe));
    candidates.extend(node_bins.iter().map(|dir| dir.join(claude_exe)));
    candidates.into_iter().find(|path| path.is_file())
}

/// Decode a persisted Claude `tool_use` block for the history importer.
pub fn decode_history_tool_use(name: &str, input: &Value) -> ToolCall {
    let string = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let optional = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    match name {
        "Bash" => ToolCall::Exec {
            command: string("command"),
        },
        "Read" => ToolCall::ReadFile {
            path: string("file_path"),
        },
        "Write" => ToolCall::WriteFile {
            path: string("file_path"),
            content: optional("content"),
        },
        "Edit" => ToolCall::EditFile {
            path: string("file_path"),
            old_string: optional("old_string"),
            new_string: optional("new_string"),
        },
        "Grep" => ToolCall::Search {
            pattern: string("pattern"),
            path: optional("path"),
        },
        "Glob" => ToolCall::Glob {
            pattern: string("pattern"),
        },
        "WebFetch" => ToolCall::WebFetch {
            url: string("url"),
            prompt: optional("prompt"),
        },
        "WebSearch" => ToolCall::WebSearch {
            query: string("query"),
        },
        "TodoWrite" => ToolCall::Todo {
            items: input
                .get("todos")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|todo| TodoItem {
                    text: todo
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    done: todo.get("status").and_then(Value::as_str) == Some("completed"),
                })
                .collect(),
        },
        _ => match name.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
            Some((server, tool)) => ToolCall::Mcp {
                server: server.to_owned(),
                tool: tool.to_owned(),
                input: (!input.is_null()).then(|| input.clone()),
            },
            None => ToolCall::Unknown {
                name: name.to_owned(),
                input: (!input.is_null()).then(|| input.clone()),
            },
        },
    }
}

/// Keep imported assistant errors worded like the retired native adapter did.
pub fn history_assistant_error_text(code: &str) -> String {
    match code {
        "authentication_failed" => "Authentication failed — sign in to Claude again.".into(),
        "oauth_org_not_allowed" => {
            "This organization isn't allowed to use Claude here.".into()
        }
        "billing_error" => "Billing error — check your Claude plan or payment method.".into(),
        "rate_limit" => "Claude usage limit reached — try again after the limit resets.".into(),
        "overloaded" => "Claude is overloaded right now — try again shortly.".into(),
        "invalid_request" => "The request was rejected as invalid.".into(),
        "model_not_found" => "The selected model isn't available.".into(),
        "server_error" => "Claude had a server error — try again.".into(),
        "max_output_tokens" => "The reply hit the maximum output length.".into(),
        "unknown" => "Claude returned an unspecified error.".into(),
        other => format!("Claude error: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kumiclaude_candidates_precede_stock_claude() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let kumi = bin.join(if cfg!(windows) {
            "kumiclaude.exe"
        } else {
            "kumiclaude"
        });
        let claude = bin.join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        });
        std::fs::write(&kumi, b"kumi").unwrap();
        std::fs::write(&claude, b"claude").unwrap();
        let search_path = std::env::join_paths([&bin]).unwrap();

        assert_eq!(
            resolve_claude_executable_from(None, Some(&search_path), None, None, &[]),
            Some(kumi.clone())
        );
        std::fs::remove_file(&kumi).unwrap();
        assert_eq!(
            resolve_claude_executable_from(None, Some(&search_path), None, None, &[]),
            Some(claude)
        );
    }
}
