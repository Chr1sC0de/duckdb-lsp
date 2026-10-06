# Validation

Development baseline: Rust 1.90.0, Python 3.12, DuckDB 1.5.5, SQLFluff 4.3.0, Jinja2 3.1.6, sqlglot 30.18.0, sqruff 0.40.0, Neovim 0.11.5, Linux x86-64.

Rust tests cover UTF-16 positions, recovery lexing, scopes/shadowing, local declarations, CTE wildcard columns, ambiguous hover, config inheritance/ambiguity, TOML context objects, multiline macros, and snippets. Python tests use real temporary DuckDB files and real sqruff/SQLFluff execution. Protocol tests launch the compiled server and exercise lifecycle, incremental edits, profiles, definitions, rendering fallback, navigation, current-version diagnostics, and missing workers.

The headless editor test uses Dadbod revision `6d1d41da4873a445c5605f2005ad2c68c99d8770`, pinned in the user's dotfiles. It verifies both custom filetypes attach, standard LSP completion, Jinja rendering, Dadbod session/file paths, and absence of semantic tokens. Blink consumes the same LSP response; its popup UI and the complete personal dotfiles environment are not reproduced by this test.

**Completion benchmark**

`cargo run --release --example benchmark` creates a 2,000-line document with a 1,000-table catalog and 20 columns per table. After 20 warmup requests, it samples 200 qualified-column completions. A local release run measured:

| Measurement | Result |
|---|---:|
| Initial scope analysis | 3.02 ms |
| Median warm completion | 0.69 ms |
| p95 warm completion | 0.93 ms |

These measure the Rust feature function, excluding IPC, Neovim display, catalog refresh, and rendering. They are a reproducible workload, not a guarantee for every document or machine. CI prints its own result.

**External integration gate**

The DuckLake CI job starts PostgreSQL and installs matching DuckDB extensions. It tests local and PostgreSQL metadata, snapshots, schema changes, read-only access, hidden metadata catalogs, and historical schema selection. Missing dependencies fail the job rather than skipping it.

This development container rejected DuckDB extension downloads and has no PostgreSQL installation, so real DuckLake integration could not run locally. Check the PR's DuckLake CI result before treating that matrix as validated. Fixtures require no personal or production credentials.

The user's full dotfiles setup, custom Tree-sitter injections, remote object-store credentials, large remote catalogs, macOS, and Windows need additional compatibility coverage before a cross-platform release claim.
