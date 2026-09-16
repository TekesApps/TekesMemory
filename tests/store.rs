use serde_json::{Value, json};
use tekes_memory::{common::*, config::SCHEMAS, store::Store};
fn scope() -> Value {
    json!({"kind":"workspace","owner_id":"u","workspace_id":"ws"})
}
fn source() -> Value {
    json!({"host":"test","ref":"test#1","digest":"a".repeat(64)})
}
fn principal(role: &str) -> Value {
    json!({"id":role,"role":role,"owner_id":"u","scopes":[scope()],"tools":SCHEMAS.keys().filter(|k|k.starts_with("memory.")&&(role=="adapter"||**k!="memory.observe")).collect::<Vec<_>>()})
}
fn save(key: &str) -> Value {
    json!({"schema_version":1,"scope":scope(),"kind":"episode","content":"运行项目测试 Rust preferences","sources":[source()],"idempotency_key":key})
}
fn search() -> Value {
    json!({"schema_version":1,"scope":scope(),"query":"","kinds":["episode","fact","procedure"],"budget":{"max_items":10,"max_utf8_bytes":32768,"estimated_tokens":8192}})
}
fn observe(key: &str) -> Value {
    json!({"schema_version":1,"scope":scope(),"idempotency_key":key,"source_event":{"host":"test","host_instance_id":"instance","workspace_id":"ws","thread_id":"t","line_id":"main","event_id":key,"source_seq":9,"source_digest":"b".repeat(64)},"observation":{"type":"turn_outcome","payload":{"turn_id":1,"summary":"visible task completed","sources":[source()],"outcome":"completed"}}})
}
struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("memory.sqlite3"), 90).unwrap();
        store.clock = || 1000;
        Self { _dir: dir, store }
    }
    fn call(&self, name: &str, a: Value) -> Value {
        self.store
            .call(
                &principal(if name == "observe" {
                    "adapter"
                } else {
                    "model"
                }),
                &format!("memory.{name}"),
                &a,
            )
            .unwrap()
    }
    fn error(&self, code: &str, name: &str, a: Value) {
        let p = principal(if name == "observe" {
            "adapter"
        } else {
            "model"
        });
        assert_eq!(
            self.store
                .call(&p, &format!("memory.{name}"), &a)
                .unwrap_err()
                .0,
            code
        )
    }
    fn correct(&self, id: &Value) -> Value {
        json!({"schema_version":1,"scope":scope(),"id":id,"expected_revision":1,"replacement":"updated","sources":[source()],"idempotency_key":"update"})
    }
    fn get(&self, id: &Value) -> Value {
        json!({"schema_version":1,"scope":scope(),"id":id})
    }
    fn forget(&self, id: &Value, rev: i64) -> Value {
        json!({"schema_version":1,"scope":scope(),"ids":[id],"expected_revisions":[rev],"idempotency_key":"forget"})
    }
}
#[test]
fn restart_and_exact_retry() {
    let f = Fixture::new();
    let a = save("key");
    let saved = f.call("save", a.clone());
    let mut second = Store::open(&f._dir.path().join("memory.sqlite3"), 90).unwrap();
    second.clock = || 1000;
    assert_eq!(
        second.call(&principal("model"), "memory.save", &a).unwrap(),
        saved
    );
    assert_eq!(
        f.call("get", f.get(&saved["id"]))["item"]["content"],
        a["content"]
    );
    let mut changed = a;
    changed["content"] = json!("different");
    f.error("idempotency_conflict", "save", changed)
}
#[test]
fn scope_and_operation_isolation() {
    let f = Fixture::new();
    let saved = f.call("save", save("k"));
    let mut s = search();
    s["scope"]["workspace_id"] = json!("other");
    f.error("scope_denied", "search", s);
    let mut p = principal("model");
    p["id"] = json!("other");
    assert_eq!(
        f.store
            .call(
                &p,
                "memory.status",
                &json!({"schema_version":1,"operation_id":saved["operation_id"]})
            )
            .unwrap_err()
            .0,
        "not_found"
    );
    let mut s = search();
    s["scope"]["thread_id"] = json!("t");
    f.error("scope_denied", "search", s)
}
#[test]
fn model_cannot_forge_observation() {
    let f = Fixture::new();
    let mut p = principal("adapter");
    p["role"] = json!("model");
    assert_eq!(
        f.store
            .call(&p, "memory.observe", &observe("k"))
            .unwrap_err()
            .0,
        "scope_denied"
    )
}
#[test]
fn cjk_and_budget() {
    let f = Fixture::new();
    f.call("save", save("k"));
    let mut s = search();
    s["query"] = json!("项目测试");
    assert_eq!(
        f.call("search", s.clone())["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    s["budget"] = json!({"max_items":1,"max_utf8_bytes":256,"estimated_tokens":64});
    assert_eq!(f.call("search", s)["items"], json!([]))
}
#[test]
fn expiry_and_pin() {
    let mut f = Fixture::new();
    let mut a = save("one");
    a["expires_at"] = json!(1010);
    f.call("save", a.clone());
    a["idempotency_key"] = json!("pin");
    a["pinned"] = json!(true);
    let pin = f.call("save", a);
    f.store.clock = || 1011;
    let r = f.call("search", search());
    assert_eq!(r["items"].as_array().unwrap().len(), 1);
    assert_eq!(r["items"][0]["id"], pin["id"])
}
#[test]
fn revision_and_history() {
    let f = Fixture::new();
    let r = f.call("save", save("k"));
    let a = f.correct(&r["id"]);
    f.call("correct", a.clone());
    let mut race = a;
    race["idempotency_key"] = json!("race");
    f.error("revision_conflict", "correct", race);
    let mut g = f.get(&r["id"]);
    g["revision"] = json!(1);
    assert_eq!(f.call("get", g)["item"]["historical"], true)
}
fn fact(key: &str, value: &str) -> Value {
    let mut a = save(key);
    a["kind"] = json!("fact");
    a["fact"] = json!({"subject":"user","predicate":"prefers","value":value});
    a
}
#[test]
fn conflicts_are_not_silently_resolved() {
    let f = Fixture::new();
    f.call("save", fact("a", "Rust"));
    f.call("save", fact("b", "Python"));
    let r = f.call("search", search());
    assert_eq!(r["items"], json!([]));
    assert_eq!(r["conflicts"].as_array().unwrap().len(), 2)
}
#[test]
fn temporal_intervals_disambiguate() {
    let f = Fixture::new();
    let mut a = fact("a", "one");
    a["fact"]["valid_to"] = json!(1000);
    f.call("save", a);
    let mut b = fact("b", "two");
    b["fact"]["valid_from"] = json!(1000);
    f.call("save", b);
    let r = f.call("search", search());
    assert_eq!(r["items"].as_array().unwrap().len(), 1);
    assert_eq!(r["conflicts"], json!([]))
}
#[test]
fn deletion_purges_history_and_index() {
    let f = Fixture::new();
    let mut a = save("k");
    a["content"] = json!("UNIQUE_PRIVATE_CONTENT");
    let r = f.call("save", a);
    f.call("correct", f.correct(&r["id"]));
    f.call("forget", f.forget(&r["id"], 2));
    f.error("not_found", "get", f.get(&r["id"]));
    assert_eq!(f.call("search", search())["items"], json!([]));
    assert_eq!(
        f.store
            .db
            .query_row("SELECT count(*) FROM versions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        f.store
            .db
            .query_row("SELECT count(*) FROM search_index", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    f.store.maintain().unwrap();
    let bytes = std::fs::read(f._dir.path().join("memory.sqlite3")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("UNIQUE_PRIVATE_CONTENT"))
}
#[test]
fn observation_queue_restart_and_business_dedup() {
    let f = Fixture::new();
    let a = f.call("observe", observe("k"));
    let mut s = Store::open(&f._dir.path().join("memory.sqlite3"), 90).unwrap();
    s.clock = || 1000;
    let b = s
        .call(&principal("adapter"), "memory.observe", &observe("another"))
        .unwrap();
    assert_eq!(a["observation_id"], b["observation_id"]);
    assert!(s.maintain().unwrap());
    assert!(!s.maintain().unwrap());
    assert_eq!(
        f.call("search", search())["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let status = s
        .call(
            &principal("adapter"),
            "memory.status",
            &json!({"schema_version":1,"operation_id":a["operation_id"]}),
        )
        .unwrap();
    assert_eq!(status["processing_state"], "completed")
}
#[test]
fn source_revision_invalidates_derived_text() {
    let f = Fixture::new();
    let mut a = observe("k");
    a["observation"]["payload"]["summary"] = json!("OLD_SOURCE");
    f.call("observe", a);
    f.store.maintain().unwrap();
    let mut b = observe("new");
    b["source_event"]["source_digest"] = json!("c".repeat(64));
    b["observation"]["payload"]["summary"] = json!("NEW_SOURCE");
    f.call("observe", b);
    f.store.maintain().unwrap();
    let r = f.call("search", search());
    assert_eq!(r["items"].as_array().unwrap().len(), 1);
    assert!(
        r["items"][0]["content"]
            .as_str()
            .unwrap()
            .contains("NEW_SOURCE")
    );
    let bytes = std::fs::read(f._dir.path().join("memory.sqlite3")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("OLD_SOURCE"))
}
#[test]
fn tombstones_prevent_resurrection() {
    let f = Fixture::new();
    f.call("observe", observe("k"));
    f.store.maintain().unwrap();
    let r = f.call("search", search())["items"][0].clone();
    f.call("forget", f.forget(&r["id"], 1));
    f.call("observe", observe("another"));
    f.store.maintain().unwrap();
    assert_eq!(f.call("search", search())["items"], json!([]));
    let mut a = observe("revision");
    a["source_event"]["source_digest"] = json!("d".repeat(64));
    f.error("source_unavailable", "observe", a)
}
fn invalidation(key: &Value) -> Value {
    let mut a = observe("invalidate");
    a["source_event"]["source_seq"] = json!(10);
    a["source_event"]["source_digest"] = json!("e".repeat(64));
    a["observation"] = json!({"type":"source_invalidated","payload":{"turn_id":1,"summary":"","sources":[source()],"source_keys":[key]}});
    a
}
#[test]
fn invalidation_cancels_pending_job() {
    let f = Fixture::new();
    let a = f.call("observe", observe("k"));
    f.call("observe", invalidation(&a["source_key"]));
    assert!(!f.store.maintain().unwrap());
    assert_eq!(f.call("search", search())["items"], json!([]))
}
#[test]
fn closed_schema_and_redaction() {
    let f = Fixture::new();
    let mut a = save("k");
    a["extra"] = true.into();
    f.error("invalid_argument", "save", a);
    let mut a = save("k");
    a["schema_version"] = json!(2);
    f.error("unsupported_schema_version", "save", a);
    let mut a = save("k");
    a["content"] = json!("api_key=very-secret Bearer abcdefgh");
    f.call("save", a);
    let r = canonical(&f.call("search", search()));
    assert!(!r.contains("very-secret"));
    assert!(!r.contains("abcdefgh"))
}
#[test]
fn rollback_never_publishes_half_operation() {
    let f = Fixture::new();
    f.store.db.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON operations BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    f.error("storage_error", "save", save("k"));
    assert_eq!(f.call("search", search())["items"], json!([]));
    assert_eq!(
        f.store
            .db
            .query_row("SELECT count(*) FROM versions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    f.store
        .db
        .execute_batch("DROP TRIGGER fail_receipt")
        .unwrap();
    f.call("save", save("k"));
}
#[test]
fn cursor_tampering_and_generation() {
    let f = Fixture::new();
    for i in 0..3 {
        f.call("save", save(&i.to_string()));
    }
    let mut a = search();
    a["budget"]["max_items"] = json!(1);
    let first = f.call("search", a.clone());
    a["cursor"] = first["next_cursor"].clone();
    let next = f.call("search", a.clone());
    assert_ne!(first["items"][0]["id"], next["items"][0]["id"]);
    let mut bad = a.clone();
    bad["cursor"] = json!(format!("{}x", a["cursor"].as_str().unwrap()));
    f.error("invalid_argument", "search", bad);
    f.call("save", save("four"));
    f.error("invalid_argument", "search", a)
}
#[test]
fn pending_revision_cancels_old_job() {
    let f = Fixture::new();
    f.call("observe", observe("k"));
    let mut a = observe("new");
    a["source_event"]["source_digest"] = json!("c".repeat(64));
    a["observation"]["payload"]["summary"] = json!("NEW_PENDING");
    f.call("observe", a);
    while f.store.maintain().unwrap() {}
    let r = f.call("search", search());
    assert_eq!(r["items"].as_array().unwrap().len(), 1);
    assert!(
        r["items"][0]["content"]
            .as_str()
            .unwrap()
            .contains("NEW_PENDING")
    )
}
#[test]
fn reused_digest_different_payload_rejects() {
    let f = Fixture::new();
    f.call("observe", observe("k"));
    let mut a = observe("new");
    a["observation"]["payload"]["summary"] = json!("changed");
    f.error("idempotency_conflict", "observe", a)
}
#[test]
fn procedure_is_only_versioned_data() {
    let f = Fixture::new();
    let mut a = save("k");
    a["kind"] = json!("procedure");
    let procedure = json!({"preconditions":["clean tree"],"steps":["run tests"],"success_criteria":"tests pass","verified_environment":"fixture","failure_modes":[]});
    a["procedure"] = procedure.clone();
    let r = f.call("save", a);
    let mut a = f.correct(&r["id"]);
    a["procedure"] = procedure;
    f.call("correct", a);
    assert_eq!(f.call("get", f.get(&r["id"]))["item"]["revision"], 2)
}
#[test]
fn expiration_purges_body_and_history() {
    let mut f = Fixture::new();
    let mut a = save("k");
    a["expires_at"] = json!(1010);
    let r = f.call("save", a);
    f.store.clock = || 1011;
    f.store.maintain().unwrap();
    assert_eq!(
        f.store
            .db
            .query_row("SELECT count(*) FROM versions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    f.call("forget", f.forget(&r["id"], 1));
}
#[test]
fn old_source_invalidation_removes_all_revisions() {
    let f = Fixture::new();
    let a = f.call("observe", observe("k"));
    f.store.maintain().unwrap();
    let record = f.call("search", search())["items"][0].clone();
    f.call("correct", f.correct(&record["id"]));
    f.call("observe", invalidation(&a["source_key"]));
    let mut get = f.get(&record["id"]);
    get["revision"] = json!(1);
    f.error("not_found", "get", get)
}
#[test]
fn expired_conflict_does_not_taint_current_fact() {
    let mut f = Fixture::new();
    let mut a = fact("a", "one");
    a["fact"]["valid_to"] = json!(1100);
    f.call("save", a);
    let mut b = fact("b", "two");
    b["fact"]["valid_from"] = json!(1000);
    f.call("save", b);
    assert_eq!(
        f.call("search", search())["conflicts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    f.store.clock = || 1101;
    let r = f.call("search", search());
    assert_eq!(r["conflicts"], json!([]));
    assert_eq!(r["items"][0]["fact"]["value"], "two")
}
#[test]
fn future_database_version_rejects() {
    let f = Fixture::new();
    f.store.db.execute_batch("PRAGMA user_version=2").unwrap();
    assert!(Store::open(&f._dir.path().join("memory.sqlite3"), 90).is_err())
}
#[test]
fn thread_inheritance_requires_explicit_grants() {
    let f = Fixture::new();
    let saved = f.call("save", save("k"));
    let mut a = search();
    a["scope"]["kind"] = json!("thread");
    a["scope"]["thread_id"] = json!("t");
    assert_eq!(f.call("search", a.clone())["items"][0]["id"], saved["id"]);
    let mut p = principal("model");
    p["scopes"] = json!([a["scope"]]);
    assert_eq!(
        f.store.call(&p, "memory.search", &a).unwrap()["items"],
        json!([])
    )
}
#[test]
fn strict_wire_rejects_duplicates_and_nonfinite_numbers() {
    assert!(decode(br#"{"a":1,"a":2}"#).is_err());
    assert!(decode(br#"{"a":1.5}"#).is_ok());
    assert!(validate(&json!(1.5), &json!({"type":"integer"})).is_err());
    assert!(decode(br#"{"a":NaN}"#).is_err());
    assert!(decode(br#"{"a":1}"#).is_ok())
}
#[test]
fn rust_schema_assets_are_closed() {
    for (n, s) in SCHEMAS.iter() {
        assert_eq!(s["additionalProperties"], false, "{n}");
    }
    assert_eq!(
        SCHEMAS.keys().filter(|n| n.starts_with("memory.")).count(),
        7
    )
}

#[test]
fn concurrent_connections_have_one_revision_winner() {
    let f = Fixture::new();
    let saved = f.call("save", save("seed"));
    let correction = f.correct(&saved["id"]);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads = (0..2)
        .map(|i| {
            let path = f._dir.path().join("memory.sqlite3");
            let barrier = barrier.clone();
            let mut correction = correction.clone();
            correction["idempotency_key"] = json!(format!("race-{i}"));
            std::thread::spawn(move || {
                let mut store = Store::open(&path, 90).unwrap();
                store.clock = || 1000;
                barrier.wait();
                store
                    .call(&principal("model"), "memory.correct", &correction)
                    .map(|r| r["revision"].as_i64().unwrap())
                    .map_err(|e| e.0)
            })
        })
        .collect::<Vec<_>>();
    let results = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|r| **r == Ok(2)).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == Err("revision_conflict".into()))
            .count(),
        1
    );
}

#[test]
fn failed_commit_rolls_back_and_releases_transaction() {
    let f = Fixture::new();
    f.store.db.execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_commit AFTER INSERT ON operations BEGIN INSERT INTO child VALUES(1); END;").unwrap();
    f.error("storage_error", "save", save("key"));
    assert!(f.store.db.is_autocommit());
    assert_eq!(f.call("search", search())["items"], json!([]));
    f.store
        .db
        .execute_batch("DROP TRIGGER fail_commit")
        .unwrap();
    f.call("save", save("key"));
}

#[test]
fn background_processing_failure_is_bounded_and_atomic() {
    let f = Fixture::new();
    let accepted = f.call("observe", observe("event"));
    f.store.db.execute_batch("CREATE TRIGGER fail_processing BEFORE INSERT ON records BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    for _ in 0..5 {
        f.store.db.execute("UPDATE jobs SET next_at=0", []).unwrap();
        assert!(f.store.maintain().unwrap());
        assert_eq!(f.call("search", search())["items"], json!([]));
    }
    let status = f
        .store
        .call(
            &principal("adapter"),
            "memory.status",
            &json!({"schema_version":1,"operation_id":accepted["operation_id"]}),
        )
        .unwrap();
    assert_eq!(status["processing_state"], "failed");
    assert_eq!(status["error"], "processing_failed");
    assert!(!f.store.maintain().unwrap());
}
