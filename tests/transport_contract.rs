mod support;

use serde_json::{Value, json};
use std::{
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use support::Service;
use tekes_memory::{client::Client, common::*};

struct Wire<'a> {
    service: &'a Service,
    http: reqwest::blocking::Client,
    session: String,
}

#[test]
fn negotiation_and_invalid_initialize_keep_json_contract() {
    let s = Service::new();
    let http = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap();
    let initialize = |client_info: Value, version: &str| {
        http.post(&s.url)
            .bearer_auth(s.token("model"))
            .header("Accept", "application/json, text/event-stream")
            .json(
                &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":version,"capabilities":{},"clientInfo":client_info}}),
            )
            .send()
            .unwrap()
    };
    let unsupported = initialize(json!({"name":"future-client","version":"1"}), "1900-01-01");
    assert_eq!(unsupported.status(), 200);
    assert!(
        unsupported.headers()["Content-Type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(
        unsupported.json::<Value>().unwrap()["result"]["protocolVersion"],
        PROTOCOL
    );
    // Preserve the pre-migration clientInfo:{} compatibility explicitly.
    let compatible = initialize(json!({}), PROTOCOL);
    assert_eq!(compatible.status(), 200);
    assert_eq!(
        compatible.json::<Value>().unwrap()["result"]["protocolVersion"],
        PROTOCOL
    );
    let malformed = initialize(json!("invalid-client-info"), PROTOCOL);
    assert!(malformed.status().is_client_error());
    let wire = Wire::new(&s);
    let mismatch = wire
        .http
        .post(&s.url)
        .bearer_auth(s.token("model"))
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "1900-01-01")
        .header("MCP-Session-Id", &wire.session)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .send()
        .unwrap();
    assert_eq!(mismatch.status(), 400);
    let duplicate = wire
        .http
        .post(&s.url)
        .bearer_auth(s.token("model"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("MCP-Protocol-Version", PROTOCOL)
        .header("MCP-Session-Id", &wire.session)
        .body(r#"{"jsonrpc":"2.0","id":2,"id":3,"method":"tools/list"}"#)
        .send()
        .unwrap();
    assert_eq!(duplicate.status(), 400);
}

#[test]
fn stdio_effects_use_http_owner_and_relay_exit_keeps_service_alive() {
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
    {
        let mut request = |value: Value| {
            writeln!(input, "{}", canonical(&value)).unwrap();
            input.flush().unwrap();
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            decode(line.as_bytes()).unwrap()
        };
        assert_eq!(
            request(
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"stdio-contract","version":"1"}}})
            )["result"]["protocolVersion"],
            PROTOCOL
        );
    }
    writeln!(
        input,
        "{}",
        canonical(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
    )
    .unwrap();
    let key = "e".repeat(64);
    writeln!(input,"{}",canonical(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
        "name":"memory.save","_meta":{"io.tekes/idempotencyKey":key},"arguments":{
            "schema_version":1,"scope":s.scope,"kind":"episode","content":"stdio shared owner",
            "sources":[{"host":"test","ref":"stdio-effect","digest":"a".repeat(64)}],"idempotency_key":"stdio-effect"}}}))).unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let saved = decode(line.as_bytes()).unwrap();
    assert_eq!(saved["result"]["isError"], false);
    drop(input);
    assert!(child.wait().unwrap().success());
    let wire = Wire::new(&s);
    let receipt = wire.rpc("memory.reconcile_effect", json!({"idempotencyKey":key}));
    assert_eq!(receipt["structuredContent"]["status"], "confirmed");
    assert_eq!(receipt["structuredContent"]["value"], saved["result"]);
    assert_eq!(wire.rpc("memory.get",json!({"schema_version":1,"scope":s.scope,"id":saved["result"]["structuredContent"]["id"]}))["isError"],false);
    s.save();
    assert_eq!(s.search()["items"].as_array().unwrap().len(), 2);
}

