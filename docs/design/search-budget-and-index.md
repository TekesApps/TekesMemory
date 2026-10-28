# Search budget and complete indexing (Issues #5 and #6)

Search must discover every token in a valid stored record while returning bounded
reference material. The search index therefore uses the complete document token
set; query parsing retains its 256 distinct-token limit. Existing databases are
reindexed once at open using a `search_index_version` marker, without changing
the SQLite schema version or stored record format.

Search results retain the record identity, scope, source metadata, and status
fields needed by callers. When full content does not fit, `content` becomes a
query-centered excerpt and `content_truncated` is true. The response-level
`truncated` is true when an excerpt was shortened or additional hits remain.
`budget.utf8_bytes` measures the serialized `items` and `conflicts` reference
arrays; the full record remains available through `memory.get`.

This updates [Issue #5](https://github.com/TekesApps/TekesMemory/issues/5) and
[Issue #6](https://github.com/TekesApps/TekesMemory/issues/6).
