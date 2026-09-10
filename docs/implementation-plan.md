**DuckDB / DuckLake LSP — implementation plan**

Design baseline · 10 September 2026

Build a performant, Neovim-first language server for DuckDB SQL, with optional DuckLake catalog access, sqruff/SQLFluff configuration support, and explicit Jinja buffer modes. Completion must remain useful without a database connection or a successful template render.

This document captures the agreed direction and proposed implementation milestones. It is a plan, not a claim of completed functionality. The exploratory Python engine is a disposable feasibility prototype; it does not establish the production stack.

**1. Agreed product boundaries**

| Component | Responsibility |
|---|---|
| Rust language server | Document state, SQL scope analysis, completion, hover, signatures, navigation, semantic diagnostics, and worker coordination |
| Small Neovim Lua integration | Filetype/language ID mapping, buffer connection selection, Dadbod integration, refresh commands, and status |
| Existing Tree-sitter setup | SQL syntax highlighting and any Jinja highlighting/injections |
| sqruff or SQLFluff adapter | Effective project settings, formatting, linting, and supported template context |
| Template worker | Jinja rendering and mappings between template source and rendered SQL |
| Database worker | Read catalog metadata through DuckDB and supported DuckLake attachments |
| Dadbod / Dadbod UI | Execute user queries and display results |

Initial releases will omit LSP semantic tokens. Existing `duckdbsql` highlighting remains the source of syntax highlighting. Database access is optional; SQL completion must not depend on installing either lint tool.

**2. Filetypes and language modes**

Use the buffer filetype as the explicit language signal. The Lua integration must preserve the intended LSP `languageId` and handle a filetype change by reopening or reattaching the document with its new mode.

| Neovim filetype | Dialect | Rendering behavior |
|---|---|---|
| `duckdbsql` | DuckDB | Analyze original SQL directly; do not render |
| `jinjaduckdbsql` | DuckDB | Resolve Jinja context and render when configuration and dependencies permit |
| `sql` | Project-selected | Optional attachment when configuration selects DuckDB; use its configured templater |

Do not infer Jinja from braces in `duckdbsql` buffers. A custom filetype selects the language mode; project configuration supplies context and style. If that configuration explicitly conflicts with the dialect or templater, show a configuration diagnostic and preserve local/catalog completion. For a Jinja templater conflict, defer rendering until resolved instead of running a different renderer.

Register the custom filetypes with the existing SQL indentation and Dadbod behavior. Reuse the user's Tree-sitter setup; adding Jinja highlighting is a separate editor task. No filename convention or replacement grammar is required by the LSP.

**3. Detect and resolve sqruff / SQLFluff configuration**

Detect configuration files and executable availability independently. An absent executable should disable only features that require running that tool, such as its formatter or renderer. Read supported declarative settings for completion without launching a tool for each request.

