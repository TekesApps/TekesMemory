//! Opt-in Kernel integration fixture: actual Rust service + extension, no Python.
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use tekes_memory::{client::Client, common::*, config};
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn main() {
    unsafe { libc::umask(0o077) };
    let root = PathBuf::from(std::env::args().nth(1).expect("root"));
    let binary = PathBuf::from(std::env::args().nth(2).expect("Rust service binary"));
    let paths = config::setup(&root.join("real-memory"), "ws", &root, 0).unwrap();
    let cp = PathBuf::from(paths["service_config"].as_str().unwrap());
    let ready = root.join("memory-ready.json");
    let mut service = ChildGuard(
        Command::new(&binary)
            .args(["serve", "--config"])
            .arg(&cp)
            .arg("--ready-file")
            .arg(&ready)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..200 {
        if ready.exists() {
            break;
        }
        assert!(service.0.try_wait().unwrap().is_none(), "service exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    let endpoint = private_json(&ready).unwrap()["endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    let ap = PathBuf::from(paths["adapter_config"].as_str().unwrap());
    let mut adapter = private_json(&ap).unwrap();
    adapter["endpoint"] = json!(endpoint);
    adapter["retrieval"]["estimated_tokens"] = json!(8192);
    std::fs::write(&ap, canonical(&adapter)).unwrap();
    for e in std::fs::read_dir(cp.parent().unwrap().join("hooks")).unwrap() {
        let path = e.unwrap().path();
        let mut hook = private_json(&path).unwrap();
        hook["argv"] = json!([binary, "kernel", "--config", ap]);
        std::fs::write(path, format!("{}\n", canonical(&hook))).unwrap();
    }
    let c = private_json(&cp).unwrap();
    let scope = &c["principals"][0]["scopes"][0];
    let credential = private_json(&cp.parent().unwrap().join("model-credential.json")).unwrap();
    Client::open(&endpoint,text(&credential,"token").unwrap(),Duration::from_secs(5)).unwrap().call("memory.save",json!({"schema_version":1,"scope":scope,"kind":"episode","content":"first second LIFECYCLE_CONTEXT_MARKER","sources":[{"host":"fixture","ref":"seed","digest":"a".repeat(64)}],"idempotency_key":"seed"})).unwrap();
    println!("{}", canonical(&json!({"hook_root":cp.parent().unwrap()})));
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "verify");
    let db = rusqlite::Connection::open(cp.parent().unwrap().join("data/memory.sqlite3")).unwrap();
    let mut records = vec![];
    for _ in 0..100 {
        let mut stmt = db
            .prepare("SELECT payload FROM observations WHERE state='completed'")
            .unwrap();
        records = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str::<Value>(&r.unwrap()).unwrap())
            .collect::<Vec<_>>();
        if records.iter().any(|r| r["type"] == "turn_outcome") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        records
            .iter()
            .filter(|r| r["type"] == "turn_outcome")
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| r["type"] == "tool_result")
            .count(),
        1
    );
    assert_eq!(
        records.iter().find(|r| r["type"] == "tool_result").unwrap()["payload"]["outcome"],
        "ok"
    );
    assert!(!canonical(&json!(records)).contains("Still thinking"));
    println!(
        "{}",
        canonical(
            &json!({"verified":true,"types":records.iter().map(|r|r["type"].clone()).collect::<Vec<_>>()})
        )
    );
    std::io::stdout().flush().unwrap();
}
