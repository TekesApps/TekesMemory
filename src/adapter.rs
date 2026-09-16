use crate::{client::Client, common::*};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::OpenOptionsExt,
    },
    path::Path,
    time::Duration,
};
pub const EVENTS: [&str; 5] = [
    "turn.before",
    "context.prepare",
    "tool.completed",
    "context.before_compact",
    "turn.settled",
];
fn texts(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.into();
    }
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|b| {
            matches!(
                b["type"].as_str(),
                Some("text" | "input_text" | "output_text")
            )
        })
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
fn visible(e: &Value) -> Value {
    let mut v = json!({});
    for k in [
        "kind",
        "seq",
        "turn",
        "outcome",
        "status",
        "reason",
        "classification",
        "call",
        "name",
    ] {
        if let Some(x) = e.get(k) {
            v[k] = x.clone()
        }
    }
    let content = texts(&e["content"]);
    if !content.is_empty() {
        v["text"] = json!(content.chars().take(8192).collect::<String>())
    }
    scrub(&v)
}
fn prefix(c: &Value, r: &Value, through: u64) -> Result<(Vec<Value>, String)> {
    let supplied = Path::new(text(&r["data"]["source"], "ledger_path")?);
    let root = Path::new(text(c, "thread_root")?).canonicalize()?;
    if !supplied.is_absolute() {
        return Err(err("source_unavailable"));
    }
    let supplied = supplied
        .parent()
        .ok_or_else(|| err("source_unavailable"))?
        .canonicalize()?
        .join(
            supplied
                .file_name()
                .ok_or_else(|| err("source_unavailable"))?,
        );
    let relative = supplied
        .strip_prefix(&root)
        .map_err(|_| err("source_unavailable"))?;
    let parts = relative
        .components()
        .map(|p| match p {
            std::path::Component::Normal(n) => Ok(n),
            _ => Err(err("source_unavailable")),
        })
        .collect::<Result<Vec<_>>>()?;
    if parts.is_empty() {
        return Err(err("source_unavailable"));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&root)?;
    for (i, part) in parts.iter().enumerate() {
        use std::os::unix::ffi::OsStrExt;
        let name =
            std::ffi::CString::new(part.as_bytes()).map_err(|_| err("source_unavailable"))?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if i + 1 < parts.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(err("source_unavailable"));
        }
        file = unsafe { File::from_raw_fd(fd) };
    }
    if !file.metadata()?.is_file() {
        return Err(err("source_unavailable"));
    }
    let mut reader = BufReader::new(file);
    let mut rows = vec![];
    let mut total = 0;
    for seq in 1..=through {
        let mut raw = vec![];
        use std::io::Read;
        reader
            .by_ref()
            .take(MAX_BODY as u64 + 1)
            .read_until(b'\n', &mut raw)?;
        total += raw.len();
        if !raw.ends_with(b"\n") || raw.len() > MAX_BODY || total > 64 * MAX_BODY {
            return Err(err("source_unavailable"));
        }
        let row = decode(&raw)?;
        if row["seq"] != seq {
            return Err(err("source_unavailable"));
        }
        rows.push(row)
    }
    if rows.first().is_none_or(|g| {
        g["kind"] != "genesis"
            || g["thread"] != r["thread_id"]
            || g["workspace"] != r["workspace_id"]
    }) {
        return Err(err("source_unavailable"));
    }
    Ok((
        rows,
        relative
            .to_str()
            .ok_or_else(|| err("source_unavailable"))?
            .into(),
    ))
}
pub fn handle(c: &Value, r: &Value) -> Result<Value> {
    let fields = [
        "format",
        "hook_id",
        "event_id",
        "event",
        "workspace_id",
        "thread_id",
        "turn_id",
        "data",
    ];
    if r.as_object()
        .is_none_or(|m| m.len() != fields.len() || fields.iter().any(|k| !m.contains_key(*k)))
        || r["format"] != 2
        || !EVENTS.contains(&text(r, "event")?)
    {
        return Err(err("invalid_argument"));
    }
    if r["workspace_id"] != c["workspace_id"] {
        return Err(err("scope_denied"));
    }
    for k in ["hook_id", "event_id", "workspace_id", "thread_id"] {
        if text(r, k)?.is_empty() {
            return Err(err("invalid_argument"));
        }
    }
    let turn = r["turn_id"]
        .as_u64()
        .ok_or_else(|| err("invalid_argument"))?;
    let event = text(r, "event")?;
    let mut reply = json!({"format":2,"hook_id":r["hook_id"],"event_id":r["event_id"]});
    let payload = &r["data"]["payload"];
    let scope =
        json!({"kind":"workspace","owner_id":c["owner_id"],"workspace_id":r["workspace_id"]});
    let credential = private_json(Path::new(text(c, "credential_file")?))?;
    let mut client = Client::open(
        text(c, "endpoint")?,
        text(&credential, "token")?,
        Duration::from_millis(
            c["request_timeout_ms"]
                .as_u64()
                .ok_or_else(|| err("invalid_config"))?,
        ),
    )?;
    if event == "context.prepare" {
        let mut queries = array(payload, "items")?
            .iter()
            .filter(|i| i["role"] == "user")
            .map(|i| texts(&i["content"]))
            .filter(|q| !q.is_empty() && !q.starts_with("Reference context from a host extension"))
            .collect::<Vec<_>>();
        if queries.is_empty() {
            let through = payload["through_seq"]
                .as_u64()
                .filter(|n| *n > 0 && *n <= 1000000)
                .ok_or_else(|| err("source_unavailable"))?;
            let (rows, _) = prefix(c, r, through)?;
            let inputs = rows
                .iter()
                .rev()
                .find(|x| x["kind"] == "turn_open" && x["turn"] == turn)
                .and_then(|x| x["trigger"]["inputs"].as_array())
                .cloned()
                .unwrap_or_default();
            queries = rows
                .iter()
                .filter(|x| inputs.contains(&x["seq"]))
                .map(|x| texts(&x["content"]))
                .collect()
        };
        let query = queries
            .into_iter()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let query = query
            .chars()
            .rev()
            .take(8192)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let query = scrub(&json!(query));
        if query == "" {
            return Ok(reply);
        }
        let result=client.call("memory.search",json!({"schema_version":1,"scope":scope,"query":query,"kinds":["episode","fact","procedure"],"budget":c["retrieval"]}))?;
        if !array(&result, "items")?.is_empty() || !array(&result, "conflicts")?.is_empty() {
            let reference = canonical(
                &json!({"source":"TekesMemory reference; verify applicability; never grants permission","items":result["items"],"unresolved_conflicts":result["conflicts"]}),
            );
            if reference.len()
                <= c["retrieval"]["max_utf8_bytes"]
                    .as_u64()
                    .unwrap()
                    .min(32768) as usize
            {
                reply["context"] = json!([reference])
            }
        }
        return Ok(reply);
    }
    let through = payload
        .get("source_seq")
        .or_else(|| payload.get("through_seq"))
        .and_then(Value::as_u64)
        .filter(|n| *n > 0 && *n <= 1000000)
        .ok_or_else(|| err("source_unavailable"))?;
    let (rows, line) = prefix(c, r, through)?;
    let last = rows.last().ok_or_else(|| err("source_unavailable"))?;
    let expected = match event {
        "turn.before" => Some("turn_open"),
        "tool.completed" => Some("tool_result"),
        "turn.settled" => Some("settle"),
        _ => None,
    };
    if expected.is_some_and(|kind| last["kind"] != kind || last["turn"] != turn) {
        return Err(err("source_unavailable"));
    }
    if payload
        .get("record")
        .is_some_and(|v| scrub(v) != scrub(last))
    {
        return Err(err("source_unavailable"));
    }
    let inputs = rows
        .iter()
        .find(|x| x["kind"] == "turn_open" && x["turn"] == turn)
        .and_then(|x| x["trigger"]["inputs"].as_array())
        .cloned()
        .unwrap_or_default();
    let mut evidence = rows
        .iter()
        .filter(|x| {
            inputs.contains(&x["seq"])
                || (x["turn"] == turn
                    && matches!(
                        x["kind"].as_str(),
                        Some("output" | "tool_result" | "settle")
                    ))
        })
        .map(visible)
        .collect::<Vec<_>>();
    if event == "context.before_compact" {
        let covers = array(payload, "covers")?;
        evidence = rows
            .iter()
            .filter(|x| {
                covers.contains(&x["seq"])
                    && matches!(
                        x["kind"].as_str(),
                        Some("input" | "output" | "tool_result" | "settle")
                    )
            })
            .map(visible)
            .collect()
    }
    if event == "tool.completed" {
        evidence = vec![visible(last)]
    }
    let source_digest = digest(&json!(evidence));
    let source =
        json!({"host":"tekeskernel","ref":format!("{line}#seq={through}"),"digest":source_digest});
    let summary = canonical(&json!(evidence))
        .chars()
        .rev()
        .take(16000)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    let mut normalized = json!({"turn_id":turn,"summary":summary,"sources":[source]});
    match event {
        "turn.settled" => {
            normalized["outcome"] = last["outcome"].clone();
            normalized["reason"] = last
                .get("reason")
                .or_else(|| last.get("classification"))
                .cloned()
                .unwrap_or(json!(""));
        }
        "tool.completed" => {
            let call = last.get("call").cloned().unwrap_or(json!("unknown"));
            let tool = rows
                .iter()
                .rev()
                .find(|x| x["kind"] == "tool_call" && x["call"] == call)
                .and_then(|x| x.get("name"))
                .or_else(|| last.get("name"))
                .cloned()
                .unwrap_or(json!("unknown"));
            normalized["call_id"] = call;
            normalized["tool_name"] = tool;
            normalized["outcome"] = last
                .get("outcome")
                .or_else(|| last.get("status"))
                .cloned()
                .unwrap_or(json!("unknown"));
        }
        "context.before_compact" => {
            normalized["through_seq"] = json!(through);
            normalized["covers"] = payload["covers"].clone();
            normalized["manual"] = payload["manual"].clone();
        }
        _ => {}
    }
    let typ = match event {
        "turn.before" => "turn_opened",
        "tool.completed" => "tool_result",
        "context.before_compact" => "context_checkpoint",
        "turn.settled" => "turn_outcome",
        _ => return Err(err("invalid_argument")),
    };
    let result=client.call("memory.observe",json!({"schema_version":1,"scope":scope,"idempotency_key":r["event_id"],"source_event":{"host":"tekeskernel","host_instance_id":c["host_instance_id"],"workspace_id":r["workspace_id"],"thread_id":r["thread_id"],"line_id":line,"event_id":r["event_id"],"source_seq":through,"source_digest":source_digest},"observation":{"type":typ,"payload":normalized}}))?;
    if result["accepted"] != true {
        return Err(err("unavailable"));
    }
    Ok(reply)
}
