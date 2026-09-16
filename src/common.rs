use serde::{
    Deserialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

pub const MAX_BODY: usize = 2 * 1024 * 1024;
pub const PROTOCOL: &str = "2025-11-25";
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);
pub type Result<T> = std::result::Result<T, Error>;
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        err("io_error")
    }
}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        err("storage_error")
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        err("invalid_argument")
    }
}
pub fn err(s: &str) -> Error {
    Error(s.into())
}
pub fn uid() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub fn canonical(v: &Value) -> String {
    String::from_utf8(serde_json_canonicalizer::to_vec(v).expect("JSON serializes")).expect("UTF8")
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn digest(v: &Value) -> String {
    hash(canonical(v).as_bytes())
}
pub fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| err("invalid_argument"))
}
pub fn array<'a>(v: &'a Value, k: &str) -> Result<&'a Vec<Value>> {
    v.get(k)
        .and_then(Value::as_array)
        .ok_or_else(|| err("invalid_argument"))
}
// Reject duplicate object keys rather than silently using the final value.
struct Strict(Value);
impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Strict;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Strict, E> {
                Ok(Strict(json!(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Strict, E> {
                Ok(Strict(json!(v)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Strict, E> {
                Ok(Strict(json!(v)))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Strict, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| Strict(Value::Number(n)))
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Strict, E> {
                Ok(Strict(json!(v)))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Strict, A::Error> {
                let mut v = vec![];
                while let Some(x) = a.next_element::<Strict>()? {
                    v.push(x.0)
                }
                Ok(Strict(json!(v)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Strict, A::Error> {
                let mut v = serde_json::Map::new();
                while let Some((k, x)) = a.next_entry::<String, Strict>()? {
                    if v.insert(k, x.0).is_some() {
                        return Err(de::Error::custom("duplicate key"));
                    }
                }
                Ok(Strict(Value::Object(v)))
            }
        }
        d.deserialize_any(V)
    }
}
pub fn decode(raw: &[u8]) -> Result<Value> {
    if raw.len() > MAX_BODY {
        return Err(err("budget_exceeded"));
    }
    Ok(serde_json::from_slice::<Strict>(raw)?.0)
}
pub fn private_json(path: &Path) -> Result<Value> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.uid() != unsafe { libc::getuid() } || m.mode() & 0o077 != 0 {
        return Err(err("unsafe_config_permissions"));
    }
    let mut b = vec![];
    f.take(MAX_BODY as u64 + 1).read_to_end(&mut b)?;
    decode(&b)
}
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| err("invalid_config"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".publish-{}", uid()));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::hard_link(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(tmp);
    result
}
pub fn validate(v: &Value, s: &Value) -> Result<()> {
    let bad = || err("invalid_argument");
    let typ = s["type"].as_str().unwrap_or("");
    let valid = match typ {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "integer" => v.as_i64().is_some(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "" => true,
        _ => false,
    };
    if !valid {
        return Err(bad());
    }
    if let Some(e) = s["enum"].as_array()
        && !e.contains(v)
    {
        return Err(bad());
    }
    if let Some(c) = s.get("const")
        && c != v
    {
        return Err(err(if *c == 1 {
            "unsupported_schema_version"
        } else {
            "invalid_argument"
        }));
    }
    match typ {
        "object" => {
            let m = v.as_object().unwrap();
            if let Some(required) = s["required"].as_array() {
                for k in required {
                    if !m.contains_key(k.as_str().ok_or_else(bad)?) {
                        return Err(bad());
                    }
                }
            }
            for (k, x) in m {
                if let Some(p) = s["properties"].get(k) {
                    validate(x, p)?
                } else if s["additionalProperties"] == false {
                    return Err(bad());
                }
            }
        }
        "array" => {
            let a = v.as_array().unwrap();
            if a.len() < s["minItems"].as_u64().unwrap_or(0) as usize
                || a.len() > s["maxItems"].as_u64().unwrap_or(10000) as usize
            {
                return Err(bad());
            }
            for x in a {
                validate(x, &s["items"])?
            }
        }
        "string" => {
            let t = v.as_str().unwrap();
            let n = t.chars().count();
            if n < s["minLength"].as_u64().unwrap_or(0) as usize
                || n > s["maxLength"].as_u64().unwrap_or(MAX_BODY as u64) as usize
            {
                return Err(bad());
            }
            if let Some(p) = s["pattern"].as_str()
                && !regex::Regex::new(&format!("^(?:{p})$"))
                    .map_err(|_| bad())?
                    .is_match(t)
            {
                return Err(bad());
            }
        }
        "integer" => {
            let n = v.as_i64().unwrap();
            if n < s["minimum"].as_i64().unwrap_or(-9007199254740991)
                || n > s["maximum"].as_i64().unwrap_or(9007199254740991)
            {
                return Err(bad());
            }
        }
        _ => {}
    }
    Ok(())
}
pub fn authorize(p: &Value, s: &Value) -> Result<()> {
    let kind = text(s, "kind")?;
    let mut keys = BTreeSet::from(["kind", "owner_id"]);
    if kind != "user" {
        keys.insert("workspace_id");
    }
    if kind == "thread" {
        keys.insert("thread_id");
    }
    if !matches!(kind, "user" | "workspace" | "thread")
        || s.as_object()
            .map(|o| o.keys().map(String::as_str).collect::<BTreeSet<_>>())
            != Some(keys)
        || s["owner_id"] != p["owner_id"]
    {
        return Err(err("scope_denied"));
    }
    if array(p, "scopes")?.iter().any(|g| {
        g == s
            || (g["kind"] == "workspace"
                && kind == "thread"
                && g["owner_id"] == s["owner_id"]
                && g["workspace_id"] == s["workspace_id"])
    }) {
        Ok(())
    } else {
        Err(err("scope_denied"))
    }
}
pub fn scrub(v: &Value) -> Value {
    use std::sync::LazyLock;
    static KEY: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)(password|secret|credential|authorization|api[_-]?key|access[_-]?token|reasoning|thinking)").unwrap()
    });
    static RULES: LazyLock<Vec<(regex::Regex, &str)>> = LazyLock::new(|| {
        vec![
            (
                regex::Regex::new(
                    r"(?is)-----BEGIN [^-]*PRIVATE KEY-----.*?-----END [^-]*PRIVATE KEY-----",
                )
                .unwrap(),
                "[REDACTED]",
            ),
            (
                regex::Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/-]+=*").unwrap(),
                "Bearer [REDACTED]",
            ),
            (
                regex::Regex::new(r"\b(?:sk-[A-Za-z0-9_-]{12,}|gh[pousr]_[A-Za-z0-9]{16,})\b")
                    .unwrap(),
                "[REDACTED]",
            ),
            (
                regex::Regex::new(
                    r"(?i)((?:password|api[_-]?key|secret|access[_-]?token)\s*[:=]\s*)[^\s,;]+",
                )
                .unwrap(),
                "${1}[REDACTED]",
            ),
        ]
    });
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    (
                        k.clone(),
                        if KEY.is_match(k) {
                            json!("[REDACTED]")
                        } else {
                            scrub(x)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(a) => json!(a.iter().map(scrub).collect::<Vec<_>>()),
        Value::String(s) => {
            let mut s = s.clone();
            for (r, to) in RULES.iter() {
                s = r.replace_all(&s, *to).into_owned()
            }
            json!(s)
        }
        _ => v.clone(),
    }
}
