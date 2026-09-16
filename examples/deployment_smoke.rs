//! Installed binary smoke outside the source directory, with an empty environment.
use serde_json::json;
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use tekes_memory::{client::Client, common::*};
struct Guard(std::process::Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn main() {
    let bin = PathBuf::from(std::env::args().nth(1).expect("installed binary"));
    assert!(bin.is_absolute());
    let root = tempfile::tempdir().unwrap();
    let threads = root.path().join("threads");
    std::fs::create_dir(&threads).unwrap();
    let out = Command::new(&bin)
        .env_clear()
        .current_dir(root.path())
        .args(["init", "--directory"])
        .arg(root.path().join("config"))
        .args(["--workspace", "ws", "--thread-root"])
        .arg(&threads)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let paths = decode(&out.stdout).unwrap();
    let cp = PathBuf::from(paths["service_config"].as_str().unwrap());
    let mut c = private_json(&cp).unwrap();
    c["port"] = json!(0);
    std::fs::write(&cp, canonical(&c)).unwrap();
    let ready = root.path().join("ready.json");
    let mut service = Guard(
        Command::new(&bin)
            .env_clear()
            .current_dir(root.path())
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
        assert!(service.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    let endpoint = private_json(&ready).unwrap()["endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    let credential = private_json(&cp.parent().unwrap().join("model-credential.json")).unwrap();
    let scope = c["principals"][0]["scopes"][0].clone();
    let mut client = Client::open(
        &endpoint,
        text(&credential, "token").unwrap(),
        Duration::from_secs(5),
    )
    .unwrap();
    client.call("memory.save",json!({"schema_version":1,"scope":scope,"kind":"episode","content":"installed Rust package marker","sources":[{"host":"fixture","ref":"installed","digest":"a".repeat(64)}],"idempotency_key":"installed"})).unwrap();
    let ap = PathBuf::from(paths["adapter_config"].as_str().unwrap());
    let mut a = private_json(&ap).unwrap();
    a["endpoint"] = json!(endpoint);
    std::fs::write(&ap, canonical(&a)).unwrap();
    let hp = cp
        .parent()
        .unwrap()
        .join("hooks/tekes-memory-context-prepare.json");
    let hook = private_json(&hp).unwrap();
    assert_eq!(hook["argv"][0], bin.to_string_lossy().as_ref());
    let request = json!({"format":2,"hook_id":"installed","event_id":"installed","event":"context.prepare","workspace_id":"ws","thread_id":"t","turn_id":1,"data":{"source":{},"payload":{"items":[{"role":"user","content":[{"type":"text","text":"installed Rust package marker"}]}]}}});
    let mut child = Command::new(hook["argv"][0].as_str().unwrap())
        .args(
            hook["argv"]
                .as_array()
                .unwrap()
                .iter()
                .skip(1)
                .map(|v| v.as_str().unwrap()),
        )
        .env_clear()
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{}", canonical(&request)).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        decode(&out.stdout).unwrap()["context"][0]
            .as_str()
            .unwrap()
            .contains("installed Rust package marker")
    );
    let plist = root.path().join("service.plist");
    let out = Command::new(&bin)
        .env_clear()
        .current_dir(root.path())
        .args(["launchd-plist", "--config"])
        .arg(&cp)
        .arg("--output")
        .arg(&plist)
        .output()
        .unwrap();
    assert!(out.status.success());
    let doc = plist::Value::from_file(&plist).unwrap();
    assert_eq!(
        doc.as_dictionary().unwrap()["ProgramArguments"]
            .as_array()
            .unwrap()[0]
            .as_string()
            .unwrap(),
        bin.to_str().unwrap()
    );
    println!(
        "{}",
        canonical(
            &json!({"installed_binary":bin,"empty_environment":true,"outside_source_directory":true,"init":true,"http_save":true,"kernel_hook":true,"launchd_plist":true})
        )
    );
}
