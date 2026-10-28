use crate::common::*;
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::*,
    service::{RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, common::client_side_sse::SseRetryPolicy,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
#[derive(Debug)]
struct NoRetry;
impl SseRetryPolicy for NoRetry {
    fn retry(&self, _: usize) -> Option<Duration> {
        None
    }
}
#[derive(Clone)]
struct LocalClient;
impl ClientHandler for LocalClient {
    fn get_info(&self) -> ClientConfig {
        serde_json::from_value(json!({"protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"tekes-memory-rust","version":env!("CARGO_PKG_VERSION")}})).unwrap()
    }
}
pub(crate) struct AsyncClient {
    service: RunningService<RoleClient, LocalClient>,
}
impl AsyncClient {
    pub(crate) async fn open(url: &str, token: &str, timeout: Duration) -> Result<Self> {
        validate_endpoint(url)?;
        let http = reqwest_mcp::Client::builder()
            .no_proxy()
            .retry(reqwest_mcp::retry::never())
            .redirect(reqwest_mcp::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| err("unavailable"))?;
        let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_owned())
            .auth_header(token)
            .reinit_on_expired_session(false)
            .max_concurrent_requests(1)
            .max_sse_event_size(MAX_RESPONSE);
        config.retry_config = Arc::new(NoRetry);
        let transport = StreamableHttpClientTransport::with_client(http, config);
        let service = tokio::time::timeout(timeout, LocalClient.serve(transport))
            .await
            .map_err(|_| err("unavailable"))?
            .map_err(|_| err("unavailable"))?;
        if service
            .peer_info()
            .is_none_or(|info| info.protocol_version.as_str() != PROTOCOL)
        {
            return Err(err("unsupported_protocol"));
        }
        Ok(Self { service })
    }
    pub(crate) async fn rpc(
        &self,
        method: &str,
        params: Value,
        notification: bool,
    ) -> std::result::Result<Value, ServiceError> {
        let message = json!({"method":method,"params":params});
        if notification {
            let notification = serde_json::from_value(message).map_err(|_| {
                ServiceError::McpError(rmcp::ErrorData::invalid_params(
                    "Invalid notification",
                    None,
                ))
            })?;
            self.service.send_notification(notification).await?;
            Ok(Value::Null)
        } else {
            let request = serde_json::from_value(message).map_err(|_| {
                ServiceError::McpError(rmcp::ErrorData::invalid_params("Invalid request", None))
            })?;
            self.service.send_request(request).await.and_then(|r| {
                serde_json::to_value(r).map_err(|_| {
                    ServiceError::McpError(rmcp::ErrorData::internal_error(
                        "Invalid response",
                        None,
                    ))
                })
            })
        }
    }
    async fn close(self) {
        let _ = self.service.cancel().await;
    }
}
enum Reply {
    Data(Value),
    Protocol(rmcp::ErrorData),
}
struct Command {
    method: String,
    params: Value,
    notification: bool,
    timeout: Duration,
    result: mpsc::SyncSender<Result<Reply>>,
}
pub struct Client {
    sender: Option<mpsc::Sender<Command>>,
    closed: mpsc::Receiver<()>,
    deadline: Instant,
}
impl Client {
    pub fn open(url: &str, token: &str, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        validate_endpoint(url)?;
        let (sender, receiver) = mpsc::channel::<Command>();
        let (ready, opened) = mpsc::sync_channel(1);
        let (closed, stopped) = mpsc::sync_channel(1);
        let url = url.to_owned();
        let token = token.to_owned();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(_) => {
                    let _ = ready.send(Err(err("unavailable")));
                    return;
                }
            };
            runtime.block_on(async move {
                let client = match AsyncClient::open(&url, &token, timeout).await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let _ = ready.send(Ok(()));
                run_commands(client, receiver).await;
            });
            let _ = closed.send(());
        });
        opened
            .recv_timeout(timeout)
            .map_err(|_| err("unavailable"))??;
        Ok(Self {
            sender: Some(sender),
            closed: stopped,
            deadline,
        })
    }
    pub fn rpc(&mut self, method: &str, params: Value, notification: bool) -> Result<Value> {
        match self.request(method, params, notification)? {
            Reply::Data(v) => Ok(v),
            Reply::Protocol(_) => Err(err("invalid_response")),
        }
    }
    fn request(&mut self, method: &str, params: Value, notification: bool) -> Result<Reply> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| err("unavailable"))?;
        let (result, receiver) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or_else(|| err("unavailable"))?
            .send(Command {
                method: method.into(),
                params,
                notification,
                timeout: remaining,
                result,
            })
            .map_err(|_| err("unavailable"))?;
        match receiver.recv_timeout(remaining) {
            Ok(r) => r,
            Err(_) => {
                self.sender.take();
                Err(err("unavailable"))
            }
        }
    }
    pub fn raw(&mut self, request: Value) -> Result<Value> {
        let notification = request.get("id").is_none();
        let r = self.request(
            request["method"].as_str().unwrap_or(""),
            request.get("params").cloned().unwrap_or(json!({})),
            notification,
        )?;
        if notification {
            Ok(Value::Null)
        } else {
            Ok(match r {
                Reply::Data(v) => json!({"jsonrpc":"2.0","id":request["id"],"result":v}),
                Reply::Protocol(e) => json!({"jsonrpc":"2.0","id":request["id"],"error":e}),
            })
        }
    }
    pub fn call(&mut self, name: &str, args: Value) -> Result<Value> {
        let r = self.rpc("tools/call", json!({"name":name,"arguments":args}), false)?;
        if r["isError"] == true {
            return Err(err(r["structuredContent"]["error"]["code"]
                .as_str()
                .unwrap_or("unavailable")));
        }
        r.get("structuredContent")
            .cloned()
            .ok_or_else(|| err("invalid_response"))
    }
    pub fn reset_deadline(&mut self, timeout: Duration) {
        self.deadline = Instant::now() + timeout;
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        // Closing the command queue lets the dedicated reactor finish DELETE.
        // Never join its OS thread or extend the caller's request budget.
        self.sender.take();
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            let _ = self
                .closed
                .recv_timeout(remaining.min(Duration::from_millis(100)));
        }
    }
}
async fn run_commands(client: AsyncClient, mut receiver: mpsc::Receiver<Command>) {
    loop {
        let received = tokio::task::spawn_blocking(move || (receiver.recv(), receiver)).await;
        let Ok((command, returned)) = received else {
            break;
        };
        receiver = returned;
        let Ok(command) = command else { break };
        let result = tokio::time::timeout(
            command.timeout,
            client.rpc(&command.method, command.params, command.notification),
        )
        .await;
        let result = match result {
            Ok(Ok(v)) => Ok(Reply::Data(v)),
            Ok(Err(ServiceError::McpError(e))) => Ok(Reply::Protocol(e)),
            _ => Err(err("unavailable")),
        };
        let failed = result.is_err();
        let _ = command.result.send(result);
        if failed {
            break;
        }
    }
    client.close().await;
}
pub fn validate_endpoint(url: &str) -> Result<()> {
    let u = reqwest::Url::parse(url).map_err(|_| err("invalid_endpoint"))?;
    if u.scheme() != "http"
        || !matches!(u.host_str(), Some("127.0.0.1" | "localhost"))
        || u.path() != "/mcp"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(err("invalid_endpoint"));
    }
    Ok(())
}
#[derive(Clone)]
struct Relay {
    peer: rmcp::service::Peer<RoleClient>,
}
impl rmcp::ServerHandler for Relay {
    async fn ping(
        &self,
        _: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<(), rmcp::ErrorData> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.peer
                .send_request(serde_json::from_value(json!({"method":"ping"})).unwrap()),
        )
        .await
        .map_err(|_| rmcp::ErrorData::internal_error("unavailable", None))?
        .map(|_| ())
        .map_err(relay_error)
    }
    fn get_info(&self) -> ServerConfig {
        crate::server::server_info()
    }
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Owned(vec![ProtocolVersion::V_2025_11_25])
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<ListToolsResult, rmcp::ErrorData> {
        tokio::time::timeout(Duration::from_secs(5), self.peer.list_tools(request))
            .await
            .map_err(|_| rmcp::ErrorData::internal_error("unavailable", None))?
            .map_err(relay_error)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<CallToolResponse, rmcp::ErrorData> {
        let mut value = serde_json::to_value(request)
            .map_err(|_| rmcp::ErrorData::invalid_params("Invalid tool request", None))?;
        value["_meta"] = serde_json::to_value(ctx.meta).unwrap();
        let request = serde_json::from_value(value)
            .map_err(|_| rmcp::ErrorData::invalid_params("Invalid tool request", None))?;
        tokio::time::timeout(Duration::from_secs(5), self.peer.call_tool(request))
            .await
            .map_err(|_| rmcp::ErrorData::internal_error("unavailable", None))?
            .map(Into::into)
            .map_err(relay_error)
    }
}
fn relay_error(error: ServiceError) -> rmcp::ErrorData {
    match error {
        ServiceError::McpError(e) => e,
        _ => rmcp::ErrorData::internal_error("unavailable", None),
    }
}
pub async fn stdio(url: &str, token: &str) -> Result<()> {
    use std::io::Read;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (reader, mut writer) = tokio::io::duplex(MAX_RESPONSE);
    let (relay_output, output_reader) = tokio::io::duplex(MAX_RESPONSE);
    let (protocol_errors, mut error_receiver) = tokio::sync::mpsc::channel::<String>(16);
    let runtime = tokio::runtime::Handle::current();
    let (finished, mut input_result) = tokio::sync::oneshot::channel();
    let (first, first_input) = tokio::sync::oneshot::channel();
    // A detached OS reader avoids Tokio stdin's uncancellable blocking task.
    // Process shutdown must not wait for a client that keeps stdin open.
    std::thread::spawn(move || {
        let mut first = Some(first);
        let mut initialized = false;
        let result = (|| {
            let mut stdin = std::io::stdin().lock();
            let mut pending = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let count = stdin.read(&mut chunk)?;
                if count == 0 {
                    if !pending.is_empty() {
                        return Err(err("invalid_argument"));
                    }
                    break;
                }
                for byte in &chunk[..count] {
                    pending.push(*byte);
                    if pending.len() > MAX_BODY {
                        return Err(err("invalid_argument"));
                    }
                    if *byte == b'\n' {
                        let mut request = decode(&pending)?;
                        if !supported_rpc_id(&request) {
                            runtime
                                .block_on(protocol_errors.send(format!(
                                    "{}\n",
                                    wire_json(&json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Request id must be a signed 64-bit integer or string"}}))
                                )))
                                .map_err(|_| err("unavailable"))?;
                            if let Some(sender) = first.take() {
                                let _ = sender.send(Ok(true));
                            }
                            pending.clear();
                            continue;
                        }
                        if !initialized {
                            if request["method"] != "initialize" {
                                return Err(err("invalid_argument"));
                            }
                            initialized = true;
                            if let Some(sender) = first.take() {
                                let _ = sender.send(Ok(true));
                            }
                        }
                        crate::common::normalize_initialize(&mut request);
                        let encoded = format!("{}\n", wire_json(&request));
                        runtime.block_on(writer.write_all(encoded.as_bytes()))?;
                        pending.clear();
                    }
                }
            }
            Ok::<(), Error>(())
        })();
        if let Some(sender) = first.take() {
            let _ = sender.send(result.as_ref().map(|_| false).map_err(Clone::clone));
        }
        let _ = finished.send(result);
    });
    if !first_input.await.map_err(|_| err("invalid_argument"))?? {
        return Ok(());
    }
    let remote = AsyncClient::open(url, token, Duration::from_secs(5)).await?;
    let relay = Relay {
        peer: remote.service.peer().clone(),
    };
    let output_task = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(output_reader);
        let mut stdout = tokio::io::stdout();
        let mut line = Vec::new();
        let mut errors_open = true;
        loop {
            tokio::select! {
                result = reader.read_until(b'\n', &mut line) => {
                    let count = result?;
                    if count == 0 {
                        // Invalid input can be followed immediately by EOF, before
                        // the SDK starts a session. Deliver errors already queued.
                        while let Ok(error) = error_receiver.try_recv() {
                            stdout.write_all(error.as_bytes()).await?;
                        }
                        break;
                    }
                    stdout.write_all(&line).await?;
                    line.clear();
                }
                error = error_receiver.recv(), if errors_open => {
                    if let Some(error) = error {
                        stdout.write_all(error.as_bytes()).await?;
                    } else {
                        errors_open = false;
                    }
                }
            }
        }
        stdout.flush().await?;
        Ok::<(), std::io::Error>(())
    });
    let result = match relay.serve((reader, relay_output)).await {
        Ok(service) => service.waiting().await.map_err(|_| err("unavailable")),
        Err(_) => Err(err("invalid_argument")),
    };
    output_task.await.map_err(|_| err("unavailable"))??;
    remote.close().await;
    if let Ok(input) = input_result.try_recv() {
        input?;
    }
    result.map(|_| ())
}
