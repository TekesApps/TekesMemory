//! Bulk fixture load isolates retrieval latency from one-fsync-per-operation write latency.
use rusqlite::params;
use serde_json::json;
use tekes_memory::{
    common::*,
    store::{Store, words},
};
fn main() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("memory.sqlite3"), 90).unwrap();
    let scope = json!({"kind":"workspace","owner_id":"bench","workspace_id":"ws"});
    let p = json!({"id":"bench","role":"model","owner_id":"bench","scopes":[scope],"tools":["memory.search"]});
    let start = std::time::Instant::now();
    store.db.execute_batch("BEGIN IMMEDIATE").unwrap();
    for i in 0..15000 {
        let fact = i >= 10000;
        let mut record = json!({"id":format!("record-{i:05}"),"scope":scope,"kind":if fact{"fact"}else{"episode"},"revision":1,"status":"active","content":format!("项目测试 project tests historical record {i}"),"sources":[{"host":"fixture","ref":format!("seed-{i}"),"digest":"a".repeat(64)}],"observed_at":now(),"verified_at":null,"pinned":false,"expires_at":null});
        let key = if fact {
            record["fact"] =
                json!({"subject":format!("project-{i}"),"predicate":"runner","value":"cargo test"});
            Some(digest(
                &json!({"subject":record["fact"]["subject"],"predicate":"runner","environment":null}),
            ))
        } else {
            None
        };
        let doc = canonical(&record);
        store
            .db
            .execute(
                "INSERT INTO records VALUES(?,?,?,?,?,?,?,?)",
                params![
                    record["id"].as_str(),
                    canonical(&scope),
                    record["kind"].as_str(),
                    1,
                    "active",
                    None::<i64>,
                    doc,
                    key
                ],
            )
            .unwrap();
        store
            .db
            .execute(
                "INSERT INTO versions VALUES(?,?,?)",
                params![record["id"].as_str(), 1, doc],
            )
            .unwrap();
        store
            .db
            .execute(
                "INSERT INTO search_index VALUES(?,?)",
                params![
                    record["id"].as_str(),
                    words(&format!(
                        "{} {}",
                        record["content"].as_str().unwrap(),
                        canonical(&record["fact"])
                    ))
                    .join(" ")
                ],
            )
            .unwrap();
    }
    store.db.execute_batch("COMMIT").unwrap();
    let load = start.elapsed().as_secs_f64();
    let args = json!({"schema_version":1,"scope":scope,"query":"项目测试 project tests","kinds":["episode","fact","procedure"],"budget":{"max_items":10,"max_utf8_bytes":32768,"estimated_tokens":8192}});
    let mut timings = vec![];
    for _ in 0..40 {
        let start = std::time::Instant::now();
        let r = store.call(&p, "memory.search", &args).unwrap();
        assert!(!r["items"].as_array().unwrap().is_empty());
        timings.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    timings.sort_by(f64::total_cmp);
    println!("{}",serde_json::to_string_pretty(&json!({"episodes":10000,"facts":5000,"queries":40,"fixture_load_seconds":load,"warm_p50_ms":(timings[19]+timings[20])/2.0,"warm_p95_ms":timings[37],"binary":"Rust release"})).unwrap());
}
