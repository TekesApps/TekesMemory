# Observation lineage and forget closure

## Contract

Every derived episode retains all upstream `source_key` values supplied by its observation and also depends on the observation event that produced it. Kernel adapter summaries assign a stable key to each visible ledger row they copy in a compact `lineage_keys` array, so `tool.completed` and a later `turn.settled` share the tool-result dependency. The Store converts those keys to bounded source descriptors when writing the derived record. Invalidation and forgetting tombstone the selected source, remove every record and historical version reachable through dependency edges, delete FTS entries, and cancel queued observations whose payload names that source.

One MCP request remains bounded by the existing 2 MiB body limit. The compact `lineage_keys` array has a 4096-item limit, matching the existing bounded `covers` list. The adapter rejects a derived observation whose deduplicated lineage exceeds that limit with `source_unavailable`; it never silently drops dependencies. The event-level `sources` list keeps its existing limit. The generated episode content omits both `sources` and `lineage_keys`; lineage remains metadata and does not crowd the summary.

## Existing databases

Existing dependency rows and source keys are preserved. Queued payloads that explicitly contain `source_key` are honored when their jobs run and are removed if that key is invalidated. The Store fixture checks this payload shape across reopen; it does not simulate a pre-fix database migration. Previously generated aggregate records whose sources contain only host/ref/digest have no stored host-instance, workspace/thread/line identity for their copied source rows. Those lost edges cannot be reconstructed safely from the current database, and guessing could delete unrelated records. Replaying an already accepted old event remains idempotent; it does not rewrite its historical payload. Full closure is guaranteed for newly captured lineage and explicit keys present in persisted payloads, not for unrecoverable aggregate edges already lost before this change.

## Verification

The Store tests exercise processed and queued explicit A to B dependencies, invalidation, tombstones, historical versions, FTS cleanup, and same-scope closure. The process test drives the real Kernel adapter through `tool.completed` to `turn.settled` to `memory.forget` and confirms the copied output cannot be searched or fetched afterward, including after service restart.
