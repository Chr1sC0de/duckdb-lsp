# Development baseline

This supplement incorporates the dotfiles review and the decision to develop
with fixtures. It updates the environment questions in the original plan.

## Verified editor configuration

Reviewed `Chr1sC0de/dotfiles` at commit
`8f2efe463b1e7489167051f8a2cefc421e52707d`.

| Component | Baseline |
|---|---|
| Neovim | Native `vim.lsp.config` / `vim.lsp.enable`; exact installed version is unknown |
| Completion | Blink.cmp, using its LSP source and signature support |
| Database tools | Dadbod, Dadbod UI, and Dadbod Completion |
| Formatter | Conform with sqruff for `sql` and `duckdbsql` |
| Highlighting | Custom `Chr1sC0de/duckdbsql-grammar` via Tree-sitter Manager; Jinja parsers and injection directive |
| Connection | A configured `duckdb:` profile; no verified persistent database or DuckLake backend |

Source: [dotfiles at the reviewed commit](https://github.com/Chr1sC0de/dotfiles/tree/8f2efe463b1e7489167051f8a2cefc421e52707d/config/nvim).

## Later dotfiles integration

- Map Jinja DuckDB filenames to `jinjaduckdbsql`; currently their configured
  mappings use `duckdbsql`.
- Extend SQL query-buffer keybindings beyond `sql` to both custom filetypes.
- Feed database suggestions through Blink's LSP source. Its Dadbod provider is
  declared but absent from its configured default source list.
- Keep Conform as the initial formatter owner. Correct its extra
  `formatters.formatters` nesting and duplicate format-on-save paths.
- Preserve existing Tree-sitter highlighting and plugin pins.
- Add and test the Neovim bridge in a separate dotfiles PR after the server's
  integration interface is working.

These are identified integration tasks, not changes already made to dotfiles.

## Test environments

| Layer | Approach |
|---|---|
| Configuration | sqruff and SQLFluff fixtures, nested overrides, ambiguous providers |
| Templates | Jinja variables, macros, includes, loops, missing context, failed renders |
| Completion | Mock catalogs for repeatable scope, ranking, and latency tests |
| DuckDB | Real temporary databases for metadata, binding, and lock behavior |
| DuckLake | Disposable local catalogs, followed by PostgreSQL-backed integration fixtures |
| Neovim | Headless tests using the reviewed plugin pins and test profiles |
| Failure handling | Simulated delays, disconnects, stale metadata, cancellation, and connection switching |

Mocks do not replace real database or editor integration tests. The user's live
setup is a final acceptance environment, not a development prerequisite.

## Remaining implementation decisions

Resolve parser and renderer dependencies during the compatibility spike. Verify
the fidelity of Jinja source maps before implementing edits against rendered
SQL. The optional Python-helper requirement and supported tool versions should
be documented explicitly after that spike.
