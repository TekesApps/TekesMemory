use crate::{common::*, config::SCHEMAS};
use base64::{Engine, engine::general_purpose::URL_SAFE};
use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, params, types::Value as SqlValue};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub struct Store {
    pub db: Connection,
    pub episode_days: i64,
    pub clock: fn() -> i64,
}
pub fn words(s: &str) -> Vec<String> {
    use std::sync::LazyLock;
    static WORD: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new("[a-z0-9_]+|[\\u{3400}-\\u{9fff}]+").unwrap());
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    for m in WORD.find_iter(&s.to_lowercase()) {
        let t = m.as_str();
        let chars = t.chars().collect::<Vec<_>>();
        let tokens = if chars[0] >= '\u{3400}' && chars.len() > 1 {
            chars.windows(2).map(|w| w.iter().collect()).collect()
        } else {
            vec![t.to_owned()]
        };
        for t in tokens {
            if seen.insert(t.clone()) {
                out.push(t);
                if out.len() == 256 {
                    return out;
                }
            }
        }
    }
    out
}
impl Store {
    pub fn open(path: &Path, episode_days: i64) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(2))?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;")?;
        let v: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if !matches!(v, 0 | 1) {
            return Err(err("unsupported_database_version"));
        }
        db.execute_batch(include_str!("schema.sql"))?;
        db.execute(
            "INSERT INTO search_index(search_index,rank) VALUES('secure-delete',1)",
            [],
        )?;
        db.execute(
            "INSERT OR IGNORE INTO meta VALUES('cursor_secret',?)",
            [format!("{}{}", uid(), uid())],
        )?;
        db.execute("INSERT OR IGNORE INTO meta VALUES('generation','0')", [])?;
        Ok(Self {
            db,
            episode_days,
            clock: now,
        })
    }
    // A failed COMMIT must not leave the connection inside a partial transaction.
    fn commit(&self) -> Result<()> {
        if let Err(error) = self.db.execute_batch("COMMIT") {
            let _ = self.db.execute_batch("ROLLBACK");
            return Err(error.into());
        }
        Ok(())
    }
    fn bump(&self) -> Result<()> {
        self.db.execute(
            "UPDATE meta SET value=CAST(value AS INTEGER)+1 WHERE key='generation'",
            [],
        )?;
        Ok(())
    }
    fn docs(&self, sql: &str, args: impl rusqlite::Params) -> Result<Vec<Value>> {
        let mut stmt = self.db.prepare(sql)?;
        let strings = stmt
            .query_map(args, |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        strings
            .iter()
            .map(|s| Ok(serde_json::from_str(s)?))
            .collect()
    }
    fn strings(&self, sql: &str, args: impl rusqlite::Params) -> Result<Vec<String>> {
        Ok(self
            .db
            .prepare(sql)?
            .query_map(args, |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn call(&self, p: &Value, name: &str, a: &Value) -> Result<Value> {
        if !array(p, "tools")?.contains(&json!(name))
            || (name == "memory.observe" && p["role"] != "adapter")
        {
            return Err(err("scope_denied"));
        }
        validate(a, SCHEMAS.get(name).ok_or_else(|| err("invalid_argument"))?)?;
        if let Some(scope) = a.get("scope") {
            authorize(p, scope)?;
        }
        if name == "memory.status" {
            let row: Option<(String, String)> = self
                .db
                .query_row(
                    "SELECT scope,result FROM operations WHERE id=? AND principal=?",
                    params![text(a, "operation_id")?, text(p, "id")?],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (s, r) = row.ok_or_else(|| err("not_found"))?;
            authorize(p, &serde_json::from_str(&s)?)?;
            let r: Value = serde_json::from_str(&r)?;
            let job: Option<(String, Option<String>)> = self
                .db
                .query_row(
                    "SELECT state,error FROM jobs WHERE id=?",
                    [r["observation_id"].as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (state, error) = job.unwrap_or(("completed".into(), None));
            return Ok(
                json!({"schema_version":1,"request_id":uid(),"operation_id":a["operation_id"],"processing_state":state,"error":error}),
            );
        }
        if matches!(name, "memory.search" | "memory.get") {
            let mut r = if name == "memory.search" {
                self.search(p, a)?
            } else {
                self.get(a)?
            };
            r["schema_version"] = json!(1);
            r["request_id"] = json!(uid());
            return Ok(r);
        }
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let r = (|| {
            let scope = canonical(&a["scope"]);
            let dg = digest(&json!({"name":name,"args":a}));
            let old: Option<(String, String)> = self
                .db
                .query_row(
                    "SELECT digest,result FROM operations WHERE principal=? AND scope=? AND key=?",
                    params![text(p, "id")?, scope, text(a, "idempotency_key")?],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((d, r)) = old {
                if d != dg {
                    return Err(err("idempotency_conflict"));
                }
                return Ok(serde_json::from_str(&r)?);
            }
            let safe = scrub(a);
            let mut r = match name {
                "memory.save" => self.save(&safe)?,
                "memory.correct" => self.correct(&safe)?,
                "memory.forget" => self.forget(&safe)?,
                "memory.observe" => self.observe(&safe)?,
                _ => return Err(err("invalid_argument")),
            };
            r["schema_version"] = json!(1);
            r["request_id"] = json!(uid());
            r["operation_id"] = json!(uid());
            self.db.execute(
                "INSERT INTO operations VALUES(?,?,?,?,?,?)",
                params![
                    text(&r, "operation_id")?,
                    text(p, "id")?,
                    scope,
                    text(a, "idempotency_key")?,
                    dg,
                    canonical(&r)
                ],
            )?;
            self.bump()?;
            Ok(r)
        })();
        match r {
            Ok(r) => {
                self.commit()?;
                Ok(r)
            }
            Err(e) => {
                self.db.execute_batch("ROLLBACK")?;
                Err(e)
            }
        }
    }
    fn row(&self, scope: &Value, id: &str) -> Result<Value> {
        let r: Option<(String, String, Option<i64>)> = self
            .db
            .query_row(
                "SELECT doc,status,expires FROM records WHERE id=? AND scope=?",
                params![id, canonical(scope)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (doc, status, expiry) = r.ok_or_else(|| err("not_found"))?;
        if matches!(status.as_str(), "deleted" | "expired")
            || expiry.is_some_and(|e| e <= (self.clock)())
        {
            return Err(err("not_found"));
        }
        Ok(serde_json::from_str(&doc)?)
    }
    fn fact_key(r: &Value) -> Option<String> {
        r.get("fact").map(|f|digest(&json!({"subject":f["subject"],"predicate":f["predicate"],"environment":f["environment"]})))
    }
    fn valid(&self, f: &Value) -> bool {
        f["valid_from"].as_i64().unwrap_or(0) <= (self.clock)()
            && (self.clock)() < f["valid_to"].as_i64().unwrap_or(9007199254740991)
    }
    fn current_status(&self, r: &Value) -> Result<String> {
        if r["kind"] != "fact" {
            return Ok(text(r, "status")?.into());
        }
        if !self.valid(&r["fact"]) {
            return Ok("expired".into());
        }
        let others=self.docs("SELECT doc FROM records WHERE scope=? AND fact_key=? AND status IN ('active','contested') AND (expires IS NULL OR expires>?)",params![canonical(&r["scope"]),Self::fact_key(r),(self.clock)()])?;
        Ok(if others
            .iter()
            .any(|o| o["fact"]["value"] != r["fact"]["value"] && self.valid(&o["fact"]))
        {
            "contested"
        } else {
            "active"
        }
        .into())
    }
    fn get(&self, a: &Value) -> Result<Value> {
        let mut r = self.row(&a["scope"], text(a, "id")?)?;
        if let Some(rev) = a.get("revision") {
            let old = self
                .docs(
                    "SELECT doc FROM versions WHERE id=? AND revision=?",
                    params![text(a, "id")?, rev.as_i64()],
                )?
                .into_iter()
                .next()
                .ok_or_else(|| err("not_found"))?;
            let historical = *rev != r["revision"];
            r = old;
            r["historical"] = json!(historical)
        }
        if r["historical"] != true {
            r["status"] = json!(self.current_status(&r)?);
            if r["status"] == "expired" {
                return Err(err("not_found"));
            }
        }
        Ok(json!({"item":r}))
    }
    fn put(&self, r: &Value) -> Result<()> {
        let id = text(r, "id")?;
        let doc = canonical(r);
        self.db.execute(
            "INSERT OR REPLACE INTO records VALUES(?,?,?,?,?,?,?,?)",
            params![
                id,
                canonical(&r["scope"]),
                text(r, "kind")?,
                r["revision"].as_i64(),
                text(r, "status")?,
                r["expires_at"].as_i64(),
                doc,
                Self::fact_key(r)
            ],
        )?;
        self.db.execute(
            "INSERT OR REPLACE INTO versions VALUES(?,?,?)",
            params![id, r["revision"].as_i64(), doc],
        )?;
        self.db
            .execute("DELETE FROM search_index WHERE id=?", [id])?;
        if matches!(text(r, "status")?, "active" | "contested") {
            self.db.execute(
                "INSERT INTO search_index VALUES(?,?)",
                params![
                    id,
                    words(&format!(
                        "{} {}",
                        text(r, "content")?,
                        canonical(&r["fact"])
                    ))
                    .join(" ")
                ],
            )?;
        }
        for s in array(r, "sources")? {
            if let Some(key) = s["source_key"].as_str() {
                self.db.execute(
                    "INSERT OR IGNORE INTO dependencies VALUES(?,?)",
                    params![id, key],
                )?;
            }
        }
        Ok(())
    }
    fn blocked(&self, a: &Value) -> Result<()> {
        for s in array(a, "sources")? {
            if let Some(k) = s["source_key"].as_str()
                && self
                    .db
                    .query_row(
                        "SELECT 1 FROM tombstones WHERE scope=? AND source_key=?",
                        params![canonical(&a["scope"]), k],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?
                    .is_some()
            {
                return Err(err("source_unavailable"));
            }
        }
        Ok(())
    }
    fn check_kind(a: &Value, kind: &str) -> Result<()> {
        if (kind == "fact") != a.get("fact").is_some()
            || (kind == "procedure") != a.get("procedure").is_some()
        {
            return Err(err("invalid_argument"));
        }
        if let Some(f) = a.get("fact")
            && f["valid_to"].as_i64().unwrap_or(9007199254740991)
                <= f["valid_from"].as_i64().unwrap_or(0)
        {
            return Err(err("invalid_argument"));
        }
        Ok(())
    }
    fn save(&self, a: &Value) -> Result<Value> {
        self.blocked(a)?;
        let kind = text(a, "kind")?;
        Self::check_kind(a, kind)?;
        let now = (self.clock)();
        if a["verified_at"].as_i64().is_some_and(|n| n > now) {
            return Err(err("invalid_argument"));
        }
        let expires = if a["pinned"] == true {
            Value::Null
        } else {
            a.get("expires_at").cloned().unwrap_or_else(|| {
                if kind == "episode" {
                    json!(now + self.episode_days * 86400)
                } else {
                    Value::Null
                }
            })
        };
        let mut r = json!({"id":uid(),"scope":a["scope"],"kind":kind,"revision":1,"status":"active","content":a["content"],"sources":a["sources"],"observed_at":now,"verified_at":a["verified_at"],"pinned":a["pinned"].as_bool().unwrap_or(false),"expires_at":expires});
        for f in ["fact", "procedure"] {
            if let Some(v) = a.get(f) {
                r[f] = v.clone()
            }
        }
        self.put(&r)?;
        if kind == "fact" {
            self.reconcile_keys(&a["scope"], &[Self::fact_key(&r).unwrap()])?
        }
        Ok(json!({"id":r["id"],"revision":1,"status":self.status(text(&r,"id")?)?}))
    }
    fn status(&self, id: &str) -> Result<String> {
        Ok(self
            .db
            .query_row("SELECT status FROM records WHERE id=?", [id], |r| r.get(0))?)
    }
    fn correct(&self, a: &Value) -> Result<Value> {
        let mut r = self.row(&a["scope"], text(a, "id")?)?;
        let old_key = Self::fact_key(&r);
        if r["revision"] != a["expected_revision"] {
            return Err(err("revision_conflict"));
        }
        Self::check_kind(a, text(&r, "kind")?)?;
        self.blocked(a)?;
        r["revision"] = json!(r["revision"].as_i64().unwrap() + 1);
        r["content"] = a["replacement"].clone();
        r["sources"] = a["sources"].clone();
        r["observed_at"] = json!((self.clock)());
        r["verified_at"] = Value::Null;
        r["status"] = json!("active");
        for f in ["fact", "procedure"] {
            if let Some(v) = a.get(f) {
                r[f] = v.clone()
            }
        }
        self.put(&r)?;
        if r["kind"] == "fact" {
            self.reconcile_keys(
                &a["scope"],
                &[old_key.unwrap(), Self::fact_key(&r).unwrap()],
            )?
        }
        Ok(json!({"id":r["id"],"revision":r["revision"],"status":self.status(text(&r,"id")?)?}))
    }
    fn reconcile(&self, scope: &Value) -> Result<()> {
        self.reconcile_keys(scope, &[])
    }
    fn reconcile_keys(&self, scope: &Value, keys: &[String]) -> Result<()> {
        let mut sql="SELECT doc FROM records WHERE scope=? AND kind='fact' AND status IN ('active','contested') AND (expires IS NULL OR expires>?)".to_owned();
        let mut values = vec![
            SqlValue::Text(canonical(scope)),
            SqlValue::Integer((self.clock)()),
        ];
        if !keys.is_empty() {
            sql.push_str(&format!(
                " AND fact_key IN ({})",
                vec!["?"; keys.len()].join(",")
            ));
            values.extend(keys.iter().cloned().map(SqlValue::Text));
        }
        let docs = self.docs(&sql, rusqlite::params_from_iter(values))?;
        let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for r in docs {
            groups
                .entry(Self::fact_key(&r).unwrap())
                .or_default()
                .push(r)
        }
        for group in groups.values() {
            for a in group {
                let fa = &a["fact"];
                let conflict = group.iter().any(|b| {
                    let fb = &b["fact"];
                    a["id"] != b["id"]
                        && fa["value"] != fb["value"]
                        && fa["valid_from"]
                            .as_i64()
                            .unwrap_or(0)
                            .max(fb["valid_from"].as_i64().unwrap_or(0))
                            < fa["valid_to"]
                                .as_i64()
                                .unwrap_or(9007199254740991)
                                .min(fb["valid_to"].as_i64().unwrap_or(9007199254740991))
                });
                let status = if conflict { "contested" } else { "active" };
                if a["status"] != status {
                    let mut a = a.clone();
                    a["status"] = json!(status);
                    self.put(&a)?
                }
            }
        }
        Ok(())
    }
    fn clear_source(&self, scope: &str, key: &str, state: &str) -> Result<()> {
        self.db.execute(
            "UPDATE observations SET payload='{}',state=? WHERE scope=? AND source_key=?",
            params![state, scope, key],
        )?;
        self.db.execute("UPDATE jobs SET state='completed' WHERE id IN (SELECT id FROM observations WHERE scope=? AND source_key=?)",params![scope,key])?;
        Ok(())
    }
    fn erase(&self, scope: &str, ids: Vec<String>, block: bool) -> Result<usize> {
        let mut queue = ids;
        let mut seen = BTreeSet::new();
        while let Some(id) = queue.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if self
                .db
                .query_row(
                    "SELECT 1 FROM records WHERE id=? AND scope=?",
                    params![id, scope],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .is_none()
            {
                continue;
            }
            for key in self.strings(
                "SELECT source_key FROM dependencies WHERE record_id=?",
                [&id],
            )? {
                if block {
                    self.db.execute(
                        "INSERT OR IGNORE INTO tombstones VALUES(?,?)",
                        params![scope, key],
                    )?;
                }
                queue.extend(self.strings("SELECT d.record_id FROM dependencies d JOIN records r ON r.id=d.record_id WHERE d.source_key=? AND r.scope=?",params![key,scope])?);
                self.clear_source(scope, &key, "deleted")?;
            }
            self.db.execute("DELETE FROM versions WHERE id=?", [&id])?;
            self.db
                .execute("DELETE FROM search_index WHERE id=?", [&id])?;
            self.db
                .execute("DELETE FROM dependencies WHERE record_id=?", [&id])?;
            self.db.execute(
                "UPDATE records SET status='deleted',doc='{}' WHERE id=?",
                [&id],
            )?;
        }
        Ok(seen.len())
    }
    fn forget(&self, a: &Value) -> Result<Value> {
        let ids = array(a, "ids")?;
        let revs = array(a, "expected_revisions")?;
        if ids.len() != revs.len()
            || ids
                .iter()
                .map(|v| v.as_str())
                .collect::<BTreeSet<_>>()
                .len()
                != ids.len()
        {
            return Err(err("invalid_argument"));
        }
        let scope = canonical(&a["scope"]);
        for (id, rev) in ids.iter().zip(revs) {
            let r: Option<i64> = self
                .db
                .query_row(
                    "SELECT revision FROM records WHERE id=? AND scope=? AND status!='deleted'",
                    params![id.as_str(), scope],
                    |r| r.get(0),
                )
                .optional()?;
            if r.is_none() {
                return Err(err("not_found"));
            }
            if r != rev.as_i64() {
                return Err(err("revision_conflict"));
            }
        }
        let count = self.erase(
            &scope,
            ids.iter().map(|v| v.as_str().unwrap().into()).collect(),
            true,
        )?;
        self.reconcile(&a["scope"])?;
        Ok(
            json!({"hidden":true,"records_deleted":count,"physical_cleanup":"checkpoint_pending","external_copies":"Host logs, hook receipts, provider request assets and backups require separate cleanup."}),
        )
    }
    fn observe(&self, a: &Value) -> Result<Value> {
        let event = &a["source_event"];
        let obs = &a["observation"];
        let scope = canonical(&a["scope"]);
        if a["scope"]["kind"] == "user"
            || event["workspace_id"] != a["scope"]["workspace_id"]
            || (a["scope"]["kind"] == "thread" && event["thread_id"] != a["scope"]["thread_id"])
        {
            return Err(err("scope_denied"));
        }
        let typ = text(obs, "type")?;
        let required: &[&str] = match typ {
            "turn_opened" => &[],
            "tool_result" => &["call_id", "tool_name", "outcome"],
            "context_checkpoint" => &["through_seq", "covers", "manual"],
            "turn_outcome" => &["outcome"],
            "source_invalidated" => &["source_keys"],
            _ => return Err(err("invalid_argument")),
        };
        if required.iter().any(|k| obs["payload"].get(k).is_none()) {
            return Err(err("invalid_argument"));
        }
        let mut identity = json!({"type":typ});
        for k in [
            "host",
            "host_instance_id",
            "workspace_id",
            "thread_id",
            "line_id",
            "source_seq",
        ] {
            identity[k] = event[k].clone()
        }
        let key = digest(&identity);
        let old:Option<(String,String,String)>=self.db.query_row("SELECT id,state,payload FROM observations WHERE scope=? AND source_key=? AND digest=?",params![scope,key,text(event,"source_digest")?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((id, state, payload)) = old {
            if !matches!(state.as_str(), "deleted" | "expired") && payload != canonical(obs) {
                return Err(err("idempotency_conflict"));
            }
            return Ok(
                json!({"accepted":true,"observation_id":id,"processing_state":state,"source_key":key}),
            );
        }
        if self
            .db
            .query_row(
                "SELECT 1 FROM tombstones WHERE scope=? AND source_key=?",
                params![scope, key],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some()
        {
            return Err(err("source_unavailable"));
        }
        let previous=self.strings("SELECT record_id FROM observations WHERE scope=? AND source_key=? AND record_id IS NOT NULL",params![scope,key])?;
        self.erase(&scope, previous, false)?;
        self.clear_source(&scope, &key, "deleted")?;
        if typ == "source_invalidated" {
            for target in array(&obs["payload"], "source_keys")? {
                let target = target.as_str().ok_or_else(|| err("invalid_argument"))?;
                self.db.execute(
                    "INSERT OR IGNORE INTO tombstones VALUES(?,?)",
                    params![scope, target],
                )?;
                let ids=self.strings("SELECT d.record_id FROM dependencies d JOIN records r ON r.id=d.record_id WHERE r.scope=? AND d.source_key=?",params![scope,target])?;
                self.erase(&scope, ids, true)?;
                self.clear_source(&scope, target, "deleted")?;
            }
        }
        let id = uid();
        let state = if matches!(typ, "turn_opened" | "source_invalidated") {
            "completed"
        } else {
            "accepted"
        };
        self.db.execute(
            "INSERT INTO observations VALUES(?,?,?,?,?,?,NULL)",
            params![
                id,
                scope,
                key,
                text(event, "source_digest")?,
                canonical(obs),
                state
            ],
        )?;
        self.db.execute(
            "INSERT INTO jobs VALUES(?,?,0,?,NULL)",
            params![id, state, (self.clock)()],
        )?;
        Ok(json!({"accepted":true,"observation_id":id,"processing_state":state,"source_key":key}))
    }
    pub fn maintain(&self) -> Result<bool> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let mut job_id = None;
        let mut attempts = 0;
        let result = (|| {
            let now = (self.clock)();
            let expired = self.docs(
                "SELECT doc FROM records WHERE status IN ('active','contested') AND expires<=?",
                [now],
            )?;
            let mut scopes = BTreeSet::new();
            for r in &expired {
                let id = text(r, "id")?;
                scopes.insert(canonical(&r["scope"]));
                self.db.execute("DELETE FROM versions WHERE id=?", [id])?;
                self.db
                    .execute("DELETE FROM search_index WHERE id=?", [id])?;
                self.db.execute(
                    "UPDATE records SET status='expired',doc='{}' WHERE id=?",
                    [id],
                )?;
                self.db.execute(
                    "UPDATE observations SET payload='{}',state='expired' WHERE record_id=?",
                    [id],
                )?;
            }
            for s in scopes {
                self.reconcile(&serde_json::from_str(&s)?)?
            }
            let job:Option<(String,i64,String,String,String)>=self.db.query_row("SELECT j.id,j.attempts,o.scope,o.payload,o.source_key FROM jobs j JOIN observations o ON o.id=j.id WHERE j.state='accepted' AND j.next_at<=? ORDER BY j.next_at,j.id LIMIT 1",[now],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
            if let Some((id, count, scope, payload, key)) = job {
                job_id = Some(id.clone());
                attempts = count;
                let obs: Value = serde_json::from_str(&payload)?;
                let mut sources = array(&obs["payload"], "sources")?.clone();
                for s in &mut sources {
                    s["source_key"] = json!(key)
                }
                let mut summary = obs["payload"].clone();
                summary
                    .as_object_mut()
                    .ok_or_else(|| err("invalid_argument"))?
                    .remove("sources");
                summary["observation"] = obs["type"].clone();
                let content = canonical(&summary).chars().take(32768).collect::<String>();
                let saved=self.save(&json!({"scope":serde_json::from_str::<Value>(&scope)?,"kind":"episode","content":content,"sources":sources}))?;
                self.db.execute(
                    "UPDATE observations SET state='completed',record_id=? WHERE id=?",
                    params![text(&saved, "id")?, id],
                )?;
                self.db.execute(
                    "UPDATE jobs SET state='completed',attempts=attempts+1,error=NULL WHERE id=?",
                    [id],
                )?;
            }
            if job_id.is_some() || !expired.is_empty() {
                self.bump()?
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.commit()?;
            }
            Err(e) => {
                self.db.execute_batch("ROLLBACK")?;
                if let Some(id) = &job_id {
                    let n = attempts + 1;
                    self.db.execute("UPDATE jobs SET state=?,attempts=?,next_at=?,error='processing_failed' WHERE id=?",params![if n>=5{"failed"}else{"accepted"},n,(self.clock)()+(1i64<<n),id])?;
                } else {
                    return Err(e);
                }
            }
        }
        self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(job_id.is_some())
    }
    fn search(&self, p: &Value, a: &Value) -> Result<Value> {
        let mut scopes = vec![a["scope"].clone()];
        if a["scope"]["kind"] == "thread" {
            let mut s = a["scope"].clone();
            s.as_object_mut().unwrap().remove("thread_id");
            s["kind"] = json!("workspace");
            scopes.push(s)
        }
        if a["scope"]["kind"] != "user" {
            scopes.push(json!({"kind":"user","owner_id":a["scope"]["owner_id"]}))
        }
        let scopes = scopes
            .into_iter()
            .filter(|s| authorize(p, s).is_ok())
            .map(|s| canonical(&s))
            .collect::<Vec<_>>();
        let mut query = a.clone();
        query.as_object_mut().unwrap().remove("cursor");
        query["principal"] = p["id"].clone();
        let query_id = digest(&query);
        let generation: String =
            self.db
                .query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
                    r.get(0)
                })?;
        let secret: String = self.db.query_row(
            "SELECT value FROM meta WHERE key='cursor_secret'",
            [],
            |r| r.get(0),
        )?;
        let key = (0..secret.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&secret[i..i + 2], 16).map_err(|_| err("storage_error")))
            .collect::<Result<Vec<_>>>()?;
        let mut offset = 0;
        if let Some(c) = a["cursor"].as_str() {
            let (encoded, signature) = c.split_once('.').ok_or_else(|| err("invalid_argument"))?;
            let raw = URL_SAFE
                .decode(encoded)
                .map_err(|_| err("invalid_argument"))?;
            let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|_| err("storage_error"))?;
            mac.update(&raw);
            let expected = format!("{:x}", mac.finalize().into_bytes());
            use subtle::ConstantTimeEq;
            if !bool::from(expected.as_bytes().ct_eq(signature.as_bytes())) {
                return Err(err("invalid_argument"));
            }
            let c = decode(&raw)?;
            if c["query"] != query_id || c["generation"] != generation {
                return Err(err("invalid_argument"));
            }
            offset = c["offset"]
                .as_i64()
                .filter(|n| *n >= 0)
                .ok_or_else(|| err("invalid_argument"))?;
        }
        let kinds = array(a, "kinds")?;
        let mut clauses = vec![
            format!("scope IN ({})", vec!["?"; scopes.len()].join(",")),
            format!("kind IN ({})", vec!["?"; kinds.len()].join(",")),
            "status IN ('active','contested')".into(),
            "(expires IS NULL OR expires>?)".into(),
        ];
        let mut params: Vec<SqlValue> = scopes.into_iter().map(SqlValue::Text).collect();
        params.extend(
            kinds
                .iter()
                .map(|v| SqlValue::Text(v.as_str().unwrap().into())),
        );
        params.push(SqlValue::Integer((self.clock)()));
        let tokens = words(text(a, "query")?);
        let (base, order) = if tokens.is_empty() {
            ("SELECT doc FROM records WHERE ", "id")
        } else {
            clauses.push("search_index MATCH ?".into());
            params.push(SqlValue::Text(
                tokens
                    .iter()
                    .map(|t| format!("\"{t}\""))
                    .collect::<Vec<_>>()
                    .join(" OR "),
            ));
            (
                "SELECT records.doc FROM search_index JOIN records ON records.id=search_index.id WHERE ",
                "search_index.rank,records.id",
            )
        };
        params.push(SqlValue::Integer(offset));
        let rows = self.docs(
            &format!(
                "{base}{} ORDER BY {order} LIMIT 1001 OFFSET ?",
                clauses.join(" AND ")
            ),
            rusqlite::params_from_iter(params),
        )?;
        let mut items = vec![];
        let mut conflicts = vec![];
        let mut used = 0;
        let mut consumed = 0;
        let cap = a["budget"]["max_utf8_bytes"]
            .as_u64()
            .unwrap()
            .min(a["budget"]["estimated_tokens"].as_u64().unwrap()) as usize;
        for mut r in rows.clone() {
            if !self.valid(&r["fact"]) {
                consumed += 1;
                continue;
            }
            r["status"] = json!(self.current_status(&r)?);
            let size = canonical(&r).len();
            if items.len() + conflicts.len() >= a["budget"]["max_items"].as_u64().unwrap() as usize
            {
                break;
            }
            if size > cap {
                consumed += 1;
                continue;
            }
            if used + size > cap {
                break;
            }
            if r["status"] == "contested" {
                conflicts.push(r)
            } else {
                items.push(r)
            }
            used += size;
            consumed += 1;
        }
        let more = consumed < rows.len() || rows.len() == 1001;
        let cursor = if more && consumed > 0 {
            let raw = canonical(
                &json!({"query":query_id,"generation":generation,"offset":offset+consumed as i64}),
            );
            let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|_| err("storage_error"))?;
            mac.update(raw.as_bytes());
            Some(format!(
                "{}.{:x}",
                URL_SAFE.encode(raw.as_bytes()),
                mac.finalize().into_bytes()
            ))
        } else {
            None
        };
        Ok(
            json!({"items":items,"conflicts":conflicts,"next_cursor":cursor,"truncated":more,"budget":{"utf8_bytes":used,"estimator":"utf8-byte-upper-bound","max_bytes":cap}}),
        )
    }
}
