# Security provider v1 (Issue #13)

Status: implementation candidate, local synthetic verification only. This public contract is independent of consumer products. Tracks [Issue #13](https://github.com/TekesApps/TekesMemory/issues/13).

A whole Store opts into a trusted `security_provider` service configuration. Ordinary Stores remain unchanged. The protected Store persists its random store ID, profile, policy version and provider identity. Removing or changing these bindings fails closed. Normal startup refuses a nonempty ordinary Store with `security_migration_required`. Explicit offline `security-maintenance --migrate-opaque` validates legacy client-encrypted records/history/observations, wraps all payloads/results, rebuilds approved indexes and writes the protection marker in one transaction. Failure rolls back the batch; it does not convert arbitrary plaintext or discard history. Protected Stores use SQLite user_version 2 so old binaries reject them; ordinary Stores remain version 1.

The provider is a separate, trusted local service. It must not call Memory recursively or acquire a consumer business lock held while that consumer calls Memory. Credentials, keys, policy state and the external anti-rollback ledger live outside the Memory backup. Memory never executes provider-supplied scripts. The provider is security-critical, not an untrusted model.

## Deployment and wire

`security_provider` contains `version:1`, `endpoint:"http://127.0.0.1:PORT/security/v1"`, `credential_file` (absolute private JSON containing `token`), `provider_id`, `profile`, `policy_version`, `purpose`, and `timeout_ms` (100–30000). No redirects or proxies; loopback IPv4 only. POST JSON with Bearer authentication, maximum request/response 8 MiB, bounded timeout. Response errors are stable Memory codes, never remote response bodies. Consumers implement this ordinary HTTP interface in any language, including Python.

Every request: `{version:1, provider_id, profile, policy_version, purpose, store_id, action, input}`. Every success: `{version:1, ok:true, output:{...}}`; failures `{version:1, ok:false, code:CODE}`. Codes: `security_denied`, `security_key_unavailable`, `security_unknown_key_version`, `security_payload_invalid`, `security_restore_required`, `security_unavailable`. Unknown/malformed responses fail as `security_protocol_error`.

| Action | Input | Output / obligation |
| --- | --- | --- |
| access | principal (id/role/owner only), operation, scope, resource, now | `allow:true`; evaluate current grants, source versions, deletion generation, purpose; no returned expanded scope is used |
| validate | kind (`record`/`observation`), value, now | `allow:true`; record also returns mandatory `expires_at` and `index_tokens` (array of 64 lowercase hex HMAC tokens, optionally prefixed by `h`, maximum 4096); verify opaque format, key versions, allowed metadata and dependencies |
| seal | context, value | `envelope` object with nonempty `key_version`; authenticated encryption binds complete request binding plus context; must never return plaintext |
| open | context, envelope | `value`; authenticate context/version before releasing value; dual-read old/new keys; never create missing keys |
| keys | empty object | `write_key_version` (nonempty string); current outer write key, unrelated to client inner format or owner/index derivation |
| checkpoint | mode (`check`/`reserve`), generation | `allow:true`; independent durable ledger checks exact generation; reserve atomically advances to generation+1 |
| audit | event | `accepted:true,event_id` only after durable idempotent acceptance; duplicate event_id is harmless |

The outer seal protects JSON held in records, historical versions, observations, operations and effect receipts. Context identifies the table/row and binds owner/scope where applicable. Jobs contain no body, only observation ID and fixed error codes. SQLite routing identifiers, scope, kind, revision, status, times, source keys, digests, indexes, sizes and access patterns remain visible; these are not claimed anonymous. Only provider-approved HMAC index tokens enter FTS. The existing client-encrypted envelope and HMAC token identity are unchanged: opening the outer envelope does not decrypt the client's inner ciphertext.

Protected content is opaque. No natural-language scrubbing, snippet truncation, fact extraction or summarization runs on it. Opaque observations supply an already-encrypted summary; background publication copies that summary exactly and rechecks policy, dependencies and expiry. Opaque records use episode kind; semantic fact/procedure interpretation is unsupported in this profile. The provider validates legacy inner-envelope syntax and explicit allowed metadata; it must not accept arbitrary plaintext because it happens to occupy a string field.

## Enforcement and reliability

Built-in tool/role/scope permission is evaluated first and remains authoritative. Provider authorization additionally covers each returned record and its original scope, historical versions, operation/effect receipts, background work, expiry and source invalidation. A mandatory deadline is the minimum of provider deadline and requested/current deadline. Pin never removes it. Provider failure cannot release content or create unprotected rows. A source tombstone prevents queued and replayed writes; the provider additionally owns current authorization/deletion generations.

Audit events contain only event ID, operation, success/denied/failure result, hashed principal/scope, timestamp and operation ID. No request body, token, raw query or source text. Successful mutation and audit outbox row commit in one SQLite transaction. Reads, status and effect reconciliation durably enqueue audit and require independent provider acceptance before release. If independent audit is unavailable the event remains pending and no result is released; otherwise restoring an unchanged business snapshot could erase the sole evidence of a successful read. Standalone denials/failures also require independent acceptance when storage/provider are available. A successful read event means authorized release, not proof that the remote caller received bytes. Delivery failure leaves `pending` in the durable outbox and never changes a committed mutation to a claimed rollback. Retry uses the same event ID. Ordinary operation/effect idempotency continues to reconcile writes; audit delivery is separately observable.

External restore ledger: before a changing transaction commits, `reserve(generation)` durably advances the independent ledger; the same transaction bumps its database generation. A normal next operation/restart checks equality. A lost reserve response or crash between reservation and SQLite commit creates an explicit `security_restore_required` quarantine, not an inferred rollback. This concrete interruption is indistinguishable from restoring an old snapshot; automatically lowering the external ledger would resurrect deletions. Retry of an already committed effect does not reserve again; attaching a new effect key to an existing operation does advance the watermark. Background cancellation also advances it even when no new record is published. Operations already committed remain committed even if later audit delivery fails. Operators recover by restoring the latest verified database and deletion journal or by a separately authorized purge/reimport; this implementation does not lower a provider watermark or supply a generic backup engine.

Keys: provider controls current write version and accepted read versions. Bounded maintenance rewraps existing envelopes using open+seal with identical row context, preserving record IDs, owner IDs, revisions, inner ciphertext and index tokens. Each batch is atomic; interrupt/restart resumes by key_version mismatch, unknown old key fails closed. Old keys remain until all live/history/receipt/observation rows and required backups are verified migrated. Changing content keys does not rotate identity/index keys.

Production key facilities, remote independent audit durability and operator backup recovery require separate deployment evidence. Local fixtures do not certify compliance.

## Consumer configuration and operation

```json
{"security_provider":{"version":1,"endpoint":"http://127.0.0.1:43210/security/v1","credential_file":"/absolute/private/provider-credential.json","provider_id":"local-security","profile":"opaque-v1","policy_version":"1","purpose":"memory","timeout_ms":2000}}
```

The provider's `validate` implementation interprets its consumer's inner format. For example, a compatible legacy content string is `v1.<base64 nonce+ciphertext>\n<h-prefixed HMAC tokens separated by spaces>`. Memory does not parse that inner format; it passes it unchanged and uses only `index_tokens` returned by the provider. The example Python fixture validates syntax and never knows the client's inner plaintext key. Authenticating/decrypting inner content belongs to the consumer; outer AAD/ciphertext authentication belongs to this provider. Metadata and inner encryption authenticity must be validated at their respective trust boundaries.

Offline commands (stop the service first; the same exclusive service lock is required):

```sh
tekes-memory security-maintenance --config /absolute/service.json --migrate-opaque
tekes-memory security-maintenance --config /absolute/service.json --max-rows 64
```

The first is an explicit single-transaction legacy opaque migration. The second rewraps at most 64 current/history/observation/operation/effect rows and delivers pending audit. Repeat until `rows_rewrapped:0`; `audit_pending` reports remaining delivery. Migration rotates only the cursor secret/restore generation, invalidating old cursors, and preserves owner, record IDs, revisions, content and HMAC index identity. Original files/backups and provider facilities require independent retention governance. Failed migration before commit leaves the legacy Store usable; an unused new provider ledger identity may remain. A reserve/commit ambiguity on an already-protected Store must not be "repaired" by lowering the external watermark.

The service performs ordinary background derivation/expiry and audit delivery; key migration is an explicit maintenance operation. Protected background policy errors preserve the accepted job and return failure for later policy recovery, instead of permanently consuming retry attempts while authorization is revoked. No response implies that external backups or disk snapshots were erased.

`security.audit_state` on successful tool responses is `delivered` for get/search/status and `pending` for transactionally queued write audit. Effect reconciliation itself requires independent audit acceptance. Existing operation/effect receipts remain minimal ID/revision/outcome metadata; they are historical evidence of submission, not a renewed right to read deleted or expired record content.

Local verification uses `tests/support/security_provider.py`, a synthetic AES-256-GCM outer provider with an independent SQLite watermark and audit sink. It requires Python `cryptography`; select an interpreter using `MEMORY_TEST_PYTHON` (default `python3`). It is deliberately not a production policy/key service. Tests do not claim real client inner-envelope authenticity from placeholder content; actual consumer Vault roundtrip is a separate integration acceptance.

### Exact authenticated row contexts

`record`: `{table:"record",id,revision,scope}` (shared by matching current/history revision); `observation`: `{table:"observation",id,scope}`; `operation`: `{table:"operation",principal,scope,key}`; `effect`: `{table:"effect",principal,key}`. The provider authenticates this complete context together with version/provider_id/profile/policy_version/purpose/store_id. Provider policy is current on every access; policy_version identifies the configured contract, not permission to reuse a stale grant. Policy changes that retain the contract may revoke immediately without changing that identifier.

Memory persists `{security_v:1,context,envelope}`. Envelope algorithm/nonce/ciphertext fields are provider-owned; `key_version` is required and is used only for bounded rewrap selection. Audit request events contain schema_version, store_id, event_id, operation, outcome, principal_hash, scope_hash, at, operation_id. They exclude plaintext/key material and can be kept independently of business deletion. Providers must return stable allow/deny and classify key/format failures using the codes above; HTTP failures/timeouts never include provider response bodies in Memory errors.
