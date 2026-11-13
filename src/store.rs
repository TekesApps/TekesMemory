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
    security: Option<crate::security::SecurityProvider>,
    transaction_generation: std::cell::Cell<i64>,
    pub db: Connection,
    pub episode_days: i64,
    pub clock: fn() -> i64,
}
fn valid_effect_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
fn scrub_write_args(name: &str, args: &Value) -> Value {
    let mut safe = args.clone();
    // Scope, source references, event IDs, and idempotency inputs are identities; redact only
    // fields that carry user-authored memory text so those identities remain exact.
    let fields: &[&str] = match name {
        "memory.save" => &["content", "fact", "procedure"],
        "memory.correct" => &["replacement", "fact", "procedure"],
        "memory.observe" => &[],
        _ => &[],
    };
    for field in fields {
        if let Some(value) = args.get(*field) {
            safe[*field] = scrub(value);
        }
    }
    if name == "memory.observe" {
        for field in ["summary", "reason"] {
            if let Some(value) = args.pointer(&format!("/observation/payload/{field}")) {
                safe["observation"]["payload"][field] = scrub(value);
            }
        }
    }
    safe
}
fn derived_record_args(scope: Value, obs: &Value, key: &str) -> Result<Value> {
    let mut sources = array(&obs["payload"], "sources")?.clone();
    if let Some(source_key) = obs["payload"]["own_source_key"].as_str()
        && !sources
            .iter()
            .any(|source| source["source_key"] == source_key)
    {
        sources.push(json!({
            "host":"tekes-memory-lineage",
            "ref":source_key,
            "digest":source_key,
            "source_key":source_key
        }));
    }
    if let Some(lineage) = obs["payload"]["lineage_keys"].as_array() {
        for source_key in lineage.iter().filter_map(Value::as_str) {
            if !sources
                .iter()
                .any(|source| source["source_key"] == source_key)
            {
                sources.push(json!({
                    "host":"tekes-memory-lineage",
                    "ref":source_key,
                    "digest":source_key,
                    "source_key":source_key
                }));
            }
        }
    }
    if !sources.iter().any(|source| source["source_key"] == key) {
        sources.push(json!({
            "host":"tekes-memory",
            "ref":key,
            "digest":digest(obs),
            "source_key":key
        }));
    }
    let mut summary = obs["payload"].clone();
    let payload = summary
        .as_object_mut()
        .ok_or_else(|| err("invalid_argument"))?;
    payload.remove("sources");
    payload.remove("own_source_key");
    payload.remove("lineage_keys");
    summary["observation"] = obs["type"].clone();
    let content = canonical(&summary).chars().take(32768).collect::<String>();
    Ok(json!({"scope":scope,"kind":"episode","content":content,"sources":sources}))
}
pub fn words(s: &str) -> Vec<String> {
    words_limited(s, None)
}
fn words_limited(s: &str, limit: Option<usize>) -> Vec<String> {
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
                if limit.is_some_and(|limit| out.len() == limit) {
                    return out;
                }
            }
        }
    }
    out
}
fn query_words(s: &str) -> Vec<String> {
    words_limited(s, Some(256))
}
impl Store {
    pub fn open(path: &Path, episode_days: i64) -> Result<Self> {
        Self::open_secure(path, episode_days, None)
    }
    pub fn open_secure(path: &Path, episode_days: i64, config: Option<&Value>) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(2))?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;")?;
        let v: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if !matches!(v, 0 | 1 | 2) {
            return Err(err("unsupported_database_version"));
        }
        db.execute_batch(include_str!("schema.sql"))?;
        if v == 0 {
            db.execute_batch("PRAGMA user_version=1")?;
        }
        db.execute_batch("CREATE TABLE IF NOT EXISTS security_audit(id TEXT PRIMARY KEY,event TEXT NOT NULL,state TEXT NOT NULL,error TEXT)")?;
        let binding: Option<String> = db
            .query_row(
                "SELECT value FROM meta WHERE key='security_binding'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if v == 2 && binding.is_none() {
            return Err(err("unsupported_database_version"));
        }
        if binding.is_some() && config.is_none() {
            return Err(err("security_provider_required"));
        }
        let security = if let Some(config) = config {
            let wanted = canonical(&crate::security::SecurityProvider::binding(config));
            if binding.as_ref().is_some_and(|v| v != &wanted) {
                return Err(err("security_profile_mismatch"));
            }
            if binding.is_none() {
                let count: i64 = db.query_row("SELECT (SELECT count(*) FROM records)+(SELECT count(*) FROM versions)+(SELECT count(*) FROM observations)+(SELECT count(*) FROM operations)+(SELECT count(*) FROM effect_receipts)+(SELECT count(*) FROM tombstones)", [], |r| r.get(0))?;
                if count != 0 {
                    return Err(err("security_migration_required"));
                }
                db.execute_batch("BEGIN IMMEDIATE")?;
                db.execute("INSERT INTO meta VALUES('security_binding',?)", [&wanted])?;
                db.execute("INSERT INTO meta VALUES('security_store_id',?)", [uid()])?;
                db.execute_batch("PRAGMA user_version=2; COMMIT")?;
            }
            let id = db.query_row(
                "SELECT value FROM meta WHERE key='security_store_id'",
                [],
                |r| r.get(0),
            )?;
            Some(crate::security::SecurityProvider::new(config, id)?)
        } else {
            None
        };
        db.execute(
            "INSERT INTO search_index(search_index,rank) VALUES('secure-delete',1)",
            [],
        )?;
        db.execute(
            "INSERT OR IGNORE INTO meta VALUES('cursor_secret',?)",
            [format!("{}{}", uid(), uid())],
        )?;
        db.execute("INSERT OR IGNORE INTO meta VALUES('generation','0')", [])?;
        // The original indexer shared the 256-token query cap. Rebuild once on
        // upgrade so existing records receive the complete document index.
        let index_version: Option<String> = db
            .query_row(
                "SELECT value FROM meta WHERE key='search_index_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if index_version.as_deref() != Some("2") {
            if security.is_some() {
                let count: i64 = db.query_row("SELECT count(*) FROM records", [], |r| r.get(0))?;
                if count != 0 {
                    return Err(err("security_migration_required"));
                }
            }
            db.execute_batch("BEGIN IMMEDIATE; DELETE FROM search_index;")?;
            let docs = {
                let mut stmt = db
                    .prepare("SELECT id,doc FROM records WHERE status IN ('active','contested')")?;
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };
            for (id, doc) in docs {
                let record: Value = serde_json::from_str(&doc)?;
                db.execute(
                    "INSERT INTO search_index VALUES(?,?)",
                    params![
                        id,
                        words(&format!(
                            "{} {}",
                            record["content"].as_str().unwrap_or_default(),
                            canonical(&record["fact"])
                        ))
                        .join(" ")
                    ],
                )?;
            }
            db.execute("INSERT INTO meta(key,value) VALUES('search_index_version','2') ON CONFLICT(key) DO UPDATE SET value='2'", [])?;
            db.execute_batch("COMMIT")?;
        }
        let store = Self {
            db,
            episode_days,
            clock: now,
            security,
            transaction_generation: std::cell::Cell::new(0),
        };
        store.checkpoint("check")?;
        Ok(store)
    }
    /// Explicit offline migration of an existing client-encrypted Store, never an automatic startup fallback.
    pub fn migrate_opaque(path: &Path, episode_days: i64, config: &Value) -> Result<Self> {
        let mut store = Self::open(path, episode_days)?;
        let id = uid();
        store.security = Some(crate::security::SecurityProvider::new(config, id.clone())?);
        let operation: Result<()> = (|| {
            store
                .security
                .as_ref()
                .unwrap()
                .allow("checkpoint", json!({"mode":"check","generation":0}))?;
            store.access(
                &json!({"id":"maintenance","role":"system"}),
                "migrate",
                &Value::Null,
                &Value::Null,
            )?;
            store.begin()?;
            let result = (|| {
                // Read the legacy values before replacing any current/history pair.
                let mut rows = Vec::new();
                for (table, column) in [("records", "doc"), ("versions", "doc")] {
                    let values = store
                        .db
                        .prepare(&format!(
                            "SELECT rowid,{column} FROM {table} WHERE {column}!='{{}}'"
                        ))?
                        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    rows.push((table, values));
                }
                store.db.execute("DELETE FROM search_index", [])?;
                for (table, values) in rows {
                    for (row, raw) in values {
                        let record: Value = serde_json::from_str(&raw)?;
                        store.access(
                            &json!({"id":"maintenance","role":"system"}),
                            "migrate",
                            &record["scope"],
                            &record,
                        )?;
                        let (record, tokens) = store.validate_record(&record)?;
                        let wrapped = store.encode_stored(&record,json!({"table":"record","id":record["id"],"revision":record["revision"],"scope":record["scope"]}))?;
                        if table == "records" {
                            store.db.execute(
                                "UPDATE records SET doc=?,expires=? WHERE rowid=?",
                                params![wrapped, record["expires_at"].as_i64(), row],
                            )?;
                            if matches!(record["status"].as_str(), Some("active" | "contested")) {
                                store.db.execute(
                                    "INSERT INTO search_index VALUES(?,?)",
                                    params![text(&record, "id")?, tokens],
                                )?;
                            }
                        } else {
                            store.db.execute(
                                "UPDATE versions SET doc=? WHERE rowid=?",
                                params![wrapped, row],
                            )?;
                        }
                    }
                }
                let observations = store
                    .db
                    .prepare("SELECT id,scope,payload FROM observations WHERE payload!='{}'")?
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                for (id, scope, raw) in observations {
                    let value: Value = serde_json::from_str(&raw)?;
                    store.security.as_ref().unwrap().allow(
                        "validate",
                        json!({"kind":"observation","value":value,"now":(store.clock)()}),
                    )?;
                    let wrapped = store.encode_stored(&value,json!({"table":"observation","id":id,"scope":serde_json::from_str::<Value>(&scope)?}))?;
                    store.db.execute(
                        "UPDATE observations SET payload=? WHERE id=?",
                        params![wrapped, id],
                    )?;
                }
                for table in ["operations", "effect_receipts"] {
                    let rows = store
                        .db
                        .prepare(&format!(
                            "SELECT rowid,principal,scope,key,result FROM {table}"
                        ))?
                        .query_map([], |r| {
                            Ok((
                                r.get::<_, i64>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, String>(4)?,
                            ))
                        })?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    for (row, principal, scope, key, raw) in rows {
                        let context = if table == "operations" {
                            json!({"table":"operation","principal":principal,"scope":serde_json::from_str::<Value>(&scope)?,"key":key})
                        } else {
                            json!({"table":"effect","principal":principal,"key":key})
                        };
                        let wrapped = store.encode_stored(&serde_json::from_str(&raw)?, context)?;
                        store.db.execute(
                            &format!("UPDATE {table} SET result=? WHERE rowid=?"),
                            params![wrapped, row],
                        )?;
                    }
                }
                store.db.execute(
                    "INSERT INTO meta VALUES('security_binding',?)",
                    [canonical(&crate::security::SecurityProvider::binding(
                        config,
                    ))],
                )?;
                store
                    .db
                    .execute("INSERT INTO meta VALUES('security_store_id',?)", [id])?;
                store
                    .db
                    .execute("UPDATE meta SET value='1' WHERE key='generation'", [])?;
                store.db.execute(
                    "UPDATE meta SET value=? WHERE key='cursor_secret'",
                    [format!("{}{}", uid(), uid())],
                )?;
                store.db.execute_batch("PRAGMA user_version=2")?;
                store.audit(
                    &json!({"id":"maintenance"}),
                    "migrate",
                    &Value::Null,
                    "success",
                    None,
                )?;
                store
                    .security
                    .as_ref()
                    .unwrap()
                    .allow("checkpoint", json!({"mode":"reserve","generation":0}))?;
                Ok(())
            })();
            if let Err(error) = result {
                store.db.execute_batch("ROLLBACK")?;
                return Err(error);
            }
            store.commit()?;
            Ok(())
        })();
        if let Err(error) = operation {
            store.audit(
                &json!({"id":"maintenance"}),
                "migrate",
                &Value::Null,
                "denied_or_failed",
                None,
            )?;
            return Err(error);
        }
        Ok(store)
    }
    fn generation(&self) -> Result<i64> {
        Ok(self.db.query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='generation'",
            [],
            |r| r.get(0),
        )?)
    }
    fn begin(&self) -> Result<()> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        self.transaction_generation.set(self.generation()?);
        Ok(())
    }
    fn checkpoint(&self, mode: &str) -> Result<()> {
        if let Some(provider) = &self.security {
            provider.allow(
                "checkpoint",
                json!({"mode":mode,"generation":self.generation()?}),
            )?;
        }
        Ok(())
    }
    fn reserve_if_changed(&self) -> Result<()> {
        if let Some(provider) = &self.security {
            let before = self.transaction_generation.get();
            if self.generation()? != before {
                provider.allow("checkpoint", json!({"mode":"reserve","generation":before}))?;
            }
        }
        Ok(())
    }
    fn encode_stored(&self, value: &Value, context: Value) -> Result<String> {
        match &self.security {
            Some(p) => p.seal(&context, value),
            None => Ok(canonical(value)),
        }
    }
    fn decode_stored(&self, raw: &str, context: Option<&Value>) -> Result<Value> {
        let value = match &self.security {
            Some(p) => p.open(raw, context)?,
            None => return Ok(serde_json::from_str(raw)?),
        };
        let stored: Value = serde_json::from_str(raw)?;
        if stored["context"]["table"] == "record"
            && ["id", "revision", "scope"]
                .iter()
                .any(|key| stored["context"][key] != value[key])
        {
            return Err(err("security_payload_invalid"));
        }
        Ok(value)
    }
    fn access(
        &self,
        principal: &Value,
        operation: &str,
        scope: &Value,
        resource: &Value,
    ) -> Result<()> {
        if let Some(provider) = &self.security {
            provider.allow("access", json!({"principal":{"id":principal["id"],"role":principal["role"],"owner_id":principal["owner_id"]},
                "operation":operation,"scope":scope,"resource":resource,"now":(self.clock)()}))?;
        }
        Ok(())
    }
    fn derived_args(&self, scope: Value, obs: &Value, key: &str) -> Result<Value> {
        let mut args = derived_record_args(scope, obs, key)?;
        if let Some(provider) = &self.security {
            provider.allow(
                "validate",
                json!({"kind":"observation","value":obs,"now":(self.clock)()}),
            )?;
            args["content"] = obs["payload"]["summary"].clone();
        }
        Ok(args)
    }
    fn audit(
        &self,
        p: &Value,
        operation: &str,
        scope: &Value,
        outcome: &str,
        operation_id: Option<&str>,
    ) -> Result<()> {
        if self.security.is_some() {
            let operation = if SCHEMAS.contains_key(operation)
                || matches!(operation, "maintenance" | "rewrap" | "migrate")
            {
                operation
            } else {
                "unknown"
            };
            let event = json!({"schema_version":1,"store_id":self.security.as_ref().unwrap().store_id,"event_id":uid(),"operation":operation,"outcome":outcome,
                "principal_hash":digest(&p["id"]),"scope_hash":digest(scope),"at":(self.clock)(),"operation_id":operation_id});
            self.db.execute(
                "INSERT INTO security_audit VALUES(?,?, 'pending',NULL)",
                params![text(&event, "event_id")?, canonical(&event)],
            )?;
            // Audit-only reads do not change the business generation. Require independent
            // durable acceptance before releasing content, so restoring an otherwise
            // identical business snapshot cannot erase a successful read's sole evidence.
            if self.db.is_autocommit()
                || (operation.starts_with("memory.")
                    && self.generation()? == self.transaction_generation.get())
            {
                let reply = self
                    .security
                    .as_ref()
                    .unwrap()
                    .request("audit", json!({"event":event}))?;
                if reply["accepted"] != true || reply["event_id"] != event["event_id"] {
                    return Err(err("security_protocol_error"));
                }
                self.db.execute(
                    "UPDATE security_audit SET state='delivered' WHERE id=?",
                    [text(&event, "event_id")?],
                )?;
            }
        }
        Ok(())
    }
    pub fn deliver_audit(&self) -> Result<usize> {
        let Some(provider) = &self.security else {
            return Ok(0);
        };
        let rows = self
            .db
            .prepare(
                "SELECT id,event FROM security_audit WHERE state='pending' ORDER BY rowid LIMIT 64",
            )?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut delivered = 0;
        for (id, event) in rows {
            let result = provider.request(
                "audit",
                json!({"event":serde_json::from_str::<Value>(&event)?}),
            );
            match result {
                Ok(reply) if reply["accepted"] == true && reply["event_id"] == id => {
                    self.db.execute(
                        "UPDATE security_audit SET state='delivered',error=NULL WHERE id=?",
                        [id],
                    )?;
                    delivered += 1;
                }
                _ => {
                    self.db.execute(
                        "UPDATE security_audit SET error='delivery_pending' WHERE id=?",
                        [id],
                    )?;
                    break;
                }
            }
        }
        Ok(delivered)
    }
    pub fn security_status(&self) -> Result<Value> {
        let pending: i64 = self.db.query_row(
            "SELECT count(*) FROM security_audit WHERE state='pending'",
            [],
            |r| r.get(0),
        )?;
        Ok(
            json!({"protected":self.security.is_some(),"audit_pending":pending,"generation":self.generation()?}),
        )
    }
    /// Rewrap at most `limit` rows, preserving client ciphertext and all row/index identities.
    pub fn rewrap_batch(&self, limit: usize) -> Result<usize> {
        let result = self.rewrap_inner(limit);
        if result.is_err() {
            self.audit(
                &json!({"id":"maintenance"}),
                "rewrap",
                &Value::Null,
                "denied_or_failed",
                None,
            )?;
        }
        result
    }
    fn rewrap_inner(&self, limit: usize) -> Result<usize> {
        let Some(provider) = &self.security else {
            return Ok(0);
        };
        if !(1..=256).contains(&limit) {
            return Err(err("invalid_argument"));
        }
        self.checkpoint("check")?;
        self.access(
            &json!({"id":"maintenance","role":"system"}),
            "rewrap",
            &Value::Null,
            &Value::Null,
        )?;
        let keys = provider.request("keys", json!({}))?;
        let current = text(&keys, "write_key_version")?;
        self.begin()?;
        let result = (|| {
            let mut changed = 0;
            for (table, column) in [
                ("records", "doc"),
                ("versions", "doc"),
                ("observations", "payload"),
                ("operations", "result"),
                ("effect_receipts", "result"),
            ] {
                let rows = self.db.prepare(&format!("SELECT rowid,{column} FROM {table} WHERE {column}!='{{}}' AND COALESCE(json_extract({column},'$.envelope.key_version'),'')!=? LIMIT ?"))?
                    .query_map(params![current,(limit-changed) as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?
                    .collect::<std::result::Result<Vec<_>,_>>()?;
                for (row, raw) in rows {
                    let stored: Value = serde_json::from_str(&raw)?;
                    let value = self.decode_stored(&raw, Some(&stored["context"]))?;
                    let wrapped = provider.seal(&stored["context"], &value)?;
                    if serde_json::from_str::<Value>(&wrapped)?["envelope"]["key_version"]
                        != current
                    {
                        return Err(err("security_protocol_error"));
                    }
                    self.db.execute(
                        &format!("UPDATE {table} SET {column}=? WHERE rowid=?"),
                        params![wrapped, row],
                    )?;
                    changed += 1;
                }
                if changed == limit {
                    break;
                }
            }
            if changed > 0 {
                self.bump()?;
                self.audit(
                    &json!({"id":"maintenance"}),
                    "rewrap",
                    &Value::Null,
                    "success",
                    None,
                )?;
                self.reserve_if_changed()?;
            }
            Ok(changed)
        })();
        match result {
            Ok(count) => {
                self.commit()?;
                Ok(count)
            }
            Err(e) => {
                self.db.execute_batch("ROLLBACK")?;
                Err(e)
            }
        }
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
            .map(|s| self.decode_stored(s, None))
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
        self.call_with_effect(p, name, a, None)
    }
    /// Record an authenticated rejection made at the transport's schema/tool boundary.
    pub fn audit_rejection(&self, principal: &Value, operation: &str) -> Result<()> {
        self.audit(principal, operation, &Value::Null, "denied_or_failed", None)
    }
    pub fn reconcile_effect(&self, p: &Value, key: &str) -> Result<Value> {
        let result = self.reconcile_effect_inner(p, key);
        if result.is_err() {
            self.audit(
                p,
                "memory.reconcile_effect",
                &Value::Null,
                "denied_or_failed",
                None,
            )?;
        }
        result
    }
    fn reconcile_effect_inner(&self, p: &Value, key: &str) -> Result<Value> {
        if ![
            "memory.save",
            "memory.correct",
            "memory.forget",
            "memory.observe",
        ]
        .iter()
        .any(|tool| {
            p["tools"]
                .as_array()
                .is_some_and(|a| a.contains(&json!(tool)))
        }) {
            return Err(err("scope_denied"));
        }
        if !valid_effect_key(key) {
            return Err(err("invalid_argument"));
        }
        self.checkpoint("check")?;
        self.access(
            p,
            "memory.reconcile_effect",
            &Value::Null,
            &json!({"effect_key":key}),
        )?;
        let row: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT scope,result FROM effect_receipts WHERE principal=? AND key=?",
                params![text(p, "id")?, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((scope, result)) = row else {
            self.audit(p, "memory.reconcile_effect", &Value::Null, "success", None)?;
            return Ok(json!({"status":"not_found"}));
        };
        authorize(p, &serde_json::from_str(&scope)?)?;
        self.access(
            p,
            "memory.reconcile_effect",
            &serde_json::from_str(&scope)?,
            &json!({"effect_key":key}),
        )?;
        let data = self.decode_stored(
            &result,
            Some(&json!({"table":"effect","principal":p["id"],"key":key})),
        )?;
        self.audit(
            p,
            "memory.reconcile_effect",
            &serde_json::from_str(&scope)?,
            "success",
            data["operation_id"].as_str(),
        )?;
        let envelope = json!({"structuredContent":data,"content":[{"type":"text","text":canonical(&data)}],"isError":false});
        Ok(json!({"status":"confirmed","value":envelope}))
    }
    pub fn call_with_effect(
        &self,
        p: &Value,
        name: &str,
        a: &Value,
        effect_key: Option<&str>,
    ) -> Result<Value> {
        let result = self.call_inner(p, name, a, effect_key);
        if self.security.is_some()
            && (result.is_err() || matches!(name, "memory.get" | "memory.search" | "memory.status"))
        {
            self.audit(
                p,
                name,
                &a["scope"],
                if result.is_ok() {
                    "success"
                } else {
                    "denied_or_failed"
                },
                None,
            )?;
        }
        result.map(|mut value| {
            if self.security.is_some() {
                value["security"] = json!({"audit_state":if matches!(name,"memory.get"|"memory.search"|"memory.status") { "delivered" } else { "pending" }});
            }
            value
        })
    }
    fn call_inner(
        &self,
        p: &Value,
        name: &str,
        a: &Value,
        effect_key: Option<&str>,
    ) -> Result<Value> {
        if effect_key.is_some_and(|key| !valid_effect_key(key)) {
            return Err(err("invalid_argument"));
        }
        if !array(p, "tools")?.contains(&json!(name))
            || (name == "memory.observe" && p["role"] != "adapter")
        {
            return Err(err("scope_denied"));
        }
        if wire_json(a).len() > MAX_BODY {
            return Err(err("budget_exceeded"));
        }
        validate(a, SCHEMAS.get(name).ok_or_else(|| err("invalid_argument"))?)?;
        if let Some(scope) = a.get("scope") {
            authorize(p, scope)?;
        }
        self.checkpoint("check")?;
        if let Some(scope) = a.get("scope") {
            self.access(p, name, scope, a)?;
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
            let scope: Value = serde_json::from_str(&s)?;
            self.access(p, name, &scope, a)?;
            let r = self.decode_stored(&r, None)?;
            if r["operation_id"] != a["operation_id"] {
                return Err(err("security_payload_invalid"));
            }
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
            if name == "memory.get" {
                self.access(p, "memory.get", &r["item"]["scope"], &r["item"])?;
            }
            r["schema_version"] = json!(1);
            r["request_id"] = json!(uid());
            return Ok(r);
        }
        self.begin()?;
        let r = (|| {
            let scope = canonical(&a["scope"]);
            let dg = digest(&json!({"name":name,"args":a}));
            if let Some(key) = effect_key {
                let receipt: Option<(String, String)> = self
                    .db
                    .query_row(
                        "SELECT digest,result FROM effect_receipts WHERE principal=? AND key=?",
                        params![text(p, "id")?, key],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((prior_digest, prior_result)) = receipt {
                    if prior_digest != dg {
                        return Err(err("idempotency_conflict"));
                    }
                    return self.decode_stored(
                        &prior_result,
                        Some(&json!({"table":"effect","principal":p["id"],"key":key})),
                    );
                }
            }
            let old: Option<(String, String)> = self
                .db
                .query_row(
                    "SELECT digest,result FROM operations WHERE principal=? AND scope=? AND key=?",
                    params![text(p, "id")?, scope, text(a, "idempotency_key")?],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let r = if let Some((d, result)) = old {
                if d != dg {
                    return Err(err("idempotency_conflict"));
                }
                self.decode_stored(&result, Some(&json!({"table":"operation","principal":p["id"],"scope":a["scope"],"key":a["idempotency_key"]})))?
            } else {
                let safe = if self.security.is_some() {
                    a.clone()
                } else {
                    scrub_write_args(name, a)
                };
                let mut r = match name {
                    "memory.save" => self.save(&safe)?,
                    "memory.correct" => {
                        let current = self.row(&safe["scope"], text(&safe, "id")?)?;
                        self.access(p, name, &safe["scope"], &current)?;
                        self.correct(&safe)?
                    }
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
                        &scope,
                        text(a, "idempotency_key")?,
                        &dg,
                        self.encode_stored(&r, json!({"table":"operation","principal":p["id"],"scope":a["scope"],"key":a["idempotency_key"]}))?
                    ],
                )?;
                self.bump()?;
                r
            };
            if let Some(key) = effect_key {
                self.db.execute(
                    "INSERT INTO effect_receipts VALUES(?,?,?,?,?)",
                    params![
                        text(p, "id")?,
                        key,
                        scope,
                        dg,
                        self.encode_stored(
                            &r,
                            json!({"table":"effect","principal":p["id"],"key":key})
                        )?
                    ],
                )?;
                // A new effect receipt can be added while replaying an existing operation.
                // Its durability must advance the restore watermark as well.
                if self.generation()? == self.transaction_generation.get() {
                    self.bump()?;
                }
            }
            Ok(r)
        })();
        match r {
            Ok(r) => {
                if let Err(e) = self
                    .audit(p, name, &a["scope"], "success", r["operation_id"].as_str())
                    .and_then(|_| self.reserve_if_changed())
                {
                    self.db.execute_batch("ROLLBACK")?;
                    return Err(e);
                }
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
        let r: Option<(String, String, Option<i64>, i64)> = self
            .db
            .query_row(
                "SELECT doc,status,expires,revision FROM records WHERE id=? AND scope=?",
                params![id, canonical(scope)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let (doc, status, expiry, revision) = r.ok_or_else(|| err("not_found"))?;
        if matches!(status.as_str(), "deleted" | "expired")
            || expiry.is_some_and(|e| e <= (self.clock)())
        {
            return Err(err("not_found"));
        }
        let record = self.decode_stored(
            &doc,
            Some(&json!({"table":"record","id":id,"revision":revision,"scope":scope})),
        )?;
        if record["id"] != id || record["scope"] != *scope {
            return Err(err("security_payload_invalid"));
        }
        Ok(record)
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
    fn search_hit(&self, mut record: Value, query: &str, available_bytes: usize) -> Option<Value> {
        let original = record["content"].as_str()?.to_owned();
        if canonical(&record).len() <= available_bytes {
            return Some(record);
        }

        if self.security.is_some() {
            return None;
        }

        // Keep the matching passage when possible, then fit it to the remaining
        // byte budget without cutting a UTF-8 code point.
        let lowercase = original.to_lowercase();
        let start = words(query)
            .into_iter()
            .find_map(|token| lowercase.find(&token))
            .unwrap_or(0);
        let start = original
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|i| *i <= start)
            .last()
            .unwrap_or(0);
        let passage = &original[start..];
        let mut low = 0usize;
        let mut high = passage.chars().count();
        let mut best = None;
        while low <= high {
            let mid = low + (high - low) / 2;
            let excerpt = passage.chars().take(mid).collect::<String>();
            record["content"] = json!(excerpt);
            record["content_truncated"] = json!(mid < original.chars().count());
            if canonical(&record).len() <= available_bytes {
                best = Some(record.clone());
                low = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                high = mid - 1;
            }
        }
        best
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
            if old["id"] != a["id"] || old["revision"] != *rev || old["scope"] != a["scope"] {
                return Err(err("security_payload_invalid"));
            }
            if self.security.is_some()
                && old["expires_at"]
                    .as_i64()
                    .is_none_or(|e| e <= (self.clock)())
            {
                return Err(err("not_found"));
            }
            r = old;
            r["historical"] = json!(historical)
        }
        if r["historical"] != true {
            r["status"] = json!(self.current_status(&r)?);
            if r["status"] == "expired" {
                return Err(err("not_found"));
            }
        }
        ensure_record_readable(&r)?;
        Ok(json!({"item":r}))
    }
    fn validate_record(&self, r: &Value) -> Result<(Value, Option<String>)> {
        let mut r = r.clone();
        let tokens = if let Some(provider) = &self.security {
            if r["kind"] != "episode" {
                return Err(err("security_payload_invalid"));
            }
            let policy = provider.allow(
                "validate",
                json!({"kind":"record","value":r,"now":(self.clock)()}),
            )?;
            let deadline = policy["expires_at"]
                .as_i64()
                .filter(|n| *n > (self.clock)())
                .ok_or_else(|| err("security_denied"))?;
            r["expires_at"] = json!(
                r["expires_at"]
                    .as_i64()
                    .map_or(deadline, |n| n.min(deadline))
            );
            let tokens = array(&policy, "index_tokens")?;
            if tokens.len() > 4096
                || tokens.iter().any(|v| {
                    v.as_str().is_none_or(|t| {
                        (t.len() != 64 && !(t.len() == 65 && t.starts_with('h')))
                            || !t
                                .strip_prefix('h')
                                .unwrap_or(t)
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                })
            {
                return Err(err("security_protocol_error"));
            }
            Some(
                tokens
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        } else {
            None
        };
        Ok((r, tokens))
    }
    fn put(&self, r: &Value) -> Result<()> {
        ensure_record_readable(r)?;
        let id = text(r, "id")?;
        let (r, tokens) = self.validate_record(r)?;
        let r = &r;
        let doc = self.encode_stored(
            r,
            json!({"table":"record","id":id,"revision":r["revision"],"scope":r["scope"]}),
        )?;
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
                    tokens.unwrap_or_else(|| words(&format!(
                        "{} {}",
                        text(r, "content").unwrap(),
                        canonical(&r["fact"])
                    ))
                    .join(" "))
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
        let expires = if a["pinned"] == true && self.security.is_none() {
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
    fn clear_lineage_dependents(&self, scope: &str, key: &str, block: bool) -> Result<Vec<String>> {
        let mut queue = vec![key.to_owned()];
        let mut seen = BTreeSet::new();
        while let Some(key) = queue.pop() {
            if !seen.insert(key.clone()) {
                continue;
            }
            if block {
                self.db.execute(
                    "INSERT OR IGNORE INTO tombstones VALUES(?,?)",
                    params![scope, key],
                )?;
            }
            let rows = self.db.prepare(
                "SELECT o.id,o.source_key,o.payload,j.state FROM observations o LEFT JOIN jobs j ON j.id=o.id WHERE o.scope=? AND o.payload!='{}'"
            )?.query_map([scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (id, own_key, payload, job_state) in rows {
                let payload = self.decode_stored(&payload, Some(&json!({"table":"observation","id":id,"scope":serde_json::from_str::<Value>(scope)?})))?;
                let depends_on_key = own_key == key
                    || payload["payload"]["own_source_key"] == key
                    || payload["payload"]["sources"]
                        .as_array()
                        .is_some_and(|sources| {
                            sources.iter().any(|source| source["source_key"] == key)
                        })
                    || payload["payload"]["lineage_keys"]
                        .as_array()
                        .is_some_and(|keys| keys.iter().any(|source_key| source_key == &key));
                if depends_on_key {
                    let aliases = payload["payload"]["own_source_key"]
                        .as_str()
                        .map(str::to_owned)
                        .into_iter()
                        .collect::<BTreeSet<_>>();
                    if block {
                        for alias in &aliases {
                            self.db.execute(
                                "INSERT OR IGNORE INTO tombstones VALUES(?,?)",
                                params![scope, alias],
                            )?;
                        }
                        self.db.execute(
                            "INSERT OR IGNORE INTO tombstones VALUES(?,?)",
                            params![scope, own_key],
                        )?;
                    }
                    self.db.execute(
                        "UPDATE observations SET payload='{}',state='deleted' WHERE id=?",
                        [&id],
                    )?;
                    if job_state.is_some() {
                        self.db
                            .execute("UPDATE jobs SET state='completed' WHERE id=?", [&id])?;
                    }
                    queue.push(own_key);
                    queue.extend(aliases);
                }
            }
        }
        Ok(seen.into_iter().collect())
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
            let mut own_keys = self.strings(
                "SELECT source_key FROM observations WHERE scope=? AND record_id=?",
                params![scope, id],
            )?;
            // Explicitly saved records have no originating observation. Preserve
            // their existing keyed-source closure, including historical sources.
            if own_keys.is_empty() {
                own_keys = self.strings(
                    "SELECT source_key FROM dependencies WHERE record_id=?",
                    [&id],
                )?;
            }
            for key in own_keys {
                let invalidated = self.clear_lineage_dependents(scope, &key, block)?;
                for invalidated_key in invalidated {
                    queue.extend(self.strings("SELECT d.record_id FROM dependencies d JOIN records r ON r.id=d.record_id WHERE d.source_key=? AND r.scope=?",params![invalidated_key,scope])?);
                }
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
            self.db.execute(
                "UPDATE observations SET payload='{}',state='deleted' WHERE record_id=?",
                [&id],
            )?;
            self.db.execute(
                "UPDATE jobs SET state='completed' WHERE id IN (SELECT id FROM observations WHERE record_id=?)",
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
        if let Some(provider) = &self.security {
            provider.allow(
                "validate",
                json!({"kind":"observation","value":obs,"now":(self.clock)()}),
            )?;
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
        let mut blocked = false;
        if typ != "source_invalidated" {
            let record_args = self.derived_args(a["scope"].clone(), obs, &key)?;
            for source in array(&record_args, "sources")? {
                if let Some(source_key) = source["source_key"].as_str()
                    && self
                        .db
                        .query_row(
                            "SELECT 1 FROM tombstones WHERE scope=? AND source_key=?",
                            params![scope, source_key],
                            |row| row.get::<_, i64>(0),
                        )
                        .optional()?
                        .is_some()
                {
                    blocked = true;
                }
            }
            let record = json!({
                "id":"ffffffffffffffffffffffffffffffff",
                "scope":record_args["scope"],"kind":record_args["kind"],
                "revision":i64::MAX,"status":"contested",
                "content":record_args["content"],"sources":record_args["sources"],
                "observed_at":i64::MAX,"verified_at":i64::MAX,"pinned":true,
                "expires_at":i64::MAX
            });
            if typ != "turn_opened" {
                ensure_record_readable(&record)?;
            }
        }
        let old:Option<(String,String,String)>=self.db.query_row("SELECT id,state,payload FROM observations WHERE scope=? AND source_key=? AND digest=?",params![scope,key,text(event,"source_digest")?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((id, state, payload)) = old {
            if !matches!(state.as_str(), "deleted" | "expired")
                && self.decode_stored(
                    &payload,
                    Some(&json!({"table":"observation","id":id,"scope":a["scope"]})),
                )? != *obs
            {
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
        let invalidated = self.clear_lineage_dependents(&scope, &key, false)?;
        let mut previous = previous;
        for invalidated_key in invalidated {
            previous.extend(self.strings("SELECT d.record_id FROM dependencies d JOIN records r ON r.id=d.record_id WHERE r.scope=? AND d.source_key=?",params![scope,invalidated_key])?);
        }
        self.erase(&scope, previous, false)?;
        self.clear_source(&scope, &key, "deleted")?;
        if typ == "source_invalidated" {
            for target in array(&obs["payload"], "source_keys")? {
                let target = target.as_str().ok_or_else(|| err("invalid_argument"))?;
                self.db.execute(
                    "INSERT OR IGNORE INTO tombstones VALUES(?,?)",
                    params![scope, target],
                )?;
                let invalidated = self.clear_lineage_dependents(&scope, target, true)?;
                let mut ids = Vec::new();
                for invalidated_key in invalidated {
                    ids.extend(self.strings("SELECT d.record_id FROM dependencies d JOIN records r ON r.id=d.record_id WHERE r.scope=? AND d.source_key=?",params![scope,invalidated_key])?);
                }
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
                self.encode_stored(
                    obs,
                    json!({"table":"observation","id":id,"scope":a["scope"]})
                )?,
                state
            ],
        )?;
        self.db.execute(
            "INSERT INTO jobs VALUES(?,?,0,?,NULL)",
            params![id, state, (self.clock)()],
        )?;
        let state = if blocked {
            // Do not leave an already-invalid source queued until maintenance:
            // a later child could otherwise publish first through its raw alias.
            let invalidated = self.clear_lineage_dependents(&scope, &key, true)?;
            let mut ids = Vec::new();
            for source_key in invalidated {
                ids.extend(self.strings("SELECT d.record_id FROM dependencies d JOIN records r ON r.id=d.record_id WHERE r.scope=? AND d.source_key=?", params![scope, source_key])?);
            }
            self.erase(&scope, ids, true)?;
            "deleted"
        } else {
            state
        };
        Ok(json!({"accepted":true,"observation_id":id,"processing_state":state,"source_key":key}))
    }
    pub fn maintain(&self) -> Result<bool> {
        match self.maintain_inner() {
            // A deferred protected job was rolled back and durably audited together
            // with retry scheduling/expiry. Preserve its failure for the caller.
            Ok((_, Some(error))) => Err(error),
            Ok((worked, None)) => Ok(worked),
            Err(error) => {
                self.audit(
                    &json!({"id":"maintenance"}),
                    "maintenance",
                    &Value::Null,
                    "denied_or_failed",
                    None,
                )?;
                Err(error)
            }
        }
    }
    fn maintain_inner(&self) -> Result<(bool, Option<Error>)> {
        self.deliver_audit()?;
        self.checkpoint("check")?;
        self.access(
            &json!({"id":"maintenance","role":"system"}),
            "maintenance",
            &Value::Null,
            &Value::Null,
        )?;
        self.begin()?;
        let mut job_id = None;
        let mut attempts = 0;
        let mut deferred_failure = None;
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
                self.db.execute_batch("SAVEPOINT maintenance_job")?;
                let job_result: Result<()> = (|| {
                    let obs = self.decode_stored(&payload, Some(&json!({"table":"observation","id":id,"scope":serde_json::from_str::<Value>(&scope)?})))?;
                    let record_args =
                        self.derived_args(serde_json::from_str(&scope)?, &obs, &key)?;
                    let mut blocked = false;
                    for source in array(&record_args, "sources")? {
                        if let Some(source_key) = source["source_key"].as_str()
                            && self
                                .db
                                .query_row(
                                    "SELECT 1 FROM tombstones WHERE scope=? AND source_key=?",
                                    params![scope, source_key],
                                    |r| r.get::<_, i64>(0),
                                )
                                .optional()?
                                .is_some()
                        {
                            blocked = true;
                            break;
                        }
                    }
                    if blocked {
                        let invalidated = self.clear_lineage_dependents(&scope, &key, true)?;
                        let mut dependent_ids = Vec::new();
                        for invalidated_key in invalidated {
                            dependent_ids.extend(self.strings("SELECT d.record_id FROM dependencies d JOIN records r ON r.id=d.record_id WHERE r.scope=? AND d.source_key=?",params![scope,invalidated_key])?);
                        }
                        self.erase(&scope, dependent_ids, true)?;
                        return Ok(());
                    }
                    self.access(
                        &json!({"id":"maintenance","role":"system"}),
                        "derive",
                        &record_args["scope"],
                        &record_args,
                    )?;
                    let saved = self.save(&record_args)?;
                    self.db.execute(
                        "UPDATE observations SET state='completed',record_id=? WHERE id=?",
                        params![text(&saved, "id")?, id],
                    )?;
                    self.db.execute(
                        "UPDATE jobs SET state='completed',attempts=attempts+1,error=NULL WHERE id=?",
                        [&id],
                    )?;
                    Ok(())
                })();
                match job_result {
                    Ok(()) => self.db.execute_batch("RELEASE maintenance_job")?,
                    Err(error) if self.security.is_some() => {
                        self.db.execute_batch(
                            "ROLLBACK TO maintenance_job; RELEASE maintenance_job",
                        )?;
                        // Recheck policy later without spending the ordinary terminal retry
                        // budget. Moving this job behind other due jobs prevents starvation.
                        self.db.execute(
                            "UPDATE jobs SET next_at=?,error=? WHERE id=?",
                            params![now + 1, error.0, id],
                        )?;
                        deferred_failure = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
            if job_id.is_some() || !expired.is_empty() {
                self.bump()?
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                if let Err(e) = (|| {
                    if self.generation()? != self.transaction_generation.get() {
                        self.audit(
                            &json!({"id":"maintenance"}),
                            "maintenance",
                            &Value::Null,
                            if deferred_failure.is_some() {
                                "denied_or_failed"
                            } else {
                                "success"
                            },
                            None,
                        )?;
                    }
                    self.reserve_if_changed()
                })() {
                    self.db.execute_batch("ROLLBACK")?;
                    return Err(e);
                }
                self.commit()?;
            }
            Err(e) => {
                self.db.execute_batch("ROLLBACK")?;
                if self.security.is_some() {
                    return Err(e);
                }
                if let Some(id) = &job_id {
                    let n = attempts + 1;
                    self.db.execute("UPDATE jobs SET state=?,attempts=?,next_at=?,error='processing_failed' WHERE id=?",params![if n>=5{"failed"}else{"accepted"},n,(self.clock)()+(1i64<<n),id])?;
                } else {
                    return Err(e);
                }
            }
        }
        self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok((job_id.is_some(), deferred_failure))
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
        let tokens = query_words(text(a, "query")?);
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
        let mut consumed = 0;
        let mut content_truncated = false;
        let cap = a["budget"]["max_utf8_bytes"]
            .as_u64()
            .unwrap()
            .min(a["budget"]["estimated_tokens"].as_u64().unwrap()) as usize;
        let mut omitted_for_budget = false;
        for mut r in rows.clone() {
            if self.security.is_some() {
                let revision: Option<i64> = self
                    .db
                    .query_row(
                        "SELECT revision FROM records WHERE id=? AND scope=?",
                        params![text(&r, "id")?, canonical(&r["scope"])],
                        |row| row.get(0),
                    )
                    .optional()?;
                if revision != r["revision"].as_i64() {
                    return Err(err("security_payload_invalid"));
                }
            }
            authorize(p, &r["scope"])?;
            self.access(p, "memory.search", &r["scope"], &r)?;
            if !self.valid(&r["fact"]) {
                consumed += 1;
                continue;
            }
            r["status"] = json!(self.current_status(&r)?);
            if items.len() + conflicts.len() >= a["budget"]["max_items"].as_u64().unwrap() as usize
            {
                break;
            }
            let contested = r["status"] == "contested";
            let used = canonical(&json!({"items":items,"conflicts":conflicts})).len();
            let remaining_record_bytes = cap.saturating_sub(used.saturating_add(1));
            let Some(r) = self.search_hit(r.clone(), text(a, "query")?, remaining_record_bytes)
            else {
                let empty_reference_bytes = canonical(&json!({"items":[],"conflicts":[]})).len();
                let fresh_page_record_bytes =
                    cap.saturating_sub(empty_reference_bytes.saturating_add(1));
                if self
                    .search_hit(r.clone(), text(a, "query")?, fresh_page_record_bytes)
                    .is_some()
                {
                    // This result fits on a new page; leave it unconsumed so the
                    // signed cursor resumes at this row instead of losing it.
                    break;
                }
                omitted_for_budget = true;
                consumed += 1;
                continue;
            };
            content_truncated |= r["content_truncated"] == true;
            if contested {
                conflicts.push(r)
            } else {
                items.push(r)
            }
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
            json!({"items":items,"conflicts":conflicts,"next_cursor":cursor,"truncated":more || content_truncated || omitted_for_budget,"budget":{"utf8_bytes":canonical(&json!({"items":items,"conflicts":conflicts})).len(),"estimator":"utf8-byte-upper-bound","max_bytes":cap}}),
        )
    }
}
