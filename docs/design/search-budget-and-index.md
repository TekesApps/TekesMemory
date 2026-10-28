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
When a hit cannot fit in the remaining page but does fit on a fresh page, the
cursor resumes at that hit. A hit whose required metadata cannot fit even on a
fresh page is skipped and reported through `truncated`, while scanning can
continue to later hits.

The Kernel adapter subtracts the exact serialized empty-wrapper overhead from
both configured limits before calling search, because the Store uses the lower
of those two limits. At their schema minima the adapter clamps to valid inputs;
the final wrapped-size check then safely omits material that cannot fit.

This updates [Issue #5](https://github.com/TekesApps/TekesMemory/issues/5) and
[Issue #6](https://github.com/TekesApps/TekesMemory/issues/6).
