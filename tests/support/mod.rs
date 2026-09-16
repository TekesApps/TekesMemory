#![allow(dead_code)]
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tekes_memory::{client::Client, common::*, config};
pub struct Service {
    pub root: tempfile::TempDir,
    pub paths: Value,
    pub url: String,
    pub scope: Value,
    pub child: Child,
}
impl Service {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let threads = root.path().join("threads");
        std::fs::create_dir(&threads).unwrap();
        let paths = config::setup(&root.path().join("config"), "ws", &threads, 0).unwrap();
        let scope = json!({"kind":"workspace","owner_id":format!("local-{}",unsafe{libc::getuid()}),"workspace_id":"ws"});
        let (child, url) = Self::start(root.path(), &paths);
        let mut s = Self {
            root,
            paths,
            url,
            scope,
            child,
        };
        s.update_adapter();
        s
    }
    pub fn start(root: &Path, paths: &Value) -> (Child, String) {
        let ready = root.join("ready.json");
        let _ = std::fs::remove_file(&ready);
        let mut child = Command::new(env!("CARGO_BIN_EXE_tekes-memory"))
            .args([
                "serve",
                "--config",
                paths["service_config"].as_str().unwrap(),
                "--ready-file",
            ])
            .arg(&ready)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        for _ in 0..200 {
            if ready.exists() {
                let r: Value = serde_json::from_slice(&std::fs::read(&ready).unwrap()).unwrap();
                return (child, r["endpoint"].as_str().unwrap().into());
            }
            if child.try_wait().unwrap().is_some() {
                let mut e = String::new();
                child.stderr.take().unwrap().read_to_string(&mut e).unwrap();
                panic!("service exited: {e}")
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("service startup timed out")
    }
    pub fn update_adapter(&mut self) {
        let path = self.adapter_path();
        let mut c = private_json(&path).unwrap();
        c["endpoint"] = json!(self.url);
        c["retrieval"]["estimated_tokens"] = json!(8192);
        std::fs::write(path, canonical(&c)).unwrap();
    }
    pub fn restart(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        let (c, u) = Self::start(self.root.path(), &self.paths);
        self.child = c;
        self.url = u;
        self.update_adapter();
    }
    pub fn credential(&self, role: &str) -> PathBuf {
        self.root
            .path()
            .join(format!("config/{role}-credential.json"))
    }
    pub fn token(&self, role: &str) -> String {
        private_json(&self.credential(role)).unwrap()["token"]
            .as_str()
            .unwrap()
            .into()
    }
    pub fn client(&self, role: &str) -> Client {
        Client::open(&self.url, &self.token(role), Duration::from_secs(5)).unwrap()
    }
    pub fn adapter_path(&self) -> PathBuf {
        PathBuf::from(self.paths["adapter_config"].as_str().unwrap())
    }
    pub fn save(&self) -> Value {
        self.client("model").call("memory.save",json!({"schema_version":1,"scope":self.scope,"kind":"episode","content":"Project tests use cargo test 项目测试","sources":[{"host":"test","ref":"seed","digest":"a".repeat(64)}],"idempotency_key":"seed"})).unwrap()
    }
    pub fn search(&self) -> Value {
        self.client("model").call("memory.search",json!({"schema_version":1,"scope":self.scope,"query":"","kinds":["episode","fact","procedure"],"budget":{"max_items":20,"max_utf8_bytes":32768,"estimated_tokens":8192}})).unwrap()
    }
    pub fn ledger(&self) -> (PathBuf, Vec<Value>) {
        let path = self.root.path().join("threads/t/main.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rows = vec![
            json!({"kind":"genesis","seq":1,"workspace":"ws","thread":"t"}),
            json!({"kind":"input","seq":2,"content":[{"type":"text","text":"How to run project tests"}]}),
            json!({"kind":"turn_open","seq":3,"turn":1,"trigger":{"inputs":[2]}}),
            json!({"kind":"tool_call","seq":4,"turn":1,"call":"c","name":"shell"}),
            json!({"kind":"tool_result","seq":5,"turn":1,"call":"c","status":"ok"}),
            json!({"kind":"output","seq":6,"turn":1,"content":[{"type":"text","text":"Project tests passed"},{"type":"thinking","thinking":"HIDDEN_REASONING"}]}),
            json!({"kind":"settle","seq":7,"turn":1,"outcome":"completed"}),
        ];
        std::fs::write(
            &path,
            rows.iter()
                .map(|v| format!("{}\n", canonical(v)))
                .collect::<String>(),
        )
        .unwrap();
        (path, rows)
    }
    pub fn hook(&self, event: &str, payload: Value, ledger: &Path) -> std::process::Output {
        let request = json!({"format":2,"hook_id":"test","event_id":digest(&json!({"event":event,"payload":payload})),"event":event,"workspace_id":"ws","thread_id":"t","turn_id":1,"data":{"source":{"ledger_path":ledger},"payload":payload}});
        let mut child = Command::new(env!("CARGO_BIN_EXE_tekes-memory"))
            .args(["kernel", "--config"])
            .arg(self.adapter_path())
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{}", canonical(&request)).unwrap();
        child.wait_with_output().unwrap()
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
