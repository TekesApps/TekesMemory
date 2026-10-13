use mcp::{HttpTransport, McpCancellationToken, McpClient, McpPeer, ProtocolMode, StdioTransport};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    process::{Command, Stdio},
    time::Duration,
};
use tekes_memory::{common::*, config};

async fn exercise(client: &mut dyn McpPeer, scope: &Value, label: &str) -> Value {
    let tools = client.list_tools().await.expect("Kernel tools/list");
    assert_eq!(tools.len(), 7);
    assert!(!tools.iter().any(|tool| tool.name == "memory.observe"));
    assert!(
        tools
            .iter()
            .any(|tool| tool.name == "memory.reconcile_effect")
    );
    let args = json!({"schema_version":1,"scope":scope,"kind":"episode","content":format!("Kernel {label} interoperation"),
        "sources":[{"host":"kernel-harness","ref":label,"digest":"a".repeat(64)}],"idempotency_key":label});
    let result = client
        .call_tool(
            "memory.save",
            schema::IJsonValue::parse(&serde_json::to_vec(&args).unwrap()).unwrap(),
            McpCancellationToken::default(),
        )
        .await
        .expect("Kernel tools/call save");
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["isError"], false);
    let args = json!({"schema_version":1,"scope":scope,"id":result["structuredContent"]["id"]});
    let got = client
        .call_tool(
            "memory.get",
            schema::IJsonValue::parse(&serde_json::to_vec(&args).unwrap()).unwrap(),
            McpCancellationToken::default(),
        )
        .await
        .expect("Kernel tools/call get");
    assert_eq!(
        serde_json::to_value(got).unwrap()["structuredContent"]["item"]["content"],
        format!("Kernel {label} interoperation")
    );
    result
}

#[tokio::main]
async fn main() {
    let binary = std::env::var("TEKES_MEMORY_BIN").expect("absolute isolated Memory binary");
    let root = tempfile::tempdir().unwrap();
    let paths = config::setup(&root.path().join("config"), "ws", root.path(), 0).unwrap();
    let ready = root.path().join("ready.json");
    let mut child = Command::new(&binary)
        .args([
            "serve",
            "--config",
            paths["service_config"].as_str().unwrap(),
            "--ready-file",
        ])
        .arg(&ready)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for _ in 0..1000 {
        if ready.exists() {
            break;
        }
        assert!(child.try_wait().unwrap().is_none());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _guard = Guard(child);
    let endpoint = private_json(&ready).unwrap()["endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    let credential = root.path().join("config/model-credential.json");
    let token = private_json(&credential).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let scope = json!({"kind":"workspace","owner_id":format!("local-{}",unsafe{libc::getuid()}),"workspace_id":"ws"});
    let mut http = McpClient::new(
        "memory-http",
        HttpTransport::new(
            endpoint.parse().unwrap(),
            &BTreeMap::new(),
            Some(&token),
            true,
        )
        .unwrap(),
    );
    http.connect(ProtocolMode::Legacy)
        .await
        .expect("Kernel HTTP initialize");
    assert_eq!(http.protocol_version(), PROTOCOL);
    exercise(&mut http, &scope, "http").await;
    let argv = vec![
        binary,
        "stdio".into(),
        "--endpoint".into(),
        endpoint,
        "--credential-file".into(),
        credential.to_str().unwrap().into(),
    ];
    let mut stdio = McpClient::new(
        "memory-stdio",
        StdioTransport::spawn(&argv, None, &BTreeMap::new()).unwrap(),
    );
    stdio
        .connect(ProtocolMode::Legacy)
        .await
        .expect("Kernel stdio initialize");
    let saved = exercise(&mut stdio, &scope, "stdio").await;
    stdio.close().await.expect("Kernel stdio close");
    // Relay exits while the single HTTP Store owner remains available.
    let args = json!({"schema_version":1,"scope":scope,"id":saved["structuredContent"]["id"]});
    let got = http
        .call_tool(
            "memory.get",
            schema::IJsonValue::parse(&serde_json::to_vec(&args).unwrap()).unwrap(),
            McpCancellationToken::default(),
        )
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(got).unwrap()["isError"], false);
    http.close().await.expect("Kernel HTTP close");
    println!(
        "Kernel MCP HTTP + stdio legacy initialize/list/save/get/close and shared Store: passed"
    );
}
