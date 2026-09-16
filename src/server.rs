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
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
struct Session {
    principal: String,
    initialized: bool,
    expires: Instant,
}
struct Inner {
    config_path: PathBuf,
    store: Mutex<Store>,
    sessions: Mutex<HashMap<String, Session>>,
    port: u16,
    _lock: File,
    slots: Arc<tokio::sync::Semaphore>,
}
fn response(code: u16, value: Option<Value>, session: Option<&str>) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(code).unwrap())
        .header("Content-Type", "application/json")
        .header("Cache-Control", "no-store");
    if let Some(s) = session {
        builder = builder.header("MCP-Session-Id", s)
    }
    builder
        .body(Body::from(value.map(|v| canonical(&v)).unwrap_or_default()))
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
    fn dispatch(&self, p: Value, h: HeaderMap, request: Value) -> Response {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        if !request.is_object()
            || request["jsonrpc"] != "2.0"
            || !request["method"].is_string()
            || (!id.is_null() && !id.is_string() && id.as_i64().is_none())
        {
            return response(
                400,
                Some(rpc_error(Value::Null, -32600, "Invalid request")),
                None,
            );
        }
        let method = request["method"].as_str().unwrap();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        if !params.is_object() {
            return response(400, None, None);
        }
        if method == "initialize" {
            if id.is_null()
                || !params["protocolVersion"].is_string()
                || !params["capabilities"].is_object()
                || !params["clientInfo"].is_object()
            {
                return response(400, None, None);
            }
            let sid = format!("{}{}", uid(), uid());
            let mut sessions = self.sessions.lock().unwrap();
            sessions.retain(|_, s| s.expires > Instant::now());
            if sessions.len() >= 1024 {
                return response(503, None, None);
            }
            sessions.insert(
                sid.clone(),
                Session {
                    principal: p["id"].as_str().unwrap().into(),
                    initialized: false,
                    expires: Instant::now() + Duration::from_secs(3600),
                },
            );
            return response(
                200,
                Some(
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":PROTOCOL,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"TekesMemory","version":env!("CARGO_PKG_VERSION")}}}),
                ),
                Some(&sid),
            );
        }
        {
            let mut sessions = self.sessions.lock().unwrap();
            let sid = header(&h, "MCP-Session-Id");
            let Some(session) = sessions.get_mut(sid) else {
                return response(if sid.is_empty() { 400 } else { 404 }, None, None);
            };
            if session.principal != p["id"] || session.expires <= Instant::now() {
                return response(404, None, None);
            }
            let version = header(&h, "MCP-Protocol-Version");
            if !version.is_empty() && version != PROTOCOL {
                return response(400, None, None);
            }
            if method == "notifications/initialized" && id.is_null() {
                session.initialized = true;
                return response(202, None, None);
            }
            if !session.initialized {
                return response(400, None, None);
            }
        }
        if id.is_null() {
            return response(202, None, None);
        }
        let result = match method {
            "ping" => json!({}),
            "tools/list" => json!({"tools":tools_for(&p)}),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                if !name.starts_with("memory.") || !SCHEMAS.contains_key(name) {
                    return response(200, Some(rpc_error(id, -32602, "Unknown tool")), None);
                }
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.store.lock().unwrap().call(&p, name, &args) {
                    Ok(data) => {
                        json!({"structuredContent":data,"content":[{"type":"text","text":canonical(&data)}],"isError":false})
                    }
                    Err(e) => {
                        json!({"structuredContent":{"schema_version":1,"error":{"code":e.0}},"content":[{"type":"text","text":e.0}],"isError":true})
                    }
                }
            }
            _ => return response(200, Some(rpc_error(id, -32601, "Method not found")), None),
        };
        response(
            200,
            Some(json!({"jsonrpc":"2.0","id":id,"result":result})),
            None,
        )
    }
}
async fn handle(State(s): State<Arc<Inner>>, request: Request) -> Response {
    let Ok(_permit) = s.slots.clone().try_acquire_owned() else {
        return response(503, None, None);
    };
    let p = match s.auth(request.headers()) {
        Ok(p) => p,
        Err(code) => return response(code, None, None),
    };
    let h = request.headers().clone();
    match request.method().as_str() {
        "GET" => return response(405, None, None),
        "DELETE" => {
            let mut sessions = s.sessions.lock().unwrap();
            let sid = header(&h, "MCP-Session-Id");
            if sessions.get(sid).is_none_or(|x| x.principal != p["id"]) {
                return response(404, None, None);
            }
            sessions.remove(sid);
            return response(200, None, None);
        }
        "POST" => {}
        _ => return response(405, None, None),
    }
    if header(&h, "Content-Type")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        != "application/json"
    {
        return response(415, None, None);
    }
    let accept = header(&h, "Accept");
    if !accept.contains("application/json") || !accept.contains("text/event-stream") {
        return response(406, None, None);
    }
    if h.contains_key("Transfer-Encoding") {
        return response(400, None, None);
    }
    let length = match header(&h, "Content-Length").parse::<usize>() {
        Ok(n) => n,
        Err(_) => return response(400, None, None),
    };
    if length == 0 || length > MAX_BODY {
        return response(413, None, None);
    }
    let raw = match tokio::time::timeout(
        Duration::from_secs(3),
        to_bytes(request.into_body(), MAX_BODY),
    )
    .await
    {
        Ok(Ok(b)) => b,
        _ => return response(400, None, None),
    };
    if raw.len() != length {
        return response(400, None, None);
    }
    let value = match decode(&raw) {
        Ok(v) => v,
        Err(_) => {
            return response(
                400,
                Some(rpc_error(Value::Null, -32700, "Parse error")),
                None,
            );
        }
    };
    tokio::task::spawn_blocking(move || s.dispatch(p, h, value))
        .await
        .unwrap_or_else(|_| {
            response(
                500,
                Some(rpc_error(Value::Null, -32603, "Internal error")),
                None,
            )
        })
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
    let router = Router::new()
        .route("/mcp", any(handle))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(inner);
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal");
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
        })
        .await;
    let _ = stop.send(true);
    let _ = maintenance.await;
    result.map_err(Into::into)
}