impl<'a> Wire<'a> {
    fn new(service: &'a Service) -> Self {
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let init = http.post(&service.url).bearer_auth(service.token("model"))
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"contract","version":"1"}}}))
            .send().unwrap();
        assert_eq!(init.status(), 200);
        assert!(
            init.headers()["Content-Type"]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let session = init.headers()["MCP-Session-Id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            init.json::<Value>().unwrap()["result"]["protocolVersion"],
            PROTOCOL
        );
        let wire = Self {
            service,
            http,
            session,
        };
        assert_eq!(
            wire.post(
                "model",
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .status(),
            202
        );
        wire
    }

    fn post(&self, role: &str, request: Value) -> reqwest::blocking::Response {
        self.http
            .post(&self.service.url)
            .bearer_auth(self.service.token(role))
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL)
            .header("MCP-Session-Id", &self.session)
            .json(&request)
            .send()
            .unwrap()
    }

    fn rpc(&self, name: &str, args: Value) -> Value {
        let r = self.post("model", json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":name,"arguments":args}}));
        assert_eq!(r.status(), 200);
        assert!(
            r.headers()["Content-Type"]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        r.json::<Value>().unwrap()["result"].clone()
    }
}

#[test]
fn authorization_changes_apply_inside_existing_session() {
    let s = Service::new();
    let wire = Wire::new(&s);
    let saved = s.save();
    let args = json!({"schema_version":1,"scope":s.scope,"id":saved["id"]});
    assert_eq!(wire.rpc("memory.get", args.clone())["isError"], false);
    assert_eq!(
        wire.post(
            "adapter",
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
        )
        .status(),
        404
    );

    let path = std::path::Path::new(s.paths["service_config"].as_str().unwrap());
    let mut config = private_json(path).unwrap();
    let model = config["principals"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["role"] == "model")
        .unwrap();
    model["tools"]
        .as_array_mut()
        .unwrap()
        .retain(|tool| tool != "memory.get");
    std::fs::write(path, canonical(&config)).unwrap();
    let list = wire
        .post(
            "model",
            json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
        )
        .json::<Value>()
        .unwrap();
    assert!(
        !list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "memory.get")
    );
    let denied = wire.rpc("memory.get", args.clone());
    assert_eq!(denied["isError"], true);
    assert_eq!(denied["structuredContent"]["error"]["code"], "scope_denied");

    let model = config["principals"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["role"] == "model")
        .unwrap();
    model["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!("memory.get"));
    model["scopes"] =
        json!([{ "kind":"workspace","owner_id":s.scope["owner_id"],"workspace_id":"other" }]);
    std::fs::write(path, canonical(&config)).unwrap();
    let denied = wire.rpc("memory.get", args);
    assert_eq!(denied["isError"], true);
    assert_eq!(denied["structuredContent"]["error"]["code"], "scope_denied");
}

#[test]
fn tool_business_failures_remain_json_results_and_do_not_poison_session() {
    let s = Service::new();
    let wire = Wire::new(&s);
    let result = wire.rpc(
        "memory.get",
        json!({"schema_version":1,"scope":s.scope,"id":"missing"}),
    );
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["error"]["code"], "not_found");
    let denied = wire.rpc("memory.observe", json!({}));
    assert_eq!(denied["isError"], true);
    assert_eq!(denied["structuredContent"]["error"]["code"], "scope_denied");
    let saved = s.save();
    assert_eq!(
        wire.rpc(
            "memory.get",
            json!({"schema_version":1,"scope":s.scope,"id":saved["id"]})
        )["isError"],
        false
    );
}

#[test]
fn service_restart_invalidates_session_but_preserves_principal_bound_receipt() {
    let mut s = Service::new();
    let key = "f".repeat(64);
    let wire = Wire::new(&s);
    let response=wire.post("model",json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
        "name":"memory.save","_meta":{"io.tekes/idempotencyKey":key},"arguments":{
            "schema_version":1,"scope":s.scope,"kind":"episode","content":"restart receipt",
            "sources":[{"host":"test","ref":"restart","digest":"a".repeat(64)}],"idempotency_key":"restart-receipt"}}})).json::<Value>().unwrap();
    assert_eq!(response["result"]["isError"], false);
    let session = wire.session.clone();
    drop(wire);
    s.restart();
    let wire = Wire::new(&s);
    let old = wire
        .http
        .post(&s.url)
        .bearer_auth(s.token("model"))
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", PROTOCOL)
        .header("MCP-Session-Id", session)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .send()
        .unwrap();
    assert_eq!(old.status(), 404);
    let receipt = wire.rpc("memory.reconcile_effect", json!({"idempotencyKey":key}));
    assert_eq!(receipt["structuredContent"]["status"], "confirmed");
    assert_eq!(receipt["structuredContent"]["value"], response["result"]);
    assert_eq!(
        s.client("adapter")
            .call("memory.reconcile_effect", json!({"idempotencyKey":key}))
            .unwrap()["status"],
        "not_found"
    );
    assert_eq!(s.search()["items"].as_array().unwrap().len(), 1);
}

