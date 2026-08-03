//! Codex catalog: models, effort ladders, and service tiers. The protocol
//! adapter itself is the shared ACP harness ([`crate::AcpHarness::codex`],
//! via the org-maintained `codex-acp` adapter wrapping the codex app-server)
//! — the bespoke app-server harness this module used to hold was retired
//! with the ACP conversion (docs/research/acp.md).

pub(crate) mod catalog;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use tokio::process::Command;

use crate::HarnessError;

/// Locate the Codex CLI used behind `codex-acp`. Kept separate from adapter
/// discovery because archived imported rollouts need one best-effort
/// app-server unarchive before ACP retries `session/load`.
fn resolve_codex_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CODEX_EXECUTABLE")
        && !path.is_empty()
    {
        return Some(PathBuf::from(path));
    }
    let exe = if cfg!(windows) { "codex.exe" } else { "codex" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|directory| !directory.as_os_str().is_empty())
                .map(|directory| directory.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(shell_path) = crate::shell_env::login_shell_path() {
        candidates.extend(
            std::env::split_paths(shell_path)
                .filter(|directory| !directory.as_os_str().is_empty())
                .map(|directory| directory.join(exe)),
        );
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local").join("bin").join(exe));
        candidates.push(home.join(".codex").join("bin").join(exe));
        candidates.push(home.join(".npm-global").join("bin").join(exe));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin").join(exe));
    candidates.push(PathBuf::from("/usr/local/bin").join(exe));
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|directory| directory.join(exe)),
    );
    candidates.into_iter().find(|path| path.is_file())
}

/// Restore an archived Codex rollout before the ACP adapter retries loading
/// it. This runs only after `codex-acp` reports that `session/load` failed;
/// errors remain best-effort so a missing/foreign thread still falls back to a
/// fresh ACP session exactly as upstream does.
pub(crate) async fn unarchive_thread(thread_id: &str) -> Result<(), HarnessError> {
    let executable = resolve_codex_executable().ok_or_else(|| {
        HarnessError::NotInstalled("codex executable unavailable for thread unarchive".into())
    })?;
    unarchive_thread_with(&executable, thread_id).await
}

async fn unarchive_thread_with(executable: &Path, thread_id: &str) -> Result<(), HarnessError> {
    let mut command = Command::new(executable);
    command.arg("app-server");
    crate::compose_child_path(&mut command, executable);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Protocol("codex child has no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Protocol("codex child has no stdout".into()))?;
    let (client, _incoming) = crate::jsonrpc::RpcClient::new(stdin, stdout);
    let operation = async {
        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "zeron-native",
                        "title": "Zeron",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": { "experimentalApi": true },
                }),
            )
            .await?;
        client.notify("initialized", None);
        client
            .request("thread/unarchive", json!({ "threadId": thread_id }))
            .await?;
        Ok::<(), HarnessError>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(5), operation)
        .await
        .map_err(|_| HarnessError::Protocol("codex thread unarchive timed out".into()))?;
    crate::shutdown_child(&mut child, Duration::from_millis(250)).await;
    result
}
