//! Trusted, versioned HTTP security provider. No remote errors or payloads enter logs.
use crate::common::*;
use serde_json::{Value, json};
use std::{io::Read, path::Path, time::Duration};

pub struct SecurityProvider {
    config: Value,
    token: String,
    client: reqwest::blocking::Client,
    pub store_id: String,
}
impl SecurityProvider {
    pub fn new(config: &Value, store_id: String) -> Result<Self> {
        let url =
            reqwest::Url::parse(text(config, "endpoint")?).map_err(|_| err("invalid_config"))?;
        if config["version"] != 1
            || url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.path() != "/security/v1"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(err("invalid_config"));
        }
        let credential = Path::new(text(config, "credential_file")?);
        if !credential.is_absolute() {
            return Err(err("invalid_config"));
        }
        let token = text(&private_json(credential)?, "token")?.to_owned();
        let timeout = config["timeout_ms"]
            .as_u64()
            .filter(|v| (100..=30000).contains(v))
            .ok_or_else(|| err("invalid_config"))?;
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(timeout))
            .build()
            .map_err(|_| err("security_unavailable"))?;
        Ok(Self {
            config: config.clone(),
            token,
            client,
            store_id,
        })
    }
    pub fn binding(config: &Value) -> Value {
        json!({"version":1,"provider_id":config["provider_id"],"profile":config["profile"],
            "policy_version":config["policy_version"],"purpose":config["purpose"]})
    }
    pub fn request(&self, action: &str, input: Value) -> Result<Value> {
        let mut request = Self::binding(&self.config);
        request["store_id"] = json!(self.store_id);
        request["action"] = json!(action);
        request["input"] = input;
        let raw = serde_json::to_vec(&request)?;
        if raw.len() > MAX_RESPONSE {
            return Err(err("budget_exceeded"));
        }
        let response = self
            .client
            .post(text(&self.config, "endpoint")?)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json")
            .body(raw)
            .send()
            .map_err(|_| err("security_unavailable"))?;
        if !response.status().is_success() {
            return Err(err("security_unavailable"));
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_RESPONSE as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| err("security_unavailable"))?;
        if bytes.len() > MAX_RESPONSE {
            return Err(err("security_protocol_error"));
        }
        let response: Value =
            serde_json::from_slice(&bytes).map_err(|_| err("security_protocol_error"))?;
        if response["version"] != 1 {
            return Err(err("security_protocol_error"));
        }
        if response["ok"] != true {
            return Err(err(match response["code"].as_str() {
                Some(
                    code @ ("security_denied"
                    | "security_key_unavailable"
                    | "security_unknown_key_version"
                    | "security_payload_invalid"
                    | "security_restore_required"
                    | "security_unavailable"),
                ) => code,
                _ => "security_protocol_error",
            }));
        }
        response
            .get("output")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or_else(|| err("security_protocol_error"))
    }
    pub fn allow(&self, action: &str, input: Value) -> Result<Value> {
        let output = self.request(action, input)?;
        if output["allow"] != true {
            return Err(err("security_denied"));
        }
        Ok(output)
    }
    pub fn seal(&self, context: &Value, value: &Value) -> Result<String> {
        let output = self.request("seal", json!({"context":context,"value":value}))?;
        let envelope = &output["envelope"];
        if !envelope.is_object() || envelope["key_version"].as_str().is_none_or(str::is_empty) {
            return Err(err("security_protocol_error"));
        }
        Ok(canonical(
            &json!({"security_v":1,"context":context,"envelope":envelope}),
        ))
    }
    pub fn open(&self, raw: &str, expected: Option<&Value>) -> Result<Value> {
        let stored: Value = serde_json::from_str(raw)?;
        if stored["security_v"] != 1 || expected.is_some_and(|v| v != &stored["context"]) {
            return Err(err("security_payload_invalid"));
        }
        self.request(
            "open",
            json!({"context":stored["context"],"envelope":stored["envelope"]}),
        )?
        .get("value")
        .cloned()
        .ok_or_else(|| err("security_protocol_error"))
    }
}