#[test]
fn disconnected_write_is_reconciled_without_duplicate_effect() {
    for lose_before_response in [false, true] {
        let s = Service::new();
        let wire = Wire::new(&s);
        let key = "d".repeat(64);
        let arguments = json!({"schema_version":1,"scope":s.scope,"kind":"episode","content":"disconnected write",
        "sources":[{"host":"test","ref":"disconnect","digest":"a".repeat(64)}],"idempotency_key":"disconnected-write"});
        let body = canonical(
            &json!({"jsonrpc":"2.0","id":99,"method":"tools/call","params":{
        "name":"memory.save","arguments":arguments,"_meta":{"io.tekes/idempotencyKey":key}}}),
        );
        let url = reqwest::Url::parse(&s.url).unwrap();
        let mut socket = TcpStream::connect(("127.0.0.1", url.port().unwrap())).unwrap();
        write!(socket,"POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: {}\r\nMCP-Session-Id: {}\r\nContent-Length: {}\r\n\r\n{}",url.port().unwrap(),s.token("model"),PROTOCOL,wire.session,body.len(),body).unwrap();
        socket.flush().unwrap();
        // Read only the response prefix: the write has committed, but its result is lost.
        if !lose_before_response {
            let mut prefix = [0; 1];
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            socket.read_exact(&mut prefix).unwrap();
        }
        drop(socket);
        let mut receipt = Value::Null;
        for _ in 0..100 {
            receipt = wire.rpc("memory.reconcile_effect", json!({"idempotencyKey":key}));
            if receipt["structuredContent"]["status"] == "confirmed" {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if lose_before_response {
            // Closing immediately can precede dispatch. The test makes no
            // second send, and confirms exactly zero or one persisted effect.
            let status = receipt["structuredContent"]["status"].as_str().unwrap();
            assert!(matches!(status, "not_found" | "confirmed"));
            assert_eq!(
                s.search()["items"].as_array().unwrap().len(),
                usize::from(status == "confirmed")
            );
            continue;
        }
        assert_eq!(receipt["structuredContent"]["status"], "confirmed");
        let replay = wire
            .post("model", decode(body.as_bytes()).unwrap())
            .json::<Value>()
            .unwrap();
        assert_eq!(replay["result"], receipt["structuredContent"]["value"]);
        assert_eq!(s.search()["items"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn client_does_not_replay_ambiguous_mutations_on_stale_session_or_timeout() {
    for (stale_session, slow_initialize) in [(true, false), (false, false), (false, true)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let writes = Arc::new(AtomicUsize::new(0));
        let observed = writes.clone();
        let mock = std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_millis(2000);
            while Instant::now() < end {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                let body_start = loop {
                    let Ok(n) = stream.read(&mut chunk) else {
                        break None;
                    };
                    if n == 0 {
                        break None;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(at) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..at]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|n| n.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= at + 4 + length {
                            break Some(at + 4);
                        }
                    }
                };
                let Some(at) = body_start else {
                    continue;
                };
                let request: Value = serde_json::from_slice(&bytes[at..]).unwrap_or(Value::Null);
                let (status, response) = match request["method"].as_str() {
                    Some("initialize") => {
                        if slow_initialize {
                            std::thread::sleep(Duration::from_millis(600));
                        }
                        (200, json!({"jsonrpc":"2.0","id":request["id"],"result":{"protocolVersion":PROTOCOL,"capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"1"}}}).to_string())
                    }
                    Some("notifications/initialized") => (202, String::new()),
                    Some("tools/call") => {
                        observed.fetch_add(1, Ordering::SeqCst);
                        if slow_initialize {
                            std::thread::sleep(Duration::from_millis(600));
                            (200,json!({"jsonrpc":"2.0","id":request["id"],"result":{"isError":false,"content":[],"structuredContent":{"ok":true}}}).to_string())
                        } else {
                            if !stale_session {
                                std::thread::sleep(Duration::from_millis(1100));
                            }
                            (404, String::new())
                        }
                    }
                    _ => (200, String::new()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nMCP-Session-Id: stale\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
            }
        });
        let mut client =
            Client::open(&endpoint, "isolated-test", Duration::from_millis(900)).unwrap();
        assert!(client.call("memory.save", json!({})).is_err());
        drop(client);
        mock.join().unwrap();
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "ambiguous mutation must not be resent"
        );
    }
}

#[test]
fn stdio_ping_cannot_succeed_after_http_owner_exits() {
    let mut s = Service::new();
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
    writeln!(input,"{}",canonical(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"ping-test","version":"1"}}}))).unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(
        decode(line.as_bytes()).unwrap()["result"]["protocolVersion"],
        PROTOCOL
    );
    writeln!(
        input,
        "{}",
        canonical(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
    )
    .unwrap();
    input.flush().unwrap();
    s.child.kill().unwrap();
    s.child.wait().unwrap();
    writeln!(
        input,
        "{}",
        canonical(&json!({"jsonrpc":"2.0","id":2,"method":"ping"}))
    )
    .unwrap();
    input.flush().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let result = output.read_line(&mut line);
        let _ = send.send((result, line));
    });
    let result = receive.recv_timeout(Duration::from_secs(2));
    let _ = child.kill();
    let _ = child.wait();
    reader.join().unwrap();
    let (read, line) = result.expect("relay must fail ping promptly after owner exits");
    read.unwrap();
    if !line.is_empty() {
        let response = decode(line.as_bytes()).unwrap();
        assert!(
            response.get("error").is_some(),
            "relay must not report local ping success: {response}"
        );
    }
}

#[test]
fn stdio_invalid_input_exits_bounded_even_when_host_keeps_stdin_open() {
    for bytes in [
        br#"{"jsonrpc":"2.0","id":1,"id":2,"method":"initialize"}
"#
        .to_vec(),
        vec![b'x'; MAX_BODY + 1],
    ] {
        let s = Service::new();
        let mut child = Command::new(env!("CARGO_BIN_EXE_tekes-memory"))
            .args(["stdio", "--endpoint", &s.url, "--credential-file"])
            .arg(s.credential("model"))
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let _ = input.write_all(&bytes);
        let _ = input.flush();
        // Keep input open throughout the wait. EOF must not be required to exit.
        let mut exited = None;
        for _ in 0..400 {
            exited = child.try_wait().unwrap();
            if exited.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if exited.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let status = exited.expect("invalid stdio input must exit without waiting for stdin EOF");
        assert!(!status.success());
        drop(input);
    }
}

#[test]
fn client_drop_waits_for_bounded_session_delete_ack() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let deletes = Arc::new(AtomicUsize::new(0));
    let ack = Arc::new(AtomicUsize::new(0));
    let observed = deletes.clone();
    let completed = ack.clone();
    let server = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            let body_start = loop {
                let Ok(n) = stream.read(&mut chunk) else {
                    break None;
                };
                if n == 0 {
                    break None;
                }
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..at]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|v| v.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= at + 4 + length {
                        break Some(at + 4);
                    }
                }
            };
            let Some(at) = body_start else { continue };
            let delete = bytes.starts_with(b"DELETE ");
            let request: Value = serde_json::from_slice(&bytes[at..]).unwrap_or(Value::Null);
            let (status, body) = if delete {
                observed.fetch_add(1, Ordering::SeqCst);
                // The server intentionally completes close after a short delay;
                // merely dropping the sender cannot satisfy the ack assertion.
                std::thread::sleep(Duration::from_millis(30));
                completed.store(1, Ordering::SeqCst);
                (200, String::new())
            } else if request["method"] == "initialize" {
                (200,json!({"jsonrpc":"2.0","id":request["id"],"result":{"protocolVersion":PROTOCOL,"capabilities":{"tools":{}},"serverInfo":{"name":"drop-mock","version":"1"}}}).to_string())
            } else if request["method"] == "notifications/initialized" {
                (202, String::new())
            } else {
                (405, String::new())
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nMCP-Session-Id: drop-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if delete {
                break;
            }
        }
    });
    let client = Client::open(&endpoint, "isolated-drop-test", Duration::from_secs(2)).unwrap();
    let began = Instant::now();
    drop(client);
    let elapsed = began.elapsed();
    let received_at_return = deletes.load(Ordering::SeqCst);
    let ack_at_return = ack.load(Ordering::SeqCst);
    server.join().unwrap();
    assert_eq!(
        received_at_return, 1,
        "Drop must send DELETE before returning"
    );
    assert_eq!(
        ack_at_return, 1,
        "Drop must wait for bounded close acknowledgement"
    );
    assert!(
        elapsed < Duration::from_millis(400),
        "Drop must remain bounded: {elapsed:?}"
    );
    assert_eq!(
        deletes.load(Ordering::SeqCst),
        1,
        "close must not replay DELETE"
    );
}
