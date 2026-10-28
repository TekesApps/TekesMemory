use crate::{
    common::*,
    config::{SCHEMAS, service_config, tools_for},
    store::Store,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::any,
};
use fs2::FileExt;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::*,
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::{SessionManager, local::LocalSessionManager},
    },
};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::HashMap,
    fs::{File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tower::ServiceExt;
struct Session {
    principal: String,
    initialized: bool,
    expires: Instant,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
struct Inner {
    config_path: PathBuf,
    store: Mutex<Store>,
    sessions: Mutex<HashMap<String, Session>>,
    port: u16,
    _lock: File,
    slots: Arc<tokio::sync::Semaphore>,
    session_slots: Arc<tokio::sync::Semaphore>,
}
#[derive(Clone)]
struct Principal(Value);
#[derive(Clone)]
struct MemoryHandler(Arc<Inner>);
pub(crate) fn server_info() -> ServerConfig {
    serde_json::from_value(json!({"protocolVersion":PROTOCOL,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"TekesMemory","version":env!("CARGO_PKG_VERSION")}})).unwrap()
}
fn principal(ctx: &RequestContext<RoleServer>) -> std::result::Result<Value, ErrorData> {
    ctx.extensions
        .get::<axum::http::request::Parts>()
        .and_then(|p| p.extensions.get::<Principal>())
        .map(|p| p.0.clone())
        .ok_or_else(|| ErrorData::internal_error("Missing authenticated principal", None))
}
impl ServerHandler for MemoryHandler {
    fn get_info(&self) -> ServerConfig {
        server_info()
    }
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(vec![ProtocolVersion::V_2025_11_25])
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        serde_json::from_value(json!({"tools":tools_for(&principal(&ctx)?)}))
            .map_err(|_| ErrorData::internal_error("Invalid tool definitions", None))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let p = principal(&ctx)?;
        let name = request.name.to_string();
        if !name.starts_with("memory.") || !SCHEMAS.contains_key(name.as_str()) {
            return Err(ErrorData::invalid_params("Unknown tool", None));
        }
        let args = request.arguments.map(Value::Object).unwrap_or(json!({}));
        let effect_key = ctx
            .meta
            .get("io.tekes/idempotencyKey")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let inner = self.0.clone();
        let result = tokio::task::spawn_blocking(move || {
            if name == "memory.reconcile_effect" {
                let may_write = [
                    "memory.save",
                    "memory.correct",
                    "memory.forget",
                    "memory.observe",
                ]
                .iter()
                .any(|tool| {
                    p["tools"]
                        .as_array()
                        .is_some_and(|tools| tools.contains(&json!(tool)))
                });
                if !may_write {
                    Err(err("scope_denied"))
                } else if let Err(e) = validate(&args, &SCHEMAS["memory.reconcile_effect"]) {
                    Err(e)
                } else {
                    inner
                        .store
                        .lock()
                        .unwrap()
                        .reconcile_effect(&p, args["idempotencyKey"].as_str().unwrap_or(""))
                }
            } else {
                inner.store.lock().unwrap().call_with_effect(
                    &p,
                    &name,
                    &args,
                    effect_key.as_deref(),
                )
            }
        })
        .await
        .map_err(|_| ErrorData::internal_error("Memory worker unavailable", None))?;
        let value = match result {
            Ok(data) => {
                json!({"structuredContent":data,"content":[{"type":"text","text":canonical(&data)}],"isError":false})
            }
            Err(e) => {
                json!({"structuredContent":{"schema_version":1,"error":{"code":e.0}},"content":[{"type":"text","text":e.0}],"isError":true})
            }
        };
        serde_json::from_value::<CallToolResult>(value)
            .map(Into::into)
            .map_err(|_| ErrorData::internal_error("Invalid tool result", None))
    }
}
fn response(code: u16, value: Option<Value>, session: Option<&str>) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(code).unwrap())
        .header("Content-Type", "application/json")
        .header("Cache-Control", "no-store");
    if let Some(s) = session {
        builder = builder.header("MCP-Session-Id", s);
    }
    builder
        .body(Body::from(value.map(|v| wire_json(&v)).unwrap_or_default()))
        .unwrap()
}
fn header<'a>(h: &'a HeaderMap, k: &str) -> &'a str {
    h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("")
}
fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
impl Inner {
    fn auth(&self, h: &HeaderMap) -> std::result::Result<Value, u16> {
        let host = header(h, "host");
        if host != format!("127.0.0.1:{}", self.port) && host != format!("localhost:{}", self.port)
        {
            return Err(403);
        }
        let c = service_config(&self.config_path).map_err(|_| 503u16)?;
        if let Some(o) = h.get("origin") {
            let origin = o.to_str().map_err(|_| 403u16)?;
            if !c["allowed_origins"]
                .as_array()
                .unwrap()
                .contains(&json!(origin))
            {
                return Err(403);
            }
        }
        let token = header(h, "authorization")
            .strip_prefix("Bearer ")
            .ok_or(401u16)?;
        let hash = hash(token.as_bytes());
        c["principals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| {
                bool::from(
                    hash.as_bytes()
                        .ct_eq(p["token_sha256"].as_str().unwrap().as_bytes()),
                )
            })
            .cloned()
            .ok_or(401)
    }
}
#[derive(Clone)]
struct HttpState {
    inner: Arc<Inner>,
    service: StreamableHttpService<MemoryHandler, LocalSessionManager>,
    manager: Arc<LocalSessionManager>,
    initialize_gate: Arc<tokio::sync::Mutex<()>>,
}
async fn handle(State(state): State<HttpState>, request: Request) -> Response {
    let s = &state.inner;
    let mut initialize_guard = None;
    let Ok(_permit) = s.slots.clone().try_acquire_owned() else {
        return response(503, None, None);
    };
    let p = match s.auth(request.headers()) {
        Ok(p) => p,
        Err(code) => return response(code, None, None),
    };
    if request.method() == axum::http::Method::GET {
        return response(405, None, None);
    };
    let sid = header(request.headers(), "MCP-Session-Id").to_owned();
    if !sid.is_empty() {
        let sessions = s.sessions.lock().unwrap();
        if sessions
            .get(&sid)
            .is_none_or(|v| v.principal != p["id"] || v.expires <= Instant::now())
        {
            return response(404, None, None);
        }
    }
    let method = request.method().clone();
    let (mut parts, body) = request.into_parts();
    parts.extensions.insert(Principal(p.clone()));
    let mut request_value = None;
    let mut session_permit = None;
    let body = if method == axum::http::Method::POST {
        if header(&parts.headers, "Content-Type")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            != "application/json"
        {
            return response(415, None, None);
        }
        if !header(&parts.headers, "Accept").contains("application/json")
            || !header(&parts.headers, "Accept").contains("text/event-stream")
        {
            return response(406, None, None);
        }
        if parts.headers.contains_key("Transfer-Encoding") {
            return response(400, None, None);
        }
        let length = match header(&parts.headers, "Content-Length").parse::<usize>() {
            Ok(n) => n,
            Err(_) => return response(400, None, None),
        };
        if length == 0 || length > MAX_BODY {
            return response(413, None, None);
        }
        let version = header(&parts.headers, "MCP-Protocol-Version");
        if !version.is_empty() && version != PROTOCOL {
            return response(400, None, None);
        }
        let raw = match tokio::time::timeout(Duration::from_secs(3), to_bytes(body, MAX_BODY)).await
        {
            Ok(Ok(raw)) => raw,
            _ => return response(400, None, None),
        };
        if raw.len() != length {
            return response(400, None, None);
        };
        let mut value = match decode(&raw) {
            Ok(value) => value,
            Err(_) => {
                return response(
                    400,
                    Some(rpc_error(Value::Null, -32700, "Parse error")),
                    None,
                );
            }
        };
        if !supported_rpc_id(&value) {
            return response(
                200,
                Some(rpc_error(
                    Value::Null,
                    -32600,
                    "Request id must be a signed 64-bit integer or string",
                )),
                None,
            );
        }
        if value["method"] == "initialize" {
            initialize_guard = Some(state.initialize_gate.lock().await);
            if state.manager.sessions.read().await.len() >= 1024 {
                return response(503, None, None);
            }
            session_permit = match s.session_slots.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => return response(503, None, None),
            };
            normalize_initialize(&mut value);
        }
        if value["method"] != "initialize" {
            let mut sessions = s.sessions.lock().unwrap();
            let Some(session) = sessions.get_mut(&sid) else {
                return response(if sid.is_empty() { 400 } else { 404 }, None, None);
            };
            if value["method"] == "notifications/initialized" && value.get("id").is_none() {
                // Initialization is committed only after rmcp accepts the notification.
            } else if !session.initialized {
                return response(400, None, None);
            }
        }
        let encoded = wire_json(&value);
        parts.headers.remove("transfer-encoding");
        parts
            .headers
            .insert("content-length", encoded.len().to_string().parse().unwrap());
        request_value = Some(value);
        Body::from(encoded)
    } else {
        body
    };
    let output = match state
        .service
        .oneshot(Request::from_parts(parts, body))
        .await
    {
        Ok(out) => out,
        Err(_) => return response(500, None, None),
    };
    let (mut parts, body) = output.into_parts();
    drop(initialize_guard);
    if parts.status.is_success()
        && request_value
            .as_ref()
            .is_some_and(|v| v["method"] == "notifications/initialized" && v.get("id").is_none())
        && let Some(session) = s.sessions.lock().unwrap().get_mut(&sid)
    {
        session.initialized = true;
    }
    if let Some(session) = parts
        .headers
        .get("MCP-Session-Id")
        .and_then(|v| v.to_str().ok())
        && let Some(permit) = session_permit.take()
    {
        s.sessions.lock().unwrap().insert(
            session.to_owned(),
            Session {
                principal: p["id"].as_str().unwrap().to_owned(),
                initialized: false,
                expires: Instant::now() + Duration::from_secs(3600),
                _permit: permit,
            },
        );
    }
    if method == axum::http::Method::DELETE && parts.status.is_success() {
        s.sessions.lock().unwrap().remove(&sid);
        parts.status = StatusCode::OK;
    }
    if header(&parts.headers, "Content-Type").starts_with("text/event-stream")
        && request_value
            .as_ref()
            .is_some_and(|v| v.get("id").is_some())
    {
        // rmcp owns dispatch and negotiation. This service has no outbound notifications:
        // adapt its one terminal request stream back to the established JSON HTTP contract.
        let raw = match tokio::time::timeout(
            Duration::from_secs(30),
            to_bytes(Body::new(body), MAX_RESPONSE),
        )
        .await
        {
            Ok(Ok(raw)) => raw,
            _ => return response(503, None, None),
        };
        let text = match std::str::from_utf8(&raw) {
            Ok(text) => text,
            Err(_) => return response(500, None, None),
        };
        let mut terminal = None;
        for frame in text.replace("\r\n", "\n").split("\n\n") {
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            let value = match decode_response(data.as_bytes()) {
                Ok(value) => value,
                Err(_) => return response(500, None, None),
            };
            if value.get("id") == request_value.as_ref().and_then(|v| v.get("id"))
                && (value.get("result").is_some() || value.get("error").is_some())
            {
                if terminal.replace(value).is_some() {
                    return response(500, None, None);
                }
            } else {
                return response(500, None, None);
            }
        }
        let Some(value) = terminal else {
            return response(500, None, None);
        };
        parts
            .headers
            .insert("content-type", "application/json".parse().unwrap());
        parts.headers.remove("content-length");
        parts
            .headers
            .insert("cache-control", "no-store".parse().unwrap());
        return Response::from_parts(parts, Body::from(wire_json(&value)));
    }
    let raw = match tokio::time::timeout(
        Duration::from_secs(30),
        to_bytes(Body::new(body), MAX_RESPONSE),
    )
    .await
    {
        Ok(Ok(raw)) => raw,
        _ => return response(503, None, None),
    };
    parts
        .headers
        .insert("cache-control", "no-store".parse().unwrap());
    Response::from_parts(parts, Body::from(raw))
}

