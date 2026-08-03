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
    let project = root.join("-fixture-project");
    std::fs::create_dir_all(&project).unwrap();
    let key = "-fixture-project/session-fixture.jsonl".to_string();
    let rows = [
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"session-fixture","cwd":"/fixture/work","timestamp":"2026-01-01T00:00:00Z","message":{"content":"hello importer"}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"session-fixture","cwd":"/fixture/work","timestamp":"2026-01-01T00:00:01Z","message":{"content":[{"type":"text","text":"hello back"}]}}),
        json!({"type":"system","subtype":"turn_duration","uuid":"leaf","parentUuid":"a1","sessionId":"session-fixture","cwd":"/fixture/work","timestamp":"2026-01-01T00:00:02Z"}),
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

    let chat = core.workspace.doc().chat(&import.chat_id).unwrap().unwrap();
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
            .doc()
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
