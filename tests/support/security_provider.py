#!/usr/bin/env python3
"""Synthetic local provider fixture, never a deployment key/policy service.
Requires cryptography; owns an independent SQLite ledger and audit sink.
"""
import base64
import hashlib
import json
import os
import sqlite3
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

root = sys.argv[1]
def compact(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
def key(version):
    return hashlib.sha256(("SYNTHETIC-ONLY-" + version).encode()).digest()
def aad(request, context):
    return compact({k: request[k] for k in ("version", "provider_id", "profile", "policy_version", "purpose", "store_id")} | {"context": context})
def fail(code):
    raise ValueError(code)
def policy(request):
    with open(root + "/control.json") as f:
        control = json.load(f)
    data = request["input"]
    action = request["action"]
    with sqlite3.connect(root + "/provider.sqlite3") as db:
        db.execute("PRAGMA synchronous=FULL")
        db.execute("CREATE TABLE IF NOT EXISTS watermarks(id TEXT PRIMARY KEY, generation INTEGER)")
        db.execute("CREATE TABLE IF NOT EXISTS audit(id TEXT PRIMARY KEY, event TEXT)")
        if action == "checkpoint":
            generation = data["generation"]
            row = db.execute("SELECT generation FROM watermarks WHERE id=?", (request["store_id"],)).fetchone()
            if row is None:
                if generation != 0:
                    fail("security_restore_required")
                db.execute("INSERT INTO watermarks VALUES(?,0)", (request["store_id"],))
            elif row[0] != generation:
                fail("security_restore_required")
            if data["mode"] == "reserve":
                db.execute("UPDATE watermarks SET generation=? WHERE id=?", (generation + 1, request["store_id"]))
                db.commit()
                if control.get("lose_reserve_response"):
                    fail("security_unavailable")
            return {"allow": True}
        if action == "access":
            if data["operation"] in control.get("deny", []):
                fail("security_denied")
            resource = data.get("resource") or {}
            if isinstance(resource, dict):
                if resource.get("expires_at", 10**18) is not None and resource.get("expires_at", 10**18) <= data["now"]:
                    fail("security_denied")
                if any(s.get("source_key") in control.get("revoked_sources", []) for s in resource.get("sources", [])):
                    fail("security_denied")
            return {"allow": True}
        if action == "validate":
            value = data["value"]
            if data["kind"] == "observation":
                if value["type"] in ("turn_opened", "source_invalidated"):
                    return {"allow": True}
                body = value["payload"]["summary"]
            else:
                body = value["content"]
            try:
                ciphertext, _, words = body.partition("\n")
                if not ciphertext.startswith("v1."):
                    fail("security_unknown_key_version")
                raw = base64.urlsafe_b64decode(ciphertext[3:] + "==")
                if len(raw) < 28:
                    fail("security_payload_invalid")
            except (KeyError, json.JSONDecodeError):
                fail("security_payload_invalid")
            return {"allow": True, "expires_at": control.get("deadline", data["now"] + 3600), "index_tokens": words.split()}
        if action == "keys":
            return {"write_key_version": control.get("write_key", "k1")}
        if action == "seal":
            version = control.get("write_key", "k1")
            nonce = os.urandom(12)
            sealed = nonce + AESGCM(key(version)).encrypt(nonce, compact(data["value"]), aad(request, data["context"]))
            return {"envelope": {"key_version": version, "algorithm": "AES-256-GCM", "ciphertext": base64.urlsafe_b64encode(sealed).decode()}}
        if action == "open":
            envelope = data["envelope"]
            version = envelope["key_version"]
            if version not in control.get("read_keys", ["k1"]):
                fail("security_unknown_key_version")
            if control.get("key_unavailable"):
                fail("security_key_unavailable")
            try:
                raw = base64.urlsafe_b64decode(envelope["ciphertext"])
                value = AESGCM(key(version)).decrypt(raw[:12], raw[12:], aad(request, data["context"]))
                return {"value": json.loads(value)}
            except Exception:
                fail("security_payload_invalid")
        if action == "audit":
            if control.get("audit_fail"):
                fail("security_unavailable")
            event = data["event"]
            db.execute("INSERT OR IGNORE INTO audit VALUES(?,?)", (event["event_id"], compact(event).decode()))
            db.commit()
            if control.get("lose_audit_response"):
                fail("security_unavailable")
            return {"accepted": True, "event_id": event["event_id"]}
    fail("security_protocol_error")

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass
    def do_POST(self):
        if self.path != "/security/v1" or self.headers.get("Authorization") != "Bearer synthetic-provider-token":
            self.send_error(403)
            return
        size = int(self.headers.get("Content-Length", "0"))
        if size > 8 * 1024 * 1024:
            self.send_error(413)
            return
        request = json.loads(self.rfile.read(size))
        try:
            output = policy(request)
            response = {"version": 1, "ok": True, "output": output}
        except ValueError as error:
            response = {"version": 1, "ok": False, "code": str(error)}
        raw = compact(response)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(root + "/ready.json", "w") as f:
    json.dump({"port": server.server_port}, f)
server.serve_forever()
