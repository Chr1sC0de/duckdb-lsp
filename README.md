# duckdb-lsp

Rust language server for DuckDB SQL, DuckLake catalogs, and Jinja templates. Completion reads cached state; database refresh, rendering, and linting run in background workers. Tree-sitter owns highlighting. Dadbod owns query execution.

**Implemented features**

- Offline keywords, types, functions, signatures, and SQL snippets.
- Database catalogs, schemas, tables, views, columns, functions, and DuckLake snapshots.
- Query aliases, CTE projections, simple wildcard expansion, and explicit local table declarations.
- Hover, definitions, references, scoped alias/CTE rename, symbols, highlights, and folding.
- Native syntax and SELECT binding diagnostics using isolated, empty shadow tables.
- Jinja context completion, rendering, includes, macros, source mappings, and failure fallback.
- sqruff/SQLFluff config discovery, inheritance, inline directives, and optional linting/formatting.
- Neovim 0.11+ integration with Dadbod connections and standard LSP completion for Blink.

**Install**

Build with Rust 1.90+. Python 3.12 is the tested helper runtime. The Rust binary embeds the helper script and offline metadata; install Python dependencies separately.

```sh
git clone https://github.com/Chr1sC0de/duckdb-lsp.git
cd duckdb-lsp
cargo install --locked --path .
python3 -m venv .venv
.venv/bin/python -m pip install -r workers/requirements.txt
```

For sqruff, use your existing executable or install `workers/requirements-sqruff.txt`. SQLFluff supplies the Jinja renderer for both config providers. Offline completion and local navigation remain available without Python dependencies.

Tested workers: DuckDB 1.5.5, SQLFluff 4.3.0, Jinja2 3.1.6, sqlglot 30.18.0, sqruff 0.40.0. Offline function metadata comes from DuckDB 1.5.5.

**Neovim with lazy.nvim and Blink**

```lua
{
  "Chr1sC0de/duckdb-lsp",
  build = "cargo build --release --locked",
  config = function(plugin)
    vim.opt.runtimepath:append(plugin.dir .. "/editors/neovim")
    require("duckdb_lsp").setup({
      cmd = { plugin.dir .. "/target/release/duckdb-lsp" },
      capabilities = require("blink.cmp").get_lsp_capabilities(),
      settings = {
        python = vim.fn.expand("~/src/duckdb-lsp/.venv/bin/python"),
        configProvider = "auto",
        lint = false,
        format = false, -- Keep Conform/sqruff as the formatter.
      },
      profiles = {
        analytics = { path = "/absolute/path/analytics.duckdb" },
        lake = {
          attachments = {
            { type = "ducklake", alias = "lake", path = "${env:DUCKLAKE_METADATA}" },
          },
          defaultCatalog = "lake",
          defaultSchema = "main",
        },
      },
    })
  end,
}
```

Change the Python path to your worker environment. lazy.nvim builds Rust; install the Python packages once in that environment.

| Filetype | Behavior |
|---|---|
| `duckdbsql` | Raw DuckDB SQL; never render Jinja |
| `jinjaduckdbsql` | Background Jinja rendering, original editor ranges |
| `sql` | Optional through `filetypes`; use only for DuckDB projects |

The bridge maps `.duckdbsql` and `.duckdb.sql` to raw SQL, and their `.j2` / `.jinja` variants to Jinja SQL. Remove older conflicting template mappings. Keep your existing Tree-sitter parser and register `jinjaduckdbsql` with your Jinja parser/injection setup; this plugin does not install grammars or advertise semantic tokens.

Dadbod's public resolver and DuckDB adapter expose the selected file, including the temporary file behind `duckdb:`. Metadata becomes available after the first query creates it. `DBExecutePost` schedules refresh. Explicit profiles override Dadbod per buffer.

| Command | Purpose |
|---|---|
| `:DuckDBLspStatus` | Mode, config provider, render/catalog state, worker errors |
| `:DuckDBLspConnection analytics` | Select a named profile |
| `:DuckDBLspDadbod` | Resume following Dadbod |
| `:DuckDBLspRefresh` | Refresh metadata |
| `:DuckDBLspConfig` | Inspect effective settings and provenance |

Include both custom filetypes in any Dadbod query mappings currently restricted to `sql`. Blink uses its `lsp` source. Enabling Dadbod completion too can produce duplicates. Keep one owner of style linting and formatting; both default to disabled in this server.

