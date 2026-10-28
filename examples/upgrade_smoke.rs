//! Bounded installed-binary service + adapter upgrade smoke (0.2.1 -> 0.2.2).
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tekes_memory::{client::Client, common::*};

const WAIT: Duration = Duration::from_secs(8);
const OLD_VERSION: &str = "0.2.1";
const NEW_VERSION: &str = "0.2.2";
const TAIL_TOKEN: &str = "UPGRADE_OLD_TAIL_7f31b2";
const TOOL_MARKER: &str = "UPGRADE_TOOL_MARKER_b42e9a";
const TURN_MARKER: &str = "UPGRADE_TURN_MARKER_d90c64";

struct ChildGuard(Child);
impl ChildGuard {
    fn stop(&mut self) {
        if let Some(status) = self.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "service exited unsuccessfully before bounded shutdown"
            );
            return;
        }
        assert_eq!(
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) },
            0,
            "send SIGTERM to service"
        );
        let until = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "service did not stop cleanly after SIGTERM"
                );
                return;
            }
            assert!(Instant::now() < until, "service did not stop after SIGTERM");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

struct TempFixture(Option<tempfile::TempDir>);
impl Drop for TempFixture {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(fixture) = self.0.take()
        {
            eprintln!(
                "upgrade smoke failed; fixture retained at {}",
                fixture.keep().display()
            );
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn run(bin: &Path, args: &[&str], cwd: &Path) -> Output {
    let mut child = Command::new(bin)
        .env_clear()
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{} command timed out", bin.display());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn service(bin: &Path, config: &Path, ready: &Path, stderr: &Path, cwd: &Path) -> ChildGuard {
    let child = Command::new(bin)
        .env_clear()
        .current_dir(cwd)
        .args(["serve", "--config"])
        .arg(config)
        .arg("--ready-file")
        .arg(ready)
        .stdout(Stdio::null())
        .stderr(Stdio::from(fs::File::create(stderr).unwrap()))
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let deadline = Instant::now() + WAIT;
    while !ready.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "service exited before ready"
        );
        assert!(Instant::now() < deadline, "service did not become ready");
        thread::sleep(Duration::from_millis(20));
    }
    child
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from).unwrap();
        assert!(
            !metadata.file_type().is_symlink(),
            "unexpected symlink in isolated fixture"
        );
        if metadata.is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
            fs::set_permissions(&to, fs::Permissions::from_mode(metadata.mode() & 0o777)).unwrap();
        }
    }
    let mode = fs::metadata(source).unwrap().mode() & 0o777;
    fs::set_permissions(target, fs::Permissions::from_mode(mode)).unwrap();
}

fn modes(root: &Path) -> Vec<(PathBuf, u32)> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let m = fs::symlink_metadata(&path).unwrap();
        assert!(!m.file_type().is_symlink());
        out.push((
            path.strip_prefix(root).unwrap().to_path_buf(),
            m.mode() & 0o777,
        ));
        if m.is_dir() {
            stack.extend(fs::read_dir(path).unwrap().map(|e| e.unwrap().path()));
        }
    }
    out.sort();
    out
}