sqruff documents `.sqruff`, `.sqruff.ini`, `sqruff.toml`, and `pyproject.toml`, with nested configuration. SQLFluff documents several INI filenames, `.sqlfluff`, and `pyproject.toml`, as well as file-level directives and special templater inheritance rules. Implement provider-specific resolution rather than a generic “nearest file wins” approximation. [sqruff configuration](https://playground.quary.dev/docs/usage/configuration/), [SQLFluff configuration](https://docs.sqlfluff.com/en/stable/configuration/setting_configuration.html)

Proposed setting: `configProvider = auto | sqruff | sqlfluff | none`.

Auto mode selects an unambiguous provider. Shared TOML conventions can make ownership ambiguous; a `tool.sqlfluff` section alone is not conclusive evidence that only SQLFluff is intended. When selection is ambiguous, expose it in status, use documented neutral completion defaults, and let an explicit provider resolve it. Do not silently merge two providers' rule sets.

Maintain one normalized configuration snapshot per effective file configuration. Record each setting's source for troubleshooting, watch relevant files, and invalidate only affected documents. Version the snapshot so obsolete worker results cannot overwrite newer settings.

| Setting category | Completion behavior |
|---|---|
| Dialect | DuckDB syntax, keywords, types, and function suggestions |
| Keyword/function capitalization | Insertion style for generated suggestions |
| Indentation | Multiline snippet layout |
| Jinja context and supported macro definitions | Variable, member, and macro-name suggestions |
| Other lint rules | Apply through the selected linter; only influence completion where a specific mapping is implemented |

Preserve actual database identifier spelling and required quoting. Do not infer a database schema from lint configuration. Do not expose configured secrets or raw connection strings in completion details or logs.

**4. Jinja rendering and source mapping**

Keep the original buffer and rendered SQL as separate, versioned representations. The original buffer owns editor ranges and edits. Rendered SQL supplies additional SQL structure for analysis.

In `jinjaduckdbsql`, load supported context, macros, and includes using the selected provider's semantics. Jinja rendering can depend on configured variables and macros; compatibility must be tested against actual project examples. [SQLFluff Jinja configuration](https://docs.sqlfluff.com/en/stable/configuration/templating/jinja.html)

Render in a persistent background worker after a short debounce. Cache by document version, effective configuration, and template dependencies. Cancel or discard obsolete work. Do not render synchronously when the user requests completion.

Source mappings must handle literal SQL, substituted expressions, whitespace trimming, includes, loops that repeat source spans, and generated SQL with no unique source location. Only offer edits or mapped diagnostics where the mapping is valid. Report unmappable template failures at their source construct; never apply a rendered offset directly to the original buffer.

| Editing context | Available assistance |
|---|---|
| Inside a Jinja expression | Declared variables, supported members, macros, filters, and template syntax |
| SQL outside template constructs | SQL scope suggestions and cached database metadata |
| Successfully rendered statement | Additional resolved structure, column inference, and mapped diagnostics |
| Missing context or incomplete template | Local syntax and catalog suggestions; reduced semantic confidence around unresolved spans |

Undefined variables must not silently become empty text that is then treated as authoritative SQL. Retain the last successful render only as a marked analysis fallback; do not publish its stale diagnostic ranges against a newer document. A render failure must not suppress unrelated database completion.

The initial compatibility target is ordinary Jinja with declared context and supported macros/includes. Full dbt execution, arbitrary Python extension behavior, and all third-party templaters require separate adapters and compatibility work. A Rust Jinja implementation must not be assumed equivalent to Python Jinja. The core server stays Rust; a Python helper may be required for SQLFluff/Jinja compatibility.

**5. SQL analysis and completion**

Choose a parser through a focused compatibility spike before committing to a dependency. Evaluate recovery on incomplete SQL, source positions, query scopes, DuckDB syntax coverage, and incremental analysis cost. Compare candidate Rust parsers or reusable sqruff components with DuckDB's native parser on valid SQL. Reusing a parser does not require replacing the editor's highlighting grammar.

Maintain document text and a query-scope model. Reuse unchanged analysis where practical and prioritize the active statement. Native DuckDB validation is a background capability, with isolation and timeouts; it must not block normal completion.

Completion combines these sources, with ranking driven by cursor context:

| Source | Examples |
|---|---|
| Current query | CTEs, aliases, derived columns, correlated references |
| Current document | Explicit table declarations and supported local definitions |
| Active catalog cache | Catalogs, schemas, tables, views, columns, types, functions, and available signatures |
| Built-in knowledge | DuckDB keywords, functions, types, and SQL snippets when offline |
| Template configuration | Declared Jinja names and supported macro signatures |

Prioritize a qualifier's columns after `alias.`, relations after `FROM`/`JOIN`, and in-scope expressions in a select list. Respect alias shadowing, quoted identifiers, and ambiguous columns. Deduplicate suggestions across sources. Track engine versions and extension availability where known; offline function knowledge is a versioned baseline rather than proof that every function exists in the active session.

Typing must never trigger execution of buffer SQL. Any native binding checks must be restricted and isolated: planning can still invoke extension or external-resource behavior. Implement safe validation boundaries before enabling these checks automatically.

**6. Database completion and DuckLake**

Use metadata queries to populate an in-memory catalog index. Read tables, views, columns, types, and functions through DuckDB's public metadata interfaces. Fetch additional details lazily for large catalogs. Ordinary completion should not scan table data or remote Parquet files. [DuckDB metadata functions](https://duckdb.org/docs/current/sql/meta/duckdb_table_functions)

Cache identity must include the connection profile, attachment set, active catalog/schema, and snapshot selection where relevant. Never mix metadata across buffers with different profiles. Retain the last successful catalog with a visible stale/offline status when refresh fails.

Refresh on connection selection, explicit refresh, cache expiry, and supported post-query integration signals. Coalesce requests, limit concurrency, and back off after failures. Refresh failure is connection status, not evidence that every referenced table is invalid.

For DuckLake, use configured read-only attachments to discover catalogs and tables. The public attachment interface supports read-only mode; snapshot queries and time travel are documented features. [DuckLake connections](https://ducklake.select/docs/stable/duckdb/usage/connecting), [snapshots](https://ducklake.select/docs/stable/duckdb/usage/snapshots), [time travel](https://ducklake.select/docs/stable/duckdb/usage/time_travel)

Start with current-schema completion. Add snapshot listing and version suggestions next. Historical-schema completion requires a separately keyed snapshot-specific catalog; current columns must not be presented as verified historical columns.

Local `.duckdb` files need special connection handling. A separate process holding a read-only connection can conflict with a writer. Prefer short-lived metadata reads, release connections promptly, coordinate with Dadbod execution where possible, and fall back to cached metadata when another writer holds the file. External writers remain outside the plugin's coordination. PostgreSQL-backed DuckLake can use a different connection lifecycle. [DuckDB concurrency](https://duckdb.org/docs/current/connect/concurrency)

**7. Neovim and Dadbod integration**

Keep the server usable through standard LSP. Ship a small Lua module for the editor-specific connection bridge and custom filetypes. Completion should work through Neovim's LSP client and compatible completion frontends without a dedicated completion-source plugin.

Read Dadbod's selected connection through its supported interfaces, normalize it into a profile, and associate that profile with the buffer. Verify actual Dadbod/Dadbod UI hooks during implementation; do not assume a particular query-completion event exists. Provide explicit refresh and profile-selection commands as dependable fallbacks. Dadbod supports DuckDB and URL-based connection selection. [Dadbod documentation](https://github.com/tpope/vim-dadbod)

Sharing connection settings does not share a running database session. Temporary tables, session variables, loaded extensions, and in-memory attachments need explicit profile/bootstrap support or later session cooperation. Do not promise automatic visibility into Dadbod's transient session state.

Proposed Lua commands cover server status, selecting a profile, refreshing the catalog, and explaining effective configuration. Status shows language mode, chosen configuration provider, available tools, render state, and catalog freshness, with sensitive fields redacted.

Define exactly one owner of formatting and style diagnostics for each buffer. A user can retain an existing sqruff integration or route the selected tool through this server. Disable overlapping capabilities in the other integration. Semantic diagnostics remain the DuckDB LSP's responsibility.

**8. Performance and resilience requirements**

Completion reads cached configuration, SQL analysis, and metadata. Database calls, template rendering, and linting run outside that request path. Separate their work queues so a slow catalog or renderer cannot starve interactive requests.

Proposed initial benchmark targets, to validate rather than advertise as achieved:

| Measurement | Target |
|---|---|
| Warm completion, server-side p95 | Under 20 ms |
| Completion while database refresh stalls | Same cached completion path; no database wait |
| Rendering/lint debounce | Start around 250 ms; configurable |
| Results from obsolete document/configuration/connection versions | Never published as current |
| Unavailable database, linter, or renderer | Server remains usable with clear capability status |

Establish a reproducible workload and reference machine: a 2,000-line active document and a catalog of 1,000 tables with 20 columns each, followed by a larger stress fixture. Measure cold startup, refresh time, end-to-end editor latency, CPU, and memory separately. Set additional budgets from measurements. Use bounded caches, capped results, cancellation, worker restart, and deadlines.

**9. Implementation milestones and acceptance gates**

| Phase | Work | Acceptance gate |
|---|---|---|
| 0 — Compatibility spike | Evaluate parser recovery, tool configuration APIs, renderer mappings, Jinja compatibility, and Dadbod connection hooks | Record dependency choices and known gaps using small executable fixtures |
| 1 — Rust LSP and Neovim foundation | Stdio lifecycle, document synchronization, custom language IDs, cancellation, status, basic offline completion | Both custom filetypes attach; incomplete SQL and Unicode edits remain responsive; Tree-sitter behavior is preserved |
| 2 — Configuration and tool adapters | Provider detection, inheritance, file watchers, style-aware completion, optional lint/format delegation | Representative configs resolve like the chosen tool; ambiguous providers and missing binaries have predictable behavior |
| 3 — Database and Dadbod completion | Buffer profiles, metadata index, async refresh, qualification, connection isolation, local file-lock handling | Real DuckDB tables/columns complete; changing a buffer connection switches its catalog; Dadbod execution is not blocked by an idle LSP reader |
| 4 — Jinja analysis | Rendering worker, context completion, include invalidation, source maps, render-error fallback | Configured variables complete; successful renders improve SQL analysis; loops/includes map correctly; missing variables preserve catalog completion |
| 5 — DuckLake integration | Read-only profiles, current schema, snapshots, qualified names, remote refresh behavior | Test current-schema and snapshot suggestions against local and PostgreSQL-backed metadata fixtures; document backend differences |
| 6 — Rich language features | Hover, signatures, definitions, references, scoped rename, semantic diagnostics, applicable fixes | Scope/shadowing tests pass; edits target original source; generated or ambiguous symbols are not unsafely renamed |
| 7 — Release hardening | Benchmarks, headless Neovim tests, integration matrix, packaging, setup documentation | Targets measured on the published workload; required integration jobs pass; release binaries and dependency requirements documented |

Phases 0–3 form the first usable plain-SQL milestone. Phases 4–5 deliver the complete agreed Jinja and DuckLake workflow. Phases 6–7 broaden language coverage and establish a release-quality baseline. These are dependency milestones, not calendar estimates.

**10. Validation matrix**

| Area | Required scenarios |
|---|---|
| Modes | `duckdbsql`, `jinjaduckdbsql`, optional generic SQL, filetype changes, conflicting config |
| Configuration | sqruff only, SQLFluff only, both, neither, nested overrides, TOML, missing executable, changes during analysis |
| SQL | Incomplete statements, multi-statement buffers, aliases, nested CTEs, quoted names, comments, Unicode, DuckDB-specific constructs |
| Jinja | Expressions, blocks, missing variables, configured macros, includes, loops, whitespace trimming, stale renders, worker timeout |
| Database | Disconnected startup, live schema, schema changes, connection switching, large catalog, duplicate names, lock conflict |
| DuckLake | Local and PostgreSQL metadata, read-only access, qualified tables, snapshots, dropped/added columns, remote failures |
| Editor | Headless Neovim attach, selected completion frontend, Dadbod execution, lint ownership, unchanged highlighting |

Use unit tests for scope/configuration/source-map behavior, protocol tests for lifecycle and cancellation, real database integration tests for metadata and locks, and headless Neovim tests for the Lua bridge. Required release integration jobs must run their dependencies explicitly rather than silently skipping DuckLake coverage.

**11. Choices still to resolve**

The architecture can proceed without another round of broad design questions. Phase 0 should settle the concrete parser and LSP libraries, renderer/source-map adapter, supported sqruff/SQLFluff versions, and whether Python is an optional requirement for full Jinja compatibility.

Before database integration acceptance, collect the user's actual connection setup: local DuckDB usage, DuckLake metadata backend, attachment bootstrap needs, Neovim version, and completion frontend. These affect compatibility fixtures and connection lifecycle, not the core separation of responsibilities.

Persistent on-disk schema caching, full dbt support, cross-file SQL project indexing, historical-schema completion, and optional semantic tokens remain later extensions. Syntax highlighting and query execution already have designated owners.