pub async fn serve(config_path: &Path, ready: Option<&Path>) -> Result<()> {
    let c = service_config(config_path)?;
    let dir = Path::new(text(&c, "data_directory")?);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let m = std::fs::symlink_metadata(dir)?;
    if m.file_type().is_symlink() || m.uid() != unsafe { libc::getuid() } || m.mode() & 0o077 != 0 {
        return Err(err("unsafe_config_permissions"));
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("service.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|_| err("service_already_running"))?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("memory.sqlite3")
            && !entry.file_type()?.is_file()
        {
            return Err(err("unsafe_database_path"));
        }
    }
    let store = Store::open(
        &dir.join("memory.sqlite3"),
        c["episode_days"].as_i64().unwrap(),
    )?;
    let listener =
        tokio::net::TcpListener::bind(("127.0.0.1", c["port"].as_u64().unwrap() as u16)).await?;
    let port = listener.local_addr()?.port();
    let inner = Arc::new(Inner {
        config_path: config_path.canonicalize()?,
        store: Mutex::new(store),
        sessions: Mutex::new(HashMap::new()),
        port,
        _lock: lock,
        slots: Arc::new(tokio::sync::Semaphore::new(32)),
        session_slots: Arc::new(tokio::sync::Semaphore::new(1024)),
    });
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let worker = inner.clone();
    let maintenance = tokio::spawn(async move {
        loop {
            let s = worker.clone();
            let busy =
                tokio::task::spawn_blocking(move || s.store.lock().unwrap().maintain()).await;
            let delay = match busy {
                Ok(Ok(true)) => 10,
                Ok(Ok(false)) => 250,
                _ => {
                    eprintln!("memory maintenance failed; retrying");
                    250
                }
            };
            tokio::select! {_=stopped.changed()=>break,_=tokio::time::sleep(Duration::from_millis(delay))=>{}}
        }
    });
    if let Some(path) = ready {
        write_private(
            path,
            format!(
                "{}\n",
                canonical(&json!({"endpoint":format!("http://127.0.0.1:{port}/mcp")}))
            )
            .as_bytes(),
        )?;
    }
    let mut manager = LocalSessionManager::default();
    manager.session_config.keep_alive = Some(Duration::from_secs(3600));
    manager.session_config.sse_retry = None;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let mut transport_config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_sse_retry(None);
    transport_config.cancellation_token = cancellation.clone();
    let manager = Arc::new(manager);
    let cleanup_inner = inner.clone();
    let cleanup_manager = manager.clone();
    let cleanup_cancel = cancellation.clone();
    let session_cleanup = tokio::spawn(async move {
        let mut unbound = HashMap::new();
        loop {
            tokio::select! {_=cleanup_cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(250))=>{}}
            let entries = cleanup_manager
                .sessions
                .read()
                .await
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            {
                let mut sessions = cleanup_inner.sessions.lock().unwrap();
                sessions.retain(|sid, _| entries.iter().any(|id| id.as_ref() == sid));
            }
            unbound.retain(|sid, _| entries.contains(sid));
            for sid in entries {
                let expired = cleanup_inner
                    .sessions
                    .lock()
                    .unwrap()
                    .get(sid.as_ref())
                    .is_some_and(|s| s.expires <= Instant::now());
                let bound = cleanup_inner
                    .sessions
                    .lock()
                    .unwrap()
                    .contains_key(sid.as_ref());
                let orphan_expired = if bound {
                    unbound.remove(&sid);
                    false
                } else {
                    unbound
                        .entry(sid.clone())
                        .or_insert_with(Instant::now)
                        .elapsed()
                        >= Duration::from_secs(60)
                };
                if expired
                    || orphan_expired
                    || !cleanup_manager.has_session(&sid).await.unwrap_or(false)
                {
                    let _ = cleanup_manager.close_session(&sid).await;
                    cleanup_inner.sessions.lock().unwrap().remove(sid.as_ref());
                }
            }
        }
    });
    let service = StreamableHttpService::new(
        {
            let inner = inner.clone();
            move || Ok(MemoryHandler(inner.clone()))
        },
        manager.clone(),
        transport_config,
    );
    let state = HttpState {
        inner,
        service,
        manager,
        initialize_gate: Arc::new(tokio::sync::Mutex::new(())),
    };
    let router = Router::new()
        .route("/mcp", any(handle))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state);
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal");
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
        })
        .await;
    cancellation.cancel();
    let _ = session_cleanup.await;
    let _ = stop.send(true);
    let _ = maintenance.await;
    result.map_err(Into::into)
}