**Configuration**

`configProvider` accepts `auto`, `sqruff`, `sqlfluff`, or `none`. Auto selects an unambiguous provider. Both providers, or shared `tool.sqlfluff` TOML alone, require an explicit choice. Settings resolve from user configuration and workspace root down to the file. Saves/watchers invalidate configuration; inline directives are reread during debounced analysis.

```ini
[sqruff]
dialect = duckdb

[sqruff:rules:capitalisation.keywords]
capitalisation_policy = upper

[sqruff:templater:jinja:context]
relation = people
limit = 100
```

Use `[sqlfluff]` / `[sqlfluff:...]` for SQLFluff, or `[tool.sqlfluff.core]` / `[tool.sqlfluff.templater.jinja.context]` in TOML. Templates can use `SELECT * FROM {{ relation }} LIMIT {{ limit }}`. Missing values produce diagnostics and preserve completion. Conflicting templater settings pause rendering. Macros/includes use SQLFluff's Jinja adapter; dbt execution is not included.

| Setting | Default |
|---|---|
| `python` | `python3` |
| `configProvider` | `auto` |
| `includeUserConfig` | `true` |
| `diagnosticsDelayMs` | `250` |
| `workerTimeoutMs` | `10000` |
| `catalogTtlSeconds` | `60`; `0` disables periodic refresh |
| `maxCompletions` | `200`, capped at `2000` |
| `nativeDiagnostics` | `true` |
| `lint`, `format` | `false`, `false` |
| `defaultConnection` | Unconnected in-memory catalog |

Settings are initialization options, optionally beneath `duckdbLsp`, and use the same structure for `workspace/didChangeConfiguration`. Restart when changing formatting ownership: its capability is selected at initialization.

**DuckLake and database access**

Profiles accept a DuckDB `path`, or `attachments` with `type`, `alias`, and `path`. DuckLake accepts local metadata files or `postgres:host=... dbname=...`, with an optional `ducklake:` prefix. Use `${env:VARIABLE}` for connection values. `defaultCatalog` / `defaultSchema` control unqualified names. An attachment's integer `snapshotVersion` selects a separately cached historical schema.

Install `ducklake` and the metadata extension (`postgres` or `sqlite`) using the same DuckDB version and user environment as the worker. The LSP loads installed extensions; it never installs extensions or creates lakes. Additional installed extension names can be listed in `extensions`.

Source attachments use `READ_ONLY`; DuckLake also uses `CREATE_IF_NOT_EXISTS false`. Connections close after refresh. Lock conflicts preserve cached metadata and appear in status. Buffer SQL never runs against source connections. Native diagnostics use empty shadow tables, disable external access, and roll back local declarations after each analysis.

`AT (VERSION => ...)` suggests cached snapshot IDs. Select a profile with `snapshotVersion` to verify historical columns; a time-travel clause alone does not fetch historical schemas. See [DuckLake connection options](https://ducklake.select/docs/stable/duckdb/usage/connecting).

**Current boundaries**

This is an initial implementation, not exhaustive coverage of every DuckDB grammar or extension. Scope inference is conservative around complex projections, lateral queries, and generated SQL. Rename supports raw SQL aliases/CTEs and rejects collisions; it never renames database objects. Workspace symbols cover open documents. Catalog definitions describe metadata, not migration files.

Native validation checks syntax and binds SELECT when schema is available. It does not reproduce source data, arbitrary extension functions, user-defined types, session state, or every DDL effect. Template edits stay on original source; ambiguous generated locations are excluded. Explicitly configured SQLFluff Python libraries execute as project code; the helper is not an operating-system security sandbox.

Documents are limited to 2 MiB; worker responses to 32 MiB. Refresh reads the full metadata catalog. Persistent disk caching, dbt, cross-file indexing, and automatic historical schema switching remain future work.

**Development**

```sh
cargo fmt --all -- --check
cargo test --locked
cargo build --locked
.venv/bin/python -m pip install -r workers/requirements-sqruff.txt pytest
.venv/bin/python -m pytest -q
cargo run --release --example benchmark
```

CI explicitly runs headless Neovim and real local/PostgreSQL DuckLake tests under `tests/integration`. See [validation](docs/validation.md), [protocol](docs/protocol.md), [implementation plan](docs/implementation-plan.md), and [dotfiles baseline](docs/development-baseline.md).
