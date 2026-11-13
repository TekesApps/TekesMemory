use serde_json::{Value, json};
use std::{
    fs,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tekes_memory::{common::*, config::SCHEMAS, store::Store};
struct Fixture {
    dir: tempfile::TempDir,
    child: Child,
    config: Value,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("control.json"), "{}").unwrap();
        write_private(
            &dir.path().join("credential.json"),
            br#"{"token":"synthetic-provider-token"}"#,
        )
        .unwrap();
        let child =
            Command::new(std::env::var("MEMORY_TEST_PYTHON").unwrap_or_else(|_| "python3".into()))
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/support/security_provider.py"
                ))
                .arg(dir.path())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let ready = loop {
            if let Ok(raw) = fs::read(dir.path().join("ready.json")) {
                if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                    break v;
                }
            }
            assert!(
                Instant::now() < deadline,
                "Python security fixture failed to start (requires cryptography)"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let config = json!({"version":1,"endpoint":format!("http://127.0.0.1:{}/security/v1",ready["port"]),
            "credential_file":dir.path().join("credential.json"),"provider_id":"fixture","profile":"opaque-v1",
            "policy_version":"p1","purpose":"synthetic-test","timeout_ms":2000});
        Self { dir, child, config }
    }
    fn control(&self, value: Value) {
        fs::write(self.dir.path().join("control.json"), canonical(&value)).unwrap();
    }
    fn open(&self) -> Store {
        let mut store = Store::open_secure(
            &self.dir.path().join("memory.sqlite3"),
            90,
            Some(&self.config),
        )
        .unwrap();
        store.clock = || 1000;
        store
    }
}
fn scope() -> Value {
    json!({"kind":"workspace","owner_id":"opaque-owner","workspace_id":"w"})
}
fn p() -> Value {
    json!({"id":"client","role":"adapter","owner_id":"opaque-owner","scopes":[scope()],"tools":SCHEMAS.keys().filter(|k|k.starts_with("memory.")).collect::<Vec<_>>()})
}
fn content() -> String {
    format!(
        "v1.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\nh{}",
        "a".repeat(64)
    )
}
fn save(key: &str) -> Value {
    json!({"schema_version":1,"scope":scope(),"kind":"episode","content":content(),"sources":[{"host":"synthetic","ref":"opaque-ref","digest":"b".repeat(64),"source_key":"source-1"}],"idempotency_key":key,"pinned":true})
}
fn get(id: &Value) -> Value {
    json!({"schema_version":1,"scope":scope(),"id":id})
}
fn search() -> Value {
    json!({"schema_version":1,"scope":scope(),"query":format!("h{}","a".repeat(64)),"kinds":["episode"],"budget":{"max_items":10,"max_utf8_bytes":32768,"estimated_tokens":8192}})
}
#[test]
fn opaque_roundtrip_disk_scope_audit_and_restart() {
    let f = Fixture::new();
    let store = f.open();
    let saved = store.call(&p(), "memory.save", &save("s")).unwrap();
    assert_eq!(
        store.call(&p(), "memory.get", &get(&saved["id"])).unwrap()["item"]["content"],
        content()
    );
    assert_eq!(
        store.call(&p(), "memory.search", &search()).unwrap()["items"][0]["content"],
        content()
    );
    let doc: String = store
        .db
        .query_row("SELECT doc FROM records", [], |r| r.get(0))
        .unwrap();
    assert!(!doc.contains("v1.AAAA"));
    assert!(doc.contains("key_version"));
    let index: String = store
        .db
        .query_row("SELECT words FROM search_index", [], |r| r.get(0))
        .unwrap();
    assert_eq!(index, format!("h{}", "a".repeat(64)));
    let mut denied = p();
    denied["owner_id"] = json!("someone-else");
    assert_eq!(
        store
            .call(&denied, "memory.get", &get(&saved["id"]))
            .unwrap_err()
            .0,
        "scope_denied"
    );
    assert!(
        store.security_status().unwrap()["audit_pending"]
            .as_i64()
            .unwrap()
            > 0
    );
    f.control(json!({"audit_fail":true}));
    assert_eq!(store.deliver_audit().unwrap(), 0);
    f.control(json!({"lose_audit_response":true}));
    assert_eq!(store.deliver_audit().unwrap(), 0);
    f.control(json!({}));
    assert!(store.deliver_audit().unwrap() > 0);
    assert_eq!(store.security_status().unwrap()["audit_pending"], 0);
    let audit = rusqlite::Connection::open(f.dir.path().join("provider.sqlite3")).unwrap();
    let events: Vec<String> = audit
        .prepare("SELECT event FROM audit")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(events.iter().any(|e| e.contains("denied_or_failed")));
    assert!(
        events
            .iter()
            .all(|e| !e.contains("opaque-ref") && !e.contains("v1.AAAA") && !e.contains("token"))
    );
    drop(store);
    assert_eq!(
        Store::open(&f.dir.path().join("memory.sqlite3"), 90)
            .err()
            .unwrap()
            .0,
        "security_provider_required"
    );
    let store = f.open();
    assert_eq!(
        store.call(&p(), "memory.get", &get(&saved["id"])).unwrap()["item"]["content"],
        content()
    );
}
#[test]
fn revocation_deadline_and_idempotent_effect_remain_enforced() {
    let f = Fixture::new();
    f.control(json!({"deadline":1002}));
    let mut store = f.open();
    let key = "d".repeat(64);
    let a = save("s");
    let saved = store
        .call_with_effect(&p(), "memory.save", &a, Some(&key))
        .unwrap();
    let generation = store.security_status().unwrap()["generation"].clone();
    assert_eq!(
        store
            .call_with_effect(&p(), "memory.save", &a, Some(&key))
            .unwrap(),
        saved
    );
    assert_eq!(store.security_status().unwrap()["generation"], generation);
    assert_eq!(
        store.call(&p(), "memory.get", &get(&saved["id"])).unwrap()["item"]["expires_at"],
        1002
    );
    f.control(
        json!({"deny":["memory.get","memory.search","memory.status","memory.reconcile_effect"]}),
    );
    assert_eq!(
        store.reconcile_effect(&p(), &key).unwrap_err().0,
        "security_denied"
    );
    assert_eq!(
        store
            .call(&p(), "memory.get", &get(&saved["id"]))
            .unwrap_err()
            .0,
        "security_denied"
    );
    f.control(json!({}));
    store.clock = || 1003;
    assert_eq!(
        store
            .call(&p(), "memory.get", &get(&saved["id"]))
            .unwrap_err()
            .0,
        "not_found"
    );
    assert!(
        store.call(&p(), "memory.search", &search()).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    store.maintain().unwrap();
    let n: i64 = store
        .db
        .query_row("SELECT count(*) FROM versions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}
#[test]
fn rotation_interruption_unknown_key_and_tamper_fail_closed() {
    let f = Fixture::new();
    let store = f.open();
    let saved = store.call(&p(), "memory.save", &save("s")).unwrap();
    f.control(json!({"write_key":"k2","read_keys":["k1","k2"]}));
    assert_eq!(store.rewrap_batch(1).unwrap(), 1);
    drop(store);
    let store = f.open();
    assert_eq!(
        store.call(&p(), "memory.get", &get(&saved["id"])).unwrap()["item"]["content"],
        content()
    );
    while store.rewrap_batch(1).unwrap() > 0 {}
    f.control(json!({"write_key":"k2","read_keys":["k2"]}));
    assert!(store.call(&p(), "memory.get", &get(&saved["id"])).is_ok());
    f.control(json!({"read_keys":[],"write_key":"k3"}));
    assert_eq!(
        store.rewrap_batch(1).unwrap_err().0,
        "security_unknown_key_version"
    );
    assert_eq!(
        store
            .call(&p(), "memory.get", &get(&saved["id"]))
            .unwrap_err()
            .0,
        "security_unknown_key_version"
    );
    f.control(json!({"read_keys":["k2"],"key_unavailable":true}));
    assert_eq!(
        store
            .call(&p(), "memory.get", &get(&saved["id"]))
            .unwrap_err()
            .0,
        "security_key_unavailable"
    );
    f.control(json!({"read_keys":["k2"]}));
    store
        .db
        .execute(
            "UPDATE records SET doc=json_set(doc,'$.context.scope.owner_id','wrong')",
            [],
        )
        .unwrap();
    assert_eq!(
        store
            .call(&p(), "memory.get", &get(&saved["id"]))
            .unwrap_err()
            .0,
        "security_payload_invalid"
    );
}
#[test]
fn restore_and_ambiguous_reservation_quarantine() {
    let f = Fixture::new();
    let store = f.open();
    let saved = store.call(&p(), "memory.save", &save("s")).unwrap();
    store
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    fs::copy(
        f.dir.path().join("memory.sqlite3"),
        f.dir.path().join("old.sqlite3"),
    )
    .unwrap();
    let forget = json!({"schema_version":1,"scope":scope(),"ids":[saved["id"]],"expected_revisions":[1],"idempotency_key":"f"});
    store.call(&p(), "memory.forget", &forget).unwrap();
    drop(store);
    assert_eq!(
        Store::open_secure(&f.dir.path().join("old.sqlite3"), 90, Some(&f.config))
            .err()
            .unwrap()
            .0,
        "security_restore_required"
    );
    let store = f.open();
    assert_eq!(
        store.call(&p(), "memory.save", &save("new")).unwrap_err().0,
        "source_unavailable"
    );
    let mut new = save("new2");
    new["sources"][0]["source_key"] = json!("source-2");
    f.control(json!({"lose_reserve_response":true}));
    assert_eq!(
        store.call(&p(), "memory.save", &new).unwrap_err().0,
        "security_unavailable"
    );
    f.control(json!({}));
    assert_eq!(
        store.call(&p(), "memory.search", &search()).unwrap_err().0,
        "security_restore_required"
    );
}
fn observation(key: &str) -> Value {
    json!({"schema_version":1,"scope":scope(),"idempotency_key":key,"source_event":{"host":"synthetic","host_instance_id":"i","workspace_id":"w","thread_id":"t","line_id":"main","event_id":key,"source_seq":1,"source_digest":"c".repeat(64)},"observation":{"type":"turn_outcome","payload":{"turn_id":1,"summary":content(),"sources":save("x")["sources"],"outcome":"completed"}}})
}
#[test]
fn queued_observation_rechecks_policy_and_copies_opaque_content() {
    let f = Fixture::new();
    let store = f.open();
    let accepted = store
        .call(&p(), "memory.observe", &observation("o"))
        .unwrap();
    let raw: String = store
        .db
        .query_row("SELECT payload FROM observations", [], |r| r.get(0))
        .unwrap();
    assert!(!raw.contains("v1.AAAA"));
    drop(store);
    let store = f.open();
    f.control(json!({"deny":["derive"]}));
    assert_eq!(store.maintain().unwrap_err().0, "security_denied");
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    f.control(json!({}));
    assert!(store.maintain().unwrap());
    let result = store.call(&p(), "memory.search", &search()).unwrap();
    assert_eq!(result["items"][0]["content"], content());
    let state = store
        .call(
            &p(),
            "memory.status",
            &json!({"schema_version":1,"operation_id":accepted["operation_id"]}),
        )
        .unwrap();
    assert_eq!(state["processing_state"], "completed");
    f.control(json!({"deny":["memory.status"]}));
    assert_eq!(
        store
            .call(
                &p(),
                "memory.status",
                &json!({"schema_version":1,"operation_id":accepted["operation_id"]})
            )
            .unwrap_err()
            .0,
        "security_denied"
    );
}
#[test]
fn correction_historical_versions_plaintext_rejection_and_new_effect_watermark() {
    let f = Fixture::new();
    let store = f.open();
    let original = save("s");
    let saved = store.call(&p(), "memory.save", &original).unwrap();
    let generation = store.security_status().unwrap()["generation"]
        .as_i64()
        .unwrap();
    let effect = "e".repeat(64);
    store
        .call_with_effect(&p(), "memory.save", &original, Some(&effect))
        .unwrap();
    assert_eq!(
        store.security_status().unwrap()["generation"],
        generation + 1
    );
    let correction = json!({"schema_version":1,"scope":scope(),"id":saved["id"],"expected_revision":1,"replacement":content(),"sources":original["sources"],"idempotency_key":"c"});
    store.call(&p(), "memory.correct", &correction).unwrap();
    let mut historical = get(&saved["id"]);
    historical["revision"] = json!(1);
    assert_eq!(
        store.call(&p(), "memory.get", &historical).unwrap()["item"]["content"],
        content()
    );
    let mut plaintext = save("bad");
    plaintext["content"] = json!("SYNTHETIC-PLAINTEXT-MUST-NOT-PERSIST");
    assert_eq!(
        store.call(&p(), "memory.save", &plaintext).unwrap_err().0,
        "security_unknown_key_version"
    );
    store
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let marker = b"SYNTHETIC-PLAINTEXT-MUST-NOT-PERSIST";
    assert!(
        !fs::read(f.dir.path().join("memory.sqlite3"))
            .unwrap()
            .windows(marker.len())
            .any(|w| w == marker)
    );
    f.control(json!({"deny":["memory.reconcile_effect"]}));
    assert!(store.reconcile_effect(&p(), &effect).is_err());
    let count:i64=store.db.query_row("SELECT count(*) FROM security_audit WHERE json_extract(event,'$.operation')='memory.reconcile_effect' AND json_extract(event,'$.outcome')='denied_or_failed'",[],|r|r.get(0)).unwrap();
    assert_eq!(count, 1);
    f.control(json!({}));
    store.db.execute("UPDATE records SET doc=(SELECT doc FROM versions WHERE versions.id=records.id AND revision=1)",[]).unwrap();
    assert_eq!(
        store
            .call(&p(), "memory.get", &get(&saved["id"]))
            .unwrap_err()
            .0,
        "security_payload_invalid"
    );
    assert_eq!(
        store.call(&p(), "memory.search", &search()).unwrap_err().0,
        "security_payload_invalid"
    );
}
#[test]
fn pending_job_source_block_removal_advances_restore_watermark() {
    let f = Fixture::new();
    let store = f.open();
    store
        .call(&p(), "memory.observe", &observation("o"))
        .unwrap();
    // The source deletion is independently known before queued publication.
    store
        .db
        .execute(
            "INSERT INTO tombstones VALUES(?,?)",
            rusqlite::params![canonical(&scope()), "source-1"],
        )
        .unwrap();
    let before = store.security_status().unwrap()["generation"]
        .as_i64()
        .unwrap();
    store.maintain().unwrap();
    assert_eq!(store.security_status().unwrap()["generation"], before + 1);
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .db
            .query_row("SELECT state FROM observations", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "deleted"
    );
}
#[test]
fn public_http_entry_loads_provider_and_preserves_legacy_opaque_wire() {
    let f = Fixture::new();
    let threads = f.dir.path().join("threads");
    fs::create_dir(&threads).unwrap();
    let paths =
        tekes_memory::config::setup(&f.dir.path().join("service"), "w", &threads, 0).unwrap();
    let path = std::path::Path::new(paths["service_config"].as_str().unwrap());
    let mut config = private_json(path).unwrap();
    config["security_provider"] = f.config.clone();
    let service_scope = config["principals"][0]["scopes"][0].clone();
    fs::write(path, canonical(&config)).unwrap();
    let ready = f.dir.path().join("service-ready.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tekes-memory"))
        .args(["serve", "--config"])
        .arg(path)
        .arg("--ready-file")
        .arg(&ready)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("service startup failed: {status}");
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let endpoint: Value = serde_json::from_slice(&fs::read(&ready).unwrap()).unwrap();
    let credentials = private_json(&f.dir.path().join("service/model-credential.json")).unwrap();
    let mut client = tekes_memory::client::Client::open(
        endpoint["endpoint"].as_str().unwrap(),
        credentials["token"].as_str().unwrap(),
        Duration::from_secs(3),
    )
    .unwrap();
    let mut a = save("http");
    a["scope"] = service_scope.clone();
    let saved = client.call("memory.save", a).unwrap();
    let mut request = get(&saved["id"]);
    request["scope"] = service_scope;
    let returned = client.call("memory.get", request).unwrap();
    let original_tools = config["principals"][0]["tools"].clone();
    config["principals"][0]["tools"] = json!(["memory.get", "memory.search", "memory.status"]);
    fs::write(path, canonical(&config)).unwrap();
    assert_eq!(
        client
            .call(
                "memory.reconcile_effect",
                json!({"schema_version":1,"idempotencyKey":"a".repeat(64)})
            )
            .unwrap_err()
            .0,
        "scope_denied"
    );
    config["principals"][0]["tools"] = original_tools;
    fs::write(path, canonical(&config)).unwrap();
    assert_eq!(
        client
            .call(
                "memory.reconcile_effect",
                json!({"schema_version":1,"idempotencyKey":"a".repeat(64),"unexpected":true})
            )
            .unwrap_err()
            .0,
        "invalid_argument"
    );
    let audit = rusqlite::Connection::open(f.dir.path().join("provider.sqlite3")).unwrap();
    let rejected:i64=audit.query_row("SELECT count(*) FROM audit WHERE json_extract(event,'$.operation')='memory.reconcile_effect' AND json_extract(event,'$.outcome')='denied_or_failed'",[],|r|r.get(0)).unwrap();
    assert_eq!(rejected, 2);
    child.kill().unwrap();
    child.wait().unwrap();
    f.control(json!({"write_key":"k2","read_keys":["k1","k2"]}));
    let maintenance = Command::new(env!("CARGO_BIN_EXE_tekes-memory"))
        .args(["security-maintenance", "--config"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        maintenance.status.success(),
        "{}",
        String::from_utf8_lossy(&maintenance.stderr)
    );
    let report: Value = serde_json::from_slice(&maintenance.stdout).unwrap();
    assert!(report["rows_rewrapped"].as_u64().unwrap() > 0);
    assert_eq!(returned["item"]["content"], content());
}
#[test]
fn legacy_opaque_migration_is_explicit_atomic_and_preserves_identity() {
    let f = Fixture::new();
    let path = f.dir.path().join("memory.sqlite3");
    let ordinary = Store::open(&path, 90).unwrap();
    let saved = ordinary
        .call_with_effect(&p(), "memory.save", &save("legacy"), Some(&"f".repeat(64)))
        .unwrap();
    drop(ordinary);
    assert_eq!(
        Store::open_secure(&path, 90, Some(&f.config))
            .err()
            .unwrap()
            .0,
        "security_migration_required"
    );
    let store = Store::migrate_opaque(&path, 90, &f.config).unwrap();
    let value = store.call(&p(), "memory.get", &get(&saved["id"])).unwrap();
    assert_eq!(value["item"]["content"], content());
    assert_eq!(value["item"]["scope"], scope());
    assert_eq!(value["item"]["revision"], 1);
    assert_eq!(
        store.reconcile_effect(&p(), &"f".repeat(64)).unwrap()["status"],
        "confirmed"
    );
    assert_eq!(
        store.call(&p(), "memory.search", &search()).unwrap()["items"][0]["id"],
        saved["id"]
    );
    drop(store);
    assert_eq!(
        Store::open(&path, 90).err().unwrap().0,
        "security_provider_required"
    );
    let bad = f.dir.path().join("bad.sqlite3");
    let ordinary = Store::open(&bad, 90).unwrap();
    ordinary
        .call(&p(), "memory.save", &save("valid-before-invalid"))
        .unwrap();
    let mut a = save("clear");
    a["content"] = json!("SYNTHETIC PLAINTEXT");
    ordinary.call(&p(), "memory.save", &a).unwrap();
    drop(ordinary);
    assert!(Store::migrate_opaque(&bad, 90, &f.config).is_err());
    let ordinary = Store::open(&bad, 90).unwrap();
    let doc: String = ordinary
        .db
        .query_row(
            "SELECT doc FROM records WHERE doc LIKE '%SYNTHETIC PLAINTEXT%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(doc.contains("SYNTHETIC PLAINTEXT"));
    assert!(!doc.contains("security_v"));
}
#[test]
fn successful_reads_require_independent_audit_before_snapshot_restore() {
    let f = Fixture::new();
    let store = f.open();
    let saved = store.call(&p(), "memory.save", &save("s")).unwrap();
    store
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    fs::copy(
        f.dir.path().join("memory.sqlite3"),
        f.dir.path().join("snapshot.sqlite3"),
    )
    .unwrap();
    f.control(json!({"audit_fail":true}));
    assert_eq!(
        store
            .call(&p(), "memory.get", &get(&saved["id"]))
            .unwrap_err()
            .0,
        "security_unavailable"
    );
    f.control(json!({}));
    let restored =
        Store::open_secure(&f.dir.path().join("snapshot.sqlite3"), 90, Some(&f.config)).unwrap();
    // The failed read disclosed nothing; a successful new read must be independently recorded.
    let mut restored = restored;
    restored.clock = || 1000;
    assert!(
        restored
            .call(&p(), "memory.get", &get(&saved["id"]))
            .is_ok()
    );
    let audit = rusqlite::Connection::open(f.dir.path().join("provider.sqlite3")).unwrap();
    let reads:i64=audit.query_row("SELECT count(*) FROM audit WHERE json_extract(event,'$.operation')='memory.get' AND json_extract(event,'$.outcome')='success'",[],|r|r.get(0)).unwrap();
    assert_eq!(reads, 1);
}
#[test]
fn missing_effect_is_gated_and_audited_and_replay_audit_cannot_disappear() {
    let f = Fixture::new();
    let store = f.open();
    let missing = "0".repeat(64);
    assert_eq!(
        store.reconcile_effect(&p(), &missing).unwrap()["status"],
        "not_found"
    );
    f.control(json!({"deny":["memory.reconcile_effect"]}));
    assert_eq!(
        store.reconcile_effect(&p(), &missing).unwrap_err().0,
        "security_denied"
    );
    f.control(json!({"audit_fail":true}));
    assert_eq!(
        store.reconcile_effect(&p(), &missing).unwrap_err().0,
        "security_unavailable"
    );
    let saved = store
        .call(&p(), "memory.save", &save("pending-write"))
        .unwrap();
    assert_eq!(saved["security"]["audit_state"], "pending");
    // A retry is a read of the already committed operation. Independent acceptance is
    // required, but failure never erases or reports not_found for the original mutation.
    assert_eq!(
        store
            .call(&p(), "memory.save", &save("pending-write"))
            .unwrap_err()
            .0,
        "security_unavailable"
    );
    assert_eq!(
        store
            .db
            .query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    f.control(json!({"lose_reserve_response":true}));
    let mut another = save("ambiguous");
    another["sources"][0]["source_key"] = json!("source-2");
    assert!(store.call(&p(), "memory.save", &another).is_err());
    f.control(json!({}));
    assert_eq!(
        store.reconcile_effect(&p(), &missing).unwrap_err().0,
        "security_restore_required"
    );
}
#[test]
fn maintenance_denials_and_migration_failure_are_independently_audited() {
    let f = Fixture::new();
    let store = f.open();
    store.call(&p(), "memory.save", &save("s")).unwrap();
    f.control(json!({"deny":["rewrap"]}));
    assert_eq!(store.rewrap_batch(1).unwrap_err().0, "security_denied");
    f.control(json!({"write_key":"k2","read_keys":[]}));
    assert_eq!(
        store.rewrap_batch(1).unwrap_err().0,
        "security_unknown_key_version"
    );
    f.control(json!({"deny":["migrate"]}));
    let ordinary = f.dir.path().join("legacy-denied.sqlite3");
    assert_eq!(
        Store::migrate_opaque(&ordinary, 90, &f.config)
            .err()
            .unwrap()
            .0,
        "security_denied"
    );
    f.control(json!({}));
    let bad = f.dir.path().join("legacy-invalid.sqlite3");
    let ordinary = Store::open(&bad, 90).unwrap();
    let mut args = save("plain");
    args["content"] = json!("synthetic plaintext");
    ordinary.call(&p(), "memory.save", &args).unwrap();
    drop(ordinary);
    assert!(Store::migrate_opaque(&bad, 90, &f.config).is_err());
    let audit = rusqlite::Connection::open(f.dir.path().join("provider.sqlite3")).unwrap();
    for action in ["rewrap", "migrate"] {
        let n:i64=audit.query_row("SELECT count(*) FROM audit WHERE json_extract(event,'$.operation')=? AND json_extract(event,'$.outcome')='denied_or_failed'",[action],|r|r.get(0)).unwrap();
        assert_eq!(n, 2, "{action}");
    }
}
