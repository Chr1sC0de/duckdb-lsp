# duckdb-lsp

A planned Rust language server for DuckDB and DuckLake, with Neovim integration,
database-aware completion, sqruff/SQLFluff configuration, and Jinja support.

**Status:** design and development setup. A runnable language server is not yet
included. See the [implementation plan](docs/implementation-plan.md) and
[development baseline](docs/development-baseline.md).

## Intended workflow

- `duckdbsql`: DuckDB SQL analysis without template rendering.
- `jinjaduckdbsql`: Jinja context completion and background rendering with source mappings.
- Cached database metadata supplements SQL scope and built-in completion.
- sqruff or SQLFluff settings inform style and supported template context.
- Neovim's existing Tree-sitter configuration owns highlighting.
- Dadbod owns query execution; the LSP reads configured catalog metadata.

## Integration target

Rust server over stdio, a small Neovim Lua bridge, Blink.cmp, Dadbod UI, and
Conform with sqruff. SQLFluff compatibility is also planned. The core server
should work with standard LSP clients; a Python helper may be needed for full
SQLFluff/Jinja compatibility.

## Development sequence

1. Validate parser, configuration-provider, and template-renderer interfaces.
2. Implement the Rust LSP and Neovim filetype integration.
3. Add configuration adapters and cached DuckDB/Dadbod completion.
4. Add Jinja rendering and DuckLake catalog/snapshot support.
5. Add richer navigation, diagnostics, benchmarks, and release packaging.

Development uses mock catalogs and declarative fixtures for deterministic tests,
plus disposable real DuckDB and DuckLake instances for integration coverage.
No personal database credentials are required to begin.

## Proposed layout

| Path | Purpose |
|---|---|
| `crates/` | Rust server and analysis components |
| `editors/neovim/` | Lua integration |
| `tests/fixtures/` | SQL, Jinja, configuration, and mock catalog fixtures |
| `tests/integration/` | Database, protocol, and headless Neovim checks |
| `docs/` | Architecture, compatibility decisions, and setup documentation |

Only the documentation is present in this initial repository seed. Source
directories and CI will be introduced with their first executable milestones.