fn invoke_hook(bin: &Path, argv: &[Value], cwd: &Path, request: &Value) {
    assert_eq!(argv[0], bin.to_string_lossy().as_ref());
    let mut child = Command::new(bin)
        .args(argv.iter().skip(1).map(|v| v.as_str().unwrap()))
        .env_clear()
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{}", canonical(request)).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        if child.try_wait().unwrap().is_some() {
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "kernel adapter failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let reply = decode(&out.stdout).unwrap();
            assert_eq!(reply["event_id"], request["event_id"]);
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("installed kernel adapter timed out");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn search(client: &mut Client, scope: &Value, query: &str) -> Value {
    client.call("memory.search", json!({"schema_version":1,"scope":scope,"query":query,"kinds":["episode","fact","procedure"],"budget":{"max_items":100,"max_utf8_bytes":32768,"estimated_tokens":8192}})).unwrap()
}

fn observation_type(item: &Value) -> Option<String> {
    let content: Value = serde_json::from_str(item["content"].as_str()?).ok()?;
    content["observation"].as_str().map(str::to_owned)
}

fn stage(name: &str) {
    println!("stage: {name}");
    std::io::stdout().flush().unwrap();
}

fn main() {
    let mut args = std::env::args().skip(1);
    let old = PathBuf::from(args.next().expect("old installed binary (0.2.1)"));
    let new = PathBuf::from(args.next().expect("new installed binary (0.2.2)"));
    let evidence_root = args.next().map(PathBuf::from);
    assert!(
        args.next().is_none(),
        "usage: upgrade_smoke OLD_BINARY NEW_BINARY [EVIDENCE_ROOT]"
    );
    assert!(
        old.is_absolute() && new.is_absolute(),
        "binary paths must be absolute"
    );
    for (bin, expected) in [(&old, OLD_VERSION), (&new, NEW_VERSION)] {
        let out = Command::new(bin)
            .env_clear()
            .arg("--version")
            .output()
            .unwrap();
        assert!(out.status.success());
        let version = String::from_utf8(out.stdout).unwrap();
        assert_eq!(version.trim(), format!("tekes-memory {expected}"));
    }
    stage("installed versions verified");

    let mut owned_temp = TempFixture(None);
    let root = if let Some(parent) = evidence_root {
        fs::create_dir_all(&parent).unwrap();
        let path = parent.join(format!("tekesmemory-upgrade-{}", uid()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    } else {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().to_path_buf();
        owned_temp.0 = Some(fixture);
        path
    };
    let config_dir = root.join("config");
    let threads = root.join("threads");
    fs::create_dir(&threads).unwrap();
    let init = run(
        &old,
        &[
            "init",
            "--directory",
            config_dir.to_str().unwrap(),
            "--workspace",
            "upgrade-ws",
            "--thread-root",
            threads.to_str().unwrap(),
        ],
        &root,
    );
    assert!(
        init.status.success(),
        "old init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let locations = decode(&init.stdout).unwrap();
    let service_config_path = PathBuf::from(locations["service_config"].as_str().unwrap());
    let adapter_config_path = PathBuf::from(locations["adapter_config"].as_str().unwrap());
    let mut svc_config = private_json(&service_config_path).unwrap();
    svc_config["port"] = json!(0);
    fs::write(
        &service_config_path,
        format!("{}\n", canonical(&svc_config)),
    )
    .unwrap();
    let scope = svc_config["principals"][0]["scopes"][0].clone();
    let owner_id = svc_config["principals"][0]["owner_id"].clone();
    let model_credential = config_dir.join("model-credential.json");
    let adapter_credential = config_dir.join("adapter-credential.json");
    let credential_hashes = [
        hash(&fs::read(&model_credential).unwrap()),
        hash(&fs::read(&adapter_credential).unwrap()),
    ];
    let mut adapter_config = private_json(&adapter_config_path).unwrap();
    let host_instance_id = adapter_config["host_instance_id"].clone();
    let old_ready = root.join("old-ready.json");
    let mut old_service = service(
        &old,
        &service_config_path,
        &old_ready,
        &root.join("old-service.stderr.log"),
        &root,
    );
    let old_endpoint = private_json(&old_ready).unwrap()["endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    let model_token = text(&private_json(&model_credential).unwrap(), "token")
        .unwrap()
        .to_owned();
    let mut old_client = Client::open(&old_endpoint, &model_token, Duration::from_secs(5)).unwrap();
    let distinct_prefix = (0..300)
        .map(|i| format!("legacyword{i:04}"))
        .collect::<Vec<_>>()
        .join(" ");
    let long_content = format!("{distinct_prefix} UPGRADE_OLD_RECORD {TAIL_TOKEN}");
    let saved = old_client.call("memory.save", json!({"schema_version":1,"scope":scope,"kind":"episode","content":long_content,"sources":[{"host":"upgrade-smoke","ref":"old-0.2.1","digest":"a".repeat(64)}],"idempotency_key":"upgrade-old-record"})).unwrap();
    let old_record_id = saved["id"].as_str().unwrap().to_owned();
    assert_eq!(saved["revision"], 1);
    assert!(
        search(&mut old_client, &scope, TAIL_TOKEN)["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != old_record_id),
        "old version unexpectedly indexed the tail token beyond its legacy limit"
    );
    drop(old_client);
    old_service.stop();
    drop(old_service);
    stage("v0.2.1 wrote durable record and stopped cleanly");

    adapter_config["endpoint"] = json!(old_endpoint);
    fs::write(
        &adapter_config_path,
        format!("{}\n", canonical(&adapter_config)),
    )
    .unwrap();
    let backup = root.join("backup");
    copy_tree(&config_dir, &backup);
    let original_modes = modes(&config_dir);
    assert_eq!(
        original_modes,
        modes(&backup),
        "backup must preserve file modes"
    );
    stage("config and data backed up with file modes preserved");
    let before_scope = scope.clone();
    let before_host_instance = host_instance_id.clone();

    let new_ready = root.join("new-ready.json");
    let mut new_service = service(
        &new,
        &service_config_path,
        &new_ready,
        &root.join("new-service.stderr.log"),
        &root,
    );
    let endpoint = private_json(&new_ready).unwrap()["endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    adapter_config["endpoint"] = json!(endpoint);
    fs::write(
        &adapter_config_path,
        format!("{}\n", canonical(&adapter_config)),
    )
    .unwrap();

    let hooks_dir = config_dir.join("hooks");
    let mut hook_events = vec![];
    let mut original_hooks = Vec::new();
    for entry in fs::read_dir(&hooks_dir).unwrap() {
        let path = entry.unwrap().path();
        let mut hook = private_json(&path).unwrap();
        original_hooks.push((path.clone(), hook.clone()));
        hook["argv"] = json!([new, "kernel", "--config", adapter_config_path]);
        hook_events.push(hook["event"].as_str().unwrap().to_owned());
        fs::write(&path, format!("{}\n", canonical(&hook))).unwrap();
    }
    hook_events.sort();
    assert_eq!(
        hook_events,
        vec![
            "context.before_compact",
            "context.prepare",
            "tool.completed",
            "turn.before",
            "turn.settled"
        ]
    );
    let new_config = private_json(&service_config_path).unwrap();
    assert_eq!(new_config["principals"][0]["scopes"][0], before_scope);
    assert_eq!(new_config["principals"][0]["owner_id"], owner_id);
    let adapter_after = private_json(&adapter_config_path).unwrap();
    assert_eq!(adapter_after["host_instance_id"], before_host_instance);
    assert_eq!(adapter_after["owner_id"], owner_id);
    assert_eq!(adapter_after["workspace_id"], before_scope["workspace_id"]);
    assert_eq!(
        hash(&fs::read(&model_credential).unwrap()),
        credential_hashes[0]
    );
    assert_eq!(
        hash(&fs::read(&adapter_credential).unwrap()),
        credential_hashes[1]
    );
    stage("v0.2.2 took over the same config, data, credentials, and identities");
    for (path, original) in &original_hooks {
        let hook = private_json(path).unwrap();
        assert_eq!(hook["argv"][0], new.to_string_lossy().as_ref());
        assert_eq!(hook["argv"][1], "kernel");
        assert_eq!(
            hook["argv"][3],
            adapter_config_path.to_string_lossy().as_ref()
        );
        let mut expected = original.clone();
        expected["argv"] = hook["argv"].clone();
        assert_eq!(
            hook, expected,
            "hook identity and policy fields must be preserved"
        );
    }

    let mut client = Client::open(&endpoint, &model_token, Duration::from_secs(5)).unwrap();
    let got = client
        .call(
            "memory.get",
            json!({"schema_version":1,"scope":scope,"id":old_record_id}),
        )
        .unwrap()["item"]
        .clone();
    assert_eq!(got["revision"], 1);
    assert_eq!(got["content"], long_content);
    assert_eq!(got["id"], old_record_id);
    let legacy_search = search(&mut client, &scope, TAIL_TOKEN);
    assert!(
        legacy_search["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == old_record_id),
        "new version did not reindex legacy tail token"
    );
    stage("old record ID, revision, content, and legacy tail-token search verified");

    let ledger = threads.join("upgrade-thread.jsonl");
    let rows = vec![
        json!({"kind":"genesis","seq":1,"workspace":"upgrade-ws","thread":"upgrade-thread"}),
        json!({"kind":"input","seq":2,"content":[{"type":"text","text":"Upgrade lifecycle input"}]}),
        json!({"kind":"turn_open","seq":3,"turn":1,"trigger":{"inputs":[2]}}),
        json!({"kind":"tool_call","seq":4,"turn":1,"call":"upgrade-call","name":"upgrade-tool"}),
        json!({"kind":"tool_result","seq":5,"turn":1,"call":"upgrade-call","status":"ok","content":[{"type":"text","text":TOOL_MARKER}]}),
        json!({"kind":"output","seq":6,"turn":1,"content":[{"type":"text","text":TURN_MARKER}]}),
        json!({"kind":"settle","seq":7,"turn":1,"outcome":"completed"}),
    ];
    fs::write(
        &ledger,
        rows.iter()
            .map(|row| format!("{}\n", canonical(row)))
            .collect::<String>(),
    )
    .unwrap();
    let hooks = [("tool.completed", 5), ("turn.settled", 7)];
    for (event, seq) in hooks {
        let path = hooks_dir.join(format!("tekes-memory-{}.json", event.replace('.', "-")));
        let hook = private_json(&path).unwrap();
        invoke_hook(
            &new,
            array(&hook, "argv").unwrap(),
            &root,
            &json!({"format":2,"hook_id":hook["id"],"event_id":format!("upgrade-{event}"),"event":event,"workspace_id":"upgrade-ws","thread_id":"upgrade-thread","turn_id":1,"data":{"source":{"ledger_path":ledger},"payload":{"source_seq":seq,"record":rows[seq as usize - 1]}}}),
        );
    }
    stage("installed v0.2.2 adapter accepted tool.completed seq 5 and turn.settled seq 7");
    let deadline = Instant::now() + WAIT;
    let (tool, turn) = loop {
        let found = search(&mut client, &scope, TOOL_MARKER);
        let items = found["items"].as_array().unwrap();
        let tool = items
            .iter()
            .find(|item| observation_type(item).as_deref() == Some("tool_result"))
            .cloned();
        let turn = items
            .iter()
            .find(|item| observation_type(item).as_deref() == Some("turn_outcome"))
            .cloned();
        if let (Some(tool), Some(turn)) = (tool, turn) {
            break (tool, turn);
        }
        assert!(
            Instant::now() < deadline,
            "adapter observations were not durably searchable"
        );
        thread::sleep(Duration::from_millis(40));
    };
    assert_ne!(tool["id"], turn["id"]);
    let ids = [tool["id"].clone(), turn["id"].clone()];
    let forget = client.call("memory.forget", json!({"schema_version":1,"scope":scope,"ids":[tool["id"]],"expected_revisions":[tool["revision"]],"idempotency_key":"upgrade-forget-tool-result"})).unwrap();
    assert_eq!(forget["hidden"], true);
    assert_eq!(
        forget["records_deleted"], 2,
        "tool result and dependent turn summary must be forgotten"
    );
    assert!(!canonical(&search(&mut client, &scope, TOOL_MARKER)).contains(TOOL_MARKER));
    assert!(!canonical(&search(&mut client, &scope, TOOL_MARKER)).contains(TURN_MARKER));
    stage("forget removed the actual tool result and dependent settled record");
    drop(client);
    new_service.stop();
    drop(new_service);
    let restarted_ready = root.join("restarted-ready.json");
    let mut restarted = service(
        &new,
        &service_config_path,
        &restarted_ready,
        &root.join("restarted-service.stderr.log"),
        &root,
    );
    let restarted_endpoint = private_json(&restarted_ready).unwrap()["endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    adapter_config = private_json(&adapter_config_path).unwrap();
    adapter_config["endpoint"] = json!(restarted_endpoint);
    fs::write(
        &adapter_config_path,
        format!("{}\n", canonical(&adapter_config)),
    )
    .unwrap();
    let mut after_restart =
        Client::open(&restarted_endpoint, &model_token, Duration::from_secs(5)).unwrap();
    let absent = search(&mut after_restart, &scope, TOOL_MARKER);
    assert!(!canonical(&absent).contains(TOOL_MARKER));
    assert!(!canonical(&absent).contains(TURN_MARKER));
    for id in ids {
        assert_eq!(
            after_restart
                .call(
                    "memory.get",
                    json!({"schema_version":1,"scope":scope,"id":id})
                )
                .unwrap_err()
                .0,
            "not_found"
        );
    }
    let old_after = after_restart
        .call(
            "memory.get",
            json!({"schema_version":1,"scope":scope,"id":old_record_id}),
        )
        .unwrap();
    assert_eq!(old_after["item"]["content"], long_content);
    assert_eq!(
        hash(&fs::read(&model_credential).unwrap()),
        credential_hashes[0]
    );
    assert_eq!(
        hash(&fs::read(&adapter_credential).unwrap()),
        credential_hashes[1]
    );
    drop(after_restart);
    restarted.stop();
    drop(restarted);
    stage("forget closure and legacy record verified after service restart");

    let summary = json!({"old_version":OLD_VERSION,"new_version":NEW_VERSION,"old_record_preserved":true,"legacy_tail_token_reindexed":true,"five_hooks_use_new_binary":true,"tool_completed_seq":5,"turn_settled_seq":7,"forget_tool_result_deleted_dependent_turn":true,"forget_survives_restart":true,"credential_hashes_unchanged":true,"scope_and_host_instance_unchanged":true,"backup_modes_preserved":true,"fixture_root":root});
    fs::write(
        root.join("summary.json"),
        format!("{}\n", canonical(&summary)),
    )
    .unwrap();
    println!("{}", canonical(&summary));
}
