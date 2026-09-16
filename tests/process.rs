mod support;
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    process::{Command, Stdio},
    time::Duration,
};
use support::Service;
use tekes_memory::{client::Client, common::*};
#[test]
fn real_http_store_restart_and_all_five_kernel_hooks() {
    let mut s = Service::new();
    s.save();
    let (path, rows) = s.ledger();
    let before = std::fs::read(&path).unwrap();
    let out = s.hook(
        "context.prepare",
        json!({"items":[{"role":"user","content":[{"type":"text","text":"project tests"}]}]}),
        &path,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reply = decode(&out.stdout).unwrap();
    assert!(reply["context"][0].as_str().unwrap().contains("cargo test"));
    for (event, payload) in [
        ("turn.before", json!({"source_seq":3,"record":rows[2]})),
        ("tool.completed", json!({"source_seq":5,"record":rows[4]})),
        (
            "context.before_compact",
            json!({"through_seq":6,"covers":[2,5,6],"manual":false}),
        ),
        ("turn.settled", json!({"source_seq":7,"record":rows[6]})),
    ] {
        for _ in 0..2 {
            let out = s.hook(event, payload.clone(), &path);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let mut result = Value::Null;
    for _ in 0..100 {
        result = s.search();
        if result["items"].as_array().unwrap().len() == 4 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(result["items"].as_array().unwrap().len(), 4);
    assert!(!canonical(&result).contains("HIDDEN_REASONING"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let ids = result["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect::<Vec<_>>();
    s.restart();
    assert_eq!(
        s.search()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].clone())
            .collect::<Vec<_>>(),
        ids
    )
}
#[test]
fn adapter_continuation_reads_admitted_task() {
    let s = Service::new();
    s.save();
    let (path, _) = s.ledger();
    let out = s.hook(
        "context.prepare",
        json!({"items":[{"role":"tool","content":[]}],"through_seq":5}),
        &path,
    );
    assert!(out.status.success());
    assert!(
        decode(&out.stdout).unwrap()["context"][0]
            .as_str()
            .unwrap()
            .contains("cargo test")
    )
}
#[test]
fn host_auth_origin_and_media_guards() {
    let s = Service::new();
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap();
    assert_eq!(http.get(&s.url).send().unwrap().status(), 401);
    assert_eq!(
        http.get(&s.url)
            .bearer_auth(s.token("model"))
            .header("Host", "evil.example")
            .send()
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        http.get(&s.url)
            .bearer_auth(s.token("model"))
            .header("Origin", "https://evil.example")
            .send()
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        http.get(&s.url)
            .bearer_auth(s.token("model"))
            .send()
            .unwrap()
            .status(),
        405
    );
    assert_eq!(
        http.post(&s.url)
            .bearer_auth(s.token("model"))
            .body("{}")
            .send()
            .unwrap()
            .status(),
        415
    )
}
#[test]
fn grants_are_hot_revoked_and_session_is_principal_bound() {
    let s = Service::new();
    let mut c = s.client("model");
    let path = std::path::Path::new(s.paths["service_config"].as_str().unwrap());
    let mut config = private_json(path).unwrap();
    config["principals"]
        .as_array_mut()
        .unwrap()
        .retain(|p| p["role"] != "model");
    std::fs::write(path, canonical(&config)).unwrap();
    assert!(c.rpc("tools/list", json!({}), false).is_err());
    assert!(Client::open(&s.url, &s.token("model"), Duration::from_secs(1)).is_err());
    assert!(
        s.client("adapter")
            .rpc("tools/list", json!({}), false)
            .is_ok()
    )
}
#[test]
fn single_owner_lock_rejects_second_service() {
    let s = Service::new();
    let out = Command::new(env!("CARGO_BIN_EXE_tekes-memory"))
        .args([
            "serve",
            "--config",
            s.paths["service_config"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("service_already_running"));
    s.save();
}
#[test]
fn adapter_rejects_symlink_and_wrong_source() {
    use std::os::unix::fs::symlink;
    let s = Service::new();
    let (path, rows) = s.ledger();
    let other = path.with_file_name("source.jsonl");
    std::fs::rename(&path, &other).unwrap();
    symlink(&other, &path).unwrap();
    let out = s.hook(
        "turn.settled",
        json!({"source_seq":7,"record":rows[6]}),
        &path,
    );
    assert!(!out.status.success());
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&other, &path).unwrap();
    let out = s.hook(
        "turn.settled",
        json!({"source_seq":7,"record":{"kind":"settle","outcome":"forged"}}),
        &path,
    );
    assert!(!out.status.success());
    assert_eq!(s.search()["items"], json!([]))
}
#[test]
fn configuration_permissions_and_symlink_are_checked() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let s = Service::new();
    let path = s.credential("model");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(private_json(&path).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = path.with_file_name("linked.json");
    symlink(&path, &link).unwrap();
    assert!(private_json(&link).is_err())
}
#[test]
fn direct_mcp_session_lifecycle_and_cross_principal_reject() {
    let s = Service::new();
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap();
    let post = |token: String, session: Option<&str>, v: Value| {
        let mut b = http
            .post(&s.url)
            .bearer_auth(token)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL);
        if let Some(s) = session {
            b = b.header("MCP-Session-Id", s)
        }
        b.json(&v).send().unwrap()
    };
    let r = post(
        s.token("model"),
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{}}}),
    );
    assert_eq!(r.status(), 200);
    let session = r.headers()["MCP-Session-Id"].to_str().unwrap().to_owned();
    assert_eq!(
        post(
            s.token("model"),
            Some(&session),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
        )
        .status(),
        400
    );
    assert_eq!(
        post(
            s.token("adapter"),
            Some(&session),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .status(),
        404
    );
    assert_eq!(
        post(
            s.token("model"),
            Some(&session),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .status(),
        202
    );
    let r = post(
        s.token("model"),
        Some(&session),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
    )
    .json::<Value>()
    .unwrap();
    assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 6);
    assert_eq!(
        http.delete(&s.url)
            .bearer_auth(s.token("model"))
            .header("MCP-Session-Id", session)
            .send()
            .unwrap()
            .status(),
        200
    )
}
#[test]
fn stdio_is_a_native_rust_relay() {
    let s = Service::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tekes-memory"))
        .args(["stdio", "--endpoint", &s.url, "--credential-file"])
        .arg(s.credential("model"))
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    for (id, method, params) in [
        (
            1,
            "initialize",
            json!({"protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{}}),
        ),
        (2, "tools/list", json!({})),
    ] {
        writeln!(
            input,
            "{}",
            canonical(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
        )
        .unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let r = decode(line.as_bytes()).unwrap();
        assert_eq!(r["id"], id);
        if id == 2 {
            assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 6)
        }
    }
    drop(input);
    assert!(child.wait().unwrap().success())
}
#[test]
fn unsafe_endpoint_and_redirect_are_not_allowed() {
    for url in [
        "http://example.com/mcp",
        "http://127.0.0.1/other",
        "http://user@localhost/mcp",
        "http://localhost/mcp?token=x",
    ] {
        assert!(tekes_memory::client::validate_endpoint(url).is_err())
    }
}
#[test]
#[ignore = "requires MCP_SDK_ROOT and Node.js; runs official upstream SDK against Rust service"]
fn official_typescript_sdk_interoperability() {
    let s = Service::new();
    let status = Command::new(std::env::var("NODE").unwrap_or("node".into()))
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/official_client.mjs"))
        .arg(&s.url)
        .arg(s.credential("model"))
        .arg(s.scope["owner_id"].as_str().unwrap())
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "requires MCP_SDK_ROOT and Node.js; official upstream SDK over Rust stdio relay"]
fn official_typescript_sdk_stdio_interoperability() {
    let s = Service::new();
    let status = Command::new(std::env::var("NODE").unwrap_or("node".into()))
        .env("TEKES_MEMORY_STDIO_BIN", env!("CARGO_BIN_EXE_tekes-memory"))
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/official_client.mjs"))
        .arg(&s.url)
        .arg(s.credential("model"))
        .arg(s.scope["owner_id"].as_str().unwrap())
        .status()
        .unwrap();
    assert!(status.success());
}
