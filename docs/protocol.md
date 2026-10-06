# Protocol and architecture

LSP 3.17 over stdio, UTF-16 positions, incremental synchronization. Stdout contains protocol frames only. Standard handlers implement completion, hover, signatures, definitions, references, prepare/rename, highlights, document/workspace symbols, folding, formatting, code actions, and commands. Formatting is advertised when enabled at initialization. Semantic tokens are absent.

**Custom requests**

| Request | Parameters | Result |
|---|---|---|
| `duckdb/status` | `{"uri":"file:///..."}`, or JSON null for all documents | `{"documents":[...]}`: mode, provider, config paths/issues, render state, worker error, connection ID, table count, refresh state, catalog error, engine version |
| `duckdb/setConnection` | `{"uri":"file:///...","profile":{...}}` | `{"connectionId":"..."}`; schedules refresh |
| `duckdb/catalogDocument` | `{"uri":"duckdb-lsp://<id>/<catalog>/<schema>/<table>"}` | `{"text":"...CREATE TABLE...","languageId":"duckdbsql"}` |

Status excludes profiles, connection values, and template values. Catalog URI path segments are percent encoded. Database definitions return these URIs; the Neovim bridge displays them as read-only buffers.

Profiles accept `path`, optional `alias` (default `db`), `attachments`, `defaultCatalog`, `defaultSchema`, installed `extensions`, and `mockCatalog` for deterministic fixtures. Attachments accept `type` (`duckdb` or `ducklake`), `alias`, `path`, and optional DuckLake `snapshotVersion`. A null profile resets to an unconnected catalog. Identity includes the complete profile and workspace root, so relative paths and historical selections cannot share unrelated caches.

`workspace/executeCommand` supports `duckdb.refreshCatalog` and `duckdb.explainConfiguration`, each with `arguments: [{"uri":"file:///..."}]`. Refresh returns `{"scheduled":true}`. Explicit configuration inspection returns normalized values and provenance, which can include configured template values; review before sharing its output.

**Scheduling**

Each document has immutable text, a scope index, config, version, and generation. Text/config/profile changes advance its generation. Worker results are committed only when generation and text identity still match. Diagnostics include the current document version. Closing a buffer clears diagnostics; editing immediately invalidates rendered text.

Completion never waits for a worker or database. A uniquely mapped literal source span may use rendered scopes, while completion edits always replace the original identifier. Loops that duplicate source spans fail this uniqueness check. Diagnostics use SQLFluff source mappings and reject generated locations. Rename operates only on original raw SQL scopes.

Three persistent Python workers separate database I/O, rendering/analysis, and formatting. Requests are serialized per worker and have deadlines. Cancellation drops and kills the child, preventing the next request from consuming an obsolete response. Debounced obsolete analysis is discarded before entering its worker. Source connections exist only during metadata refresh. Two empty shadow databases are cached inside the analysis worker; local declarations are rolled back per analysis.

Native validation extracts statements with DuckDB, reconstructs explicit column declarations in memory, and EXPLAINs SELECT statements after disabling external access and locking configuration. It never attaches source databases or executes user DML/DDL against them. Recovery at statement boundaries allows later diagnostics after earlier syntax errors. Rust sqlparser's DuckDB dialect is a fallback when native validation is unavailable.

**Editor integration**

The Lua bridge uses Neovim's built-in LSP API and Dadbod's `db#resolve`, `db#adapter#duckdb#dbext`, and `*/DBExecutePost`. It shares connection metadata, not a running session. Transient attachments in a separate Dadbod process must be represented explicitly in the LSP profile.

Capitalize keyword/function/type suggestions from supported config rules; apply indentation to snippets. Other rules belong to the selected formatter/linter. SQLFluff provides Jinja compatibility; dbt and arbitrary third-party templaters are outside this adapter.
