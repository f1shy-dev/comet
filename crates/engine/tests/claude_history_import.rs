//! Claude history RPC/persistence tests.
//!
//! No test in this file invokes a Claude harness. The optional real-data audit
//! accepts only a directory of COPIED JSONLs and verifies those copies are
//! byte-identical before/after import.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use comet_engine::{ClaudeHistory, EngineCore, HarnessRegistry};
use comet_harness::mock::MockHarness;
use comet_proto::{ClaudeHistoryListing, ClaudeImportResult, HarnessId};
use comet_rpc::methods;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness { script: Vec::new() }));
    Arc::new(registry)
}

fn assemble(data_dir: &Path, history_root: &Path) -> EngineCore {
    let mut core = EngineCore::assemble(data_dir, registry(), HarnessId::Mock, None)
        .expect("engine assembles");
    core.claude_history = ClaudeHistory::new(history_root);
    core
}

fn write_fixture(root: &Path) -> (String, Vec<u8>) {
    write_fixture_with_cwd(root, "/fixture/work")
}

fn write_fixture_with_cwd(root: &Path, cwd: &str) -> (String, Vec<u8>) {
    let project = root.join("-fixture-project");
    std::fs::create_dir_all(&project).unwrap();
    let key = "-fixture-project/session-fixture.jsonl".to_string();
    let rows = [
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"session-fixture","cwd":cwd,"timestamp":"2026-01-01T00:00:00Z","message":{"content":"hello importer"}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"session-fixture","cwd":cwd,"timestamp":"2026-01-01T00:00:01Z","message":{"content":[{"type":"text","text":"hello back"}]}}),
        json!({"type":"system","subtype":"turn_duration","uuid":"leaf","parentUuid":"a1","sessionId":"session-fixture","cwd":cwd,"timestamp":"2026-01-01T00:00:02Z"}),
        json!({"type":"ai-title","sessionId":"session-fixture","aiTitle":"Fixture import"}),
        json!({"type":"last-prompt","sessionId":"session-fixture","leafUuid":"leaf","lastPrompt":"hello importer"}),
    ];
    let bytes = (rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
        .into_bytes();
    std::fs::write(root.join(&key), &bytes).unwrap();
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
async fn rpc_import_attaches_to_source_and_is_idempotent() {
    let history_root = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let (source_key, source_before) = write_fixture(history_root.path());
    let core = assemble(data_dir.path(), history_root.path());
    let client = comet_rpc::memory_client(core.rpc_service());

    let listing: ClaudeHistoryListing = serde_json::from_value(
        client
            .call(methods::LIST_CLAUDE_THREADS, json!({"limit": 10}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(listing.available);
    assert_eq!(listing.threads.len(), 1);
    assert_eq!(listing.threads[0].session_id, "session-fixture");

    let import: ClaudeImportResult = serde_json::from_value(
        client
            .call(
                methods::IMPORT_CLAUDE_THREAD,
                json!({"sourceKey": source_key}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(import.imported_messages, 2);
    assert_eq!(
        std::fs::read(history_root.path().join(&source_key)).unwrap(),
        source_before
    );

    let chat = core.workspace.chat(&import.chat_id).unwrap().unwrap();
    assert_eq!(chat.harness_session_id.as_deref(), Some("session-fixture"));
    assert_eq!(chat.harness_session_cwd.as_deref(), Some("/fixture/work"));
    let first_entries = core
        .doc_host
        .open(&import.chat_id)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert_eq!(first_entries.len(), 2);

    // Retry returns the same deterministic chat and does not duplicate rows.
    let retry: ClaudeImportResult = serde_json::from_value(
        client
            .call(
                methods::IMPORT_CLAUDE_THREAD,
                json!({"sourceKey": source_key}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retry.chat_id, import.chat_id);
    assert_eq!(
        core.doc_host
            .open(&import.chat_id)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        std::fs::read(history_root.path().join(&source_key)).unwrap(),
        source_before
    );
    core.shutdown().await;
}

#[tokio::test]
async fn rpc_import_groups_linked_worktree_under_main_repo_space() {
    let git_root = tempfile::tempdir().unwrap();
    let (repo, worktree) = linked_worktree(git_root.path());
    let history_root = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let worktree_cwd = worktree.to_string_lossy().to_string();
    let (source_key, _) = write_fixture_with_cwd(history_root.path(), &worktree_cwd);
    let core = assemble(data_dir.path(), history_root.path());
    let client = comet_rpc::memory_client(core.rpc_service());

    let imported: ClaudeImportResult = serde_json::from_value(
        client
            .call(
                methods::IMPORT_CLAUDE_THREAD,
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

#[tokio::test]
async fn retry_does_not_regroup_an_existing_import() {
    let git_root = tempfile::tempdir().unwrap();
    let future_worktree = git_root.path().join("linked-worktree");
    let worktree_cwd = future_worktree.to_string_lossy().to_string();
    let history_root = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let (source_key, _) = write_fixture_with_cwd(history_root.path(), &worktree_cwd);
    let core = assemble(data_dir.path(), history_root.path());
    let client = comet_rpc::memory_client(core.rpc_service());

    // First import predates the worktree, reproducing an existing legacy
    // import whose space is the exact source cwd.
    let first: ClaudeImportResult = serde_json::from_value(
        client
            .call(
                methods::IMPORT_CLAUDE_THREAD,
                json!({"sourceKey": source_key}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    let first_space = core
        .workspace
        .space(&first.space_id)
        .unwrap()
        .unwrap();
    assert_eq!(first_space.path, worktree_cwd);

    // Once that path becomes a real linked worktree, a retry can resolve its
    // main repo—but must still preserve the already-imported chat grouping.
    let _ = linked_worktree(git_root.path());
    let retry: ClaudeImportResult = serde_json::from_value(
        client
            .call(
                methods::IMPORT_CLAUDE_THREAD,
                json!({"sourceKey": source_key}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retry.chat_id, first.chat_id);
    assert_eq!(retry.space_id, first.space_id);
    assert_eq!(core.workspace.read_spaces().unwrap().len(), 1);
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

/// Run manually with a root containing copied (never original) Claude JSONLs:
/// `COMET_CLAUDE_HISTORY_AUDIT_ROOT=/tmp/... cargo test -p comet-engine
///   --test claude_history_import imports_copied_real_threads -- --ignored --nocapture`
#[tokio::test]
#[ignore = "requires an explicit directory of copied real Claude JSONLs"]
async fn imports_copied_real_threads_without_source_mutation() {
    let root = PathBuf::from(
        std::env::var_os("COMET_CLAUDE_HISTORY_AUDIT_ROOT")
            .expect("COMET_CLAUDE_HISTORY_AUDIT_ROOT must name copied JSONLs"),
    );
    let canonical_root = root.canonicalize().expect("audit root exists");
    assert!(
        root.starts_with("/tmp") || canonical_root.starts_with("/private/tmp"),
        "audit root must be under /tmp"
    );
    let data_dir = tempfile::tempdir().unwrap();
    let core = assemble(data_dir.path(), &root);
    let client = comet_rpc::memory_client(core.rpc_service());
    let listing: ClaudeHistoryListing = serde_json::from_value(
        client
            .call(methods::LIST_CLAUDE_THREADS, json!({"limit": 10}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(listing.threads.len() >= 3, "copy at least three JSONLs");

    for thread in listing.threads.iter().take(3) {
        let source = root.join(&thread.source_key);
        let before = sha256_file(&source);
        let started = std::time::Instant::now();
        let imported: ClaudeImportResult = serde_json::from_value(
            client
                .call(
                    methods::IMPORT_CLAUDE_THREAD,
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
            "{}: {} MiB, {} messages, {} parts, {} branch records, {:?}",
            imported.session_id,
            imported.source_bytes / (1024 * 1024),
            imported.imported_messages,
            imported.imported_parts,
            imported.branch_records,
            started.elapsed(),
        );
    }
    core.shutdown().await;
}
