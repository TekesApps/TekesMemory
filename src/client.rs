use crate::common::*;
use reqwest::{blocking::Client as Http, header::HeaderMap};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
pub struct Client {
    http: Http,
    url: String,
    token: String,
    session: Option<String>,
    deadline: Instant,
    counter: u64,
}
impl Client {
    pub fn open(url: &str, token: &str, timeout: Duration) -> Result<Self> {
        validate_endpoint(url)?;
        let http = Http::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| err("unavailable"))?;
        let mut c = Self {
            http,
            url: url.into(),
            token: token.into(),
            session: None,
            deadline: Instant::now() + timeout,
            counter: 0,
        };
        let r=c.rpc("initialize",json!({"protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"tekes-memory-rust","version":env!("CARGO_PKG_VERSION")}}),false)?;
        if r["protocolVersion"] != PROTOCOL {
            return Err(err("unsupported_protocol"));
        }
        c.rpc("notifications/initialized", json!({}), true)?;
        Ok(c)
    }
    fn headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        for (k, v) in [
            ("Authorization", format!("Bearer {}", self.token)),
            ("Content-Type", "application/json".into()),
            ("Accept", "application/json, text/event-stream".into()),
            ("MCP-Protocol-Version", PROTOCOL.into()),
        ] {
            h.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().map_err(|_| err("invalid_config"))?,
            );
        }
        if let Some(s) = &self.session {
            h.insert(
                "MCP-Session-Id",
                s.parse().map_err(|_| err("invalid_response"))?,
            );
        }
        Ok(h)
    }
    pub fn rpc(&mut self, method: &str, params: Value, notification: bool) -> Result<Value> {
        self.counter += 1;
        let mut request = json!({"jsonrpc":"2.0","method":method,"params":params});
        if !notification {
            request["id"] = json!(self.counter)
        }
        let mut result = self.raw(request)?;
        if notification {
            return Ok(Value::Null);
        }
        if result["id"] != self.counter || result.get("result").is_none() {
            return Err(err("invalid_response"));
        }
        Ok(result["result"].take())
    }
    pub fn raw(&mut self, request: Value) -> Result<Value> {
        use std::io::Read;
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| err("unavailable"))?;
        let response = self
            .http
            .post(&self.url)
            .headers(self.headers()?)
            .timeout(remaining)
            .body(canonical(&request))
            .send()
            .map_err(|_| err("unavailable"))?;
        if let Some(s) = response.headers().get("MCP-Session-Id") {
            self.session = Some(s.to_str().map_err(|_| err("invalid_response"))?.into())
        }
        let status = response.status();
        if !status.is_success() {
            return Err(err("unavailable"));
        }
        if request.get("id").is_none() {
            if status.as_u16() != 202 {
                return Err(err("invalid_response"));
            }
            return Ok(Value::Null);
        }
        let mut raw = vec![];
        response.take(MAX_BODY as u64 + 1).read_to_end(&mut raw)?;
        decode(&raw)
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
        if self.session.is_some()
            && let Ok(h) = self.headers()
        {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                let _ = self
                    .http
                    .delete(&self.url)
                    .headers(h)
                    .timeout(remaining.min(Duration::from_millis(100)))
                    .send();
            }
        }
    }
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
