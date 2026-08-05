//! Codex history RPC/persistence tests.
//!
//! Tests never invoke Codex. The optional real-data audit accepts only a
//! directory of copied rollout JSONLs and verifies those copies are unchanged.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use comet_doc::MessagePart;
use comet_engine::{CodexHistory, EngineCore, HarnessRegistry};
use comet_harness::mock::MockHarness;
use comet_proto::{CodexHistoryListing, CodexImportResult, HarnessId};
use comet_rpc::methods;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness { script: Vec::new() }));
    Arc::new(registry)
}

fn assemble(data_dir: &Path, codex_home: &Path) -> EngineCore {
    let mut core = EngineCore::assemble(data_dir, registry(), HarnessId::Mock, None)
        .expect("engine assembles");
    core.codex_history = CodexHistory::new(codex_home);
    core
}

fn write_rows(path: &Path, rows: &[Value]) -> Vec<u8> {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = (rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
        .into_bytes();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

fn write_fixture(root: &Path) -> (String, Vec<u8>) {
    write_fixture_with_cwd(root, "/fixture/work")
}

fn write_fixture_with_cwd(root: &Path, cwd: &str) -> (String, Vec<u8>) {
    let key = "sessions/2026/01/01/rollout-fixture.jsonl".to_string();
    let rows = [
        json!({"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"thread-fixture","session_id":"thread-fixture","cwd":cwd,"source":"vscode","originator":"codex_cli_rs"}}),
        json!({"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"internal instructions"}]}}),
        json!({"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>internal</environment_context>"}]}}),
        json!({"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello Codex importer"}]}}),
        json!({"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"custom_tool_call","id":"item-tool","call_id":"call-1","name":"exec","input":"sensitive orchestration source","status":"completed"}}),
        json!({"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-1","output":[{"type":"text","text":"private output"}]}}),
        json!({"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"message","id":"assistant-1","role":"assistant","content":[{"type":"output_text","text":"hello back"}]}}),
        json!({"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"one more thing"}]}}),
        json!({"timestamp":"2026-01-01T00:00:06Z","type":"response_item","payload":{"type":"message","id":"assistant-2","role":"assistant","content":[{"type":"output_text","text":"done"}]}}),
    ];
    let bytes = write_rows(&root.join(&key), &rows);

    // A spawned subagent rollout must not appear as a top-level import choice.
    write_rows(
        &root.join("sessions/2026/01/01/rollout-subagent.jsonl"),
        &[
            json!({"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"subagent","session_id":"subagent","cwd":"/fixture/work","source":{"subagent":{"thread_spawn":{"parent_thread_id":"thread-fixture"}}}}}),
            json!({"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"delegated task"}]}}),
        ],
    );
    (key, bytes)
}

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .expect("git spawns");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn linked_worktree(root: &Path) -> (PathBuf, PathBuf) {
    let repo = root.join("repo");
    let worktree = root.join("linked-worktree");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "fixture\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature/import",
            worktree.to_str().unwrap(),
        ],
    );
    (repo, worktree)
}

#[tokio::test]
async fn rpc_import_attaches_to_source_filters_internal_rows_and_is_idempotent() {
    let codex_home = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let (source_key, source_before) = write_fixture(codex_home.path());
    let core = assemble(data_dir.path(), codex_home.path());
    let client = comet_rpc::memory_client(core.rpc_service());

    let listing: CodexHistoryListing = serde_json::from_value(
        client
            .call(methods::LIST_CODEX_THREADS, json!({"limit": 10}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(listing.available);
    assert_eq!(listing.threads.len(), 1);
    assert_eq!(listing.threads[0].session_id, "thread-fixture");
    assert_eq!(
        listing.threads[0].title.as_deref(),
        Some("hello Codex importer")
    );

    let imported: CodexImportResult = serde_json::from_value(
        client
            .call(
                methods::IMPORT_CODEX_THREAD,
                json!({"sourceKey": source_key}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(imported.imported_messages, 4);
    assert_eq!(imported.title, "hello Codex importer");
    assert_eq!(
        std::fs::read(codex_home.path().join(&source_key)).unwrap(),
        source_before
    );

    let chat = core
        .workspace
        .chat(&imported.chat_id)
        .unwrap()
        .unwrap();
    assert_eq!(chat.harness_session_id.as_deref(), Some("thread-fixture"));
    assert_eq!(chat.harness_session_cwd.as_deref(), Some("/fixture/work"));
    assert_eq!(chat.config.unwrap().harness, HarnessId::Codex);

    let entries = core
        .doc_host
        .open(&imported.chat_id)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert_eq!(entries.len(), 4);
    assert!(entries[1].parts.iter().any(|part| matches!(
        part,
        MessagePart::Tool {
            resolved: true,
            is_error: false,
            ..
        }
    )));

    let retry: CodexImportResult = serde_json::from_value(
        client
            .call(
                methods::IMPORT_CODEX_THREAD,
                json!({"sourceKey": source_key}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retry.chat_id, imported.chat_id);
    assert_eq!(
        core.doc_host
            .open(&imported.chat_id)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .len(),
        4
    );
    core.shutdown().await;
}

#[tokio::test]
async fn rpc_import_groups_linked_worktree_under_main_repo_space() {
    let git_root = tempfile::tempdir().unwrap();
    let (repo, worktree) = linked_worktree(git_root.path());
    let codex_home = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let worktree_cwd = worktree.to_string_lossy().to_string();
    let (source_key, _) = write_fixture_with_cwd(codex_home.path(), &worktree_cwd);
    let core = assemble(data_dir.path(), codex_home.path());
    let client = comet_rpc::memory_client(core.rpc_service());

    let imported: CodexImportResult = serde_json::from_value(
        client
            .call(
                methods::IMPORT_CODEX_THREAD,
                json!({"sourceKey": source_key}),
            )
            .await
            .unwrap(),
    )
    .unwrap();

    let chat = core
        .workspace
        .chat(&imported.chat_id)
        .unwrap()
        .unwrap();
    assert_eq!(chat.cwd.as_deref(), Some(worktree_cwd.as_str()));
    assert_eq!(
        chat.harness_session_cwd.as_deref(),
        Some(worktree_cwd.as_str())
    );
    let space = core
        .workspace
        .space(chat.space_id.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::canonicalize(space.path).unwrap(),
        std::fs::canonicalize(repo).unwrap()
    );
    core.shutdown().await;
}

fn sha256_file(path: &Path) -> String {
    let mut reader = std::io::BufReader::new(std::fs::File::open(path).unwrap());
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = reader.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Run manually with a copied CODEX_HOME-shaped directory beneath `/tmp`:
/// `COMET_CODEX_HISTORY_AUDIT_ROOT=/tmp/... cargo test -p comet-engine
///   --test codex_history_import imports_copied_real_threads -- --ignored --nocapture`
#[tokio::test]
#[ignore = "requires an explicit directory of copied real Codex rollouts"]
async fn imports_copied_real_threads_without_source_mutation() {
    let root = PathBuf::from(
        std::env::var_os("COMET_CODEX_HISTORY_AUDIT_ROOT")
            .expect("COMET_CODEX_HISTORY_AUDIT_ROOT must name copied JSONLs"),
    );
    let canonical_root = root.canonicalize().expect("audit root exists");
    assert!(
        root.starts_with("/tmp") || canonical_root.starts_with("/private/tmp"),
        "audit root must be under /tmp"
    );
    let data_dir = tempfile::tempdir().unwrap();
    let core = assemble(data_dir.path(), &root);
    let client = comet_rpc::memory_client(core.rpc_service());
    let listing: CodexHistoryListing = serde_json::from_value(
        client
            .call(methods::LIST_CODEX_THREADS, json!({"limit": 10}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        listing.threads.len() >= 3,
        "copy at least three primary rollouts"
    );

    for thread in listing.threads.iter().take(3) {
        let source = root.join(&thread.source_key);
        let before = sha256_file(&source);
        let started = std::time::Instant::now();
        let imported: CodexImportResult = serde_json::from_value(
            client
                .call(
                    methods::IMPORT_CODEX_THREAD,
                    json!({"sourceKey": thread.source_key}),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        let after = sha256_file(&source);
        assert_eq!(before, after, "copied source changed: {}", source.display());
        assert_eq!(before, imported.source_sha256);
        let chat = core
            .workspace
            .chat(&imported.chat_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            chat.harness_session_id.as_deref(),
            Some(imported.session_id.as_str())
        );
        println!(
            "{}: {} MiB, {} messages, {} parts, {} source records, {:?}",
            imported.session_id,
            imported.source_bytes / (1024 * 1024),
            imported.imported_messages,
            imported.imported_parts,
            imported.source_records,
            started.elapsed(),
        );
    }
    core.shutdown().await;
}
