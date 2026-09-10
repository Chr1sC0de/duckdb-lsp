local root = assert(vim.env.DUCKDB_LSP_ROOT)
vim.cmd("filetype plugin indent on")
vim.opt.runtimepath:prepend(root .. "/editors/neovim")
if vim.env.DADBOD_PATH then
  vim.opt.runtimepath:prepend(vim.env.DADBOD_PATH)
  vim.cmd("runtime plugin/dadbod.vim")
end
local bridge = require("duckdb_lsp")
bridge.setup({
  cmd = { vim.env.DUCKDB_LSP_BIN or (root .. "/target/debug/duckdb-lsp") },
  dadbod = false,
  settings = { includeUserConfig = false, catalogTtlSeconds = 0, diagnosticsDelayMs = 20 },
})
vim.cmd("edit " .. vim.fn.fnameescape(root .. "/test-data/editor.duckdbsql"))
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "SELECT p. FROM people p" })
assert(vim.bo.filetype == "duckdbsql", "raw filetype")
assert(vim.wait(10000, function() return #vim.lsp.get_clients({ name = "duckdb_lsp", bufnr = 0 }) > 0 end), "LSP did not attach")
local client = vim.lsp.get_clients({ name = "duckdb_lsp", bufnr = 0 })[1]
assert(not client.server_capabilities.semanticTokensProvider, "Tree-sitter owns highlighting")
local function request(method, params)
  local response = assert(client:request_sync(method, params, 5000, 0))
  assert(not response.err, vim.inspect(response.err))
  return response.result
end
bridge.set_connection({ mockCatalog = {
  defaultCatalog = "db", defaultSchema = "main",
  tables = { { catalog = "db", schema = "main", name = "people", columns = { { name = "id", type = "INTEGER" } } } },
} })
local uri = vim.uri_from_bufnr(0)
assert(vim.wait(5000, function()
  return request("duckdb/status", { uri = uri }).documents[1].catalogTables == 1
end), "catalog refresh")
local result = request("textDocument/completion", { textDocument = { uri = uri }, position = { line = 0, character = 9 } })
assert(result.items[1].label == "id", vim.inspect(result))
assert(result.items[1].textEdit.newText == '"id"')

if vim.env.DADBOD_PATH then
  vim.b.db = "duckdb:"
  local profile = assert(bridge.dadbod_profile(vim.api.nvim_get_current_buf()))
  assert(profile.path:match("%.duckdb$"), "Dadbod session file")
  assert(profile.path == vim.fn["db#adapter#duckdb#dbext"](vim.fn["db#resolve"]("")).dbname)
  vim.b.db = "duckdb:" .. root .. "/test-data/example.duckdb"
  assert(bridge.dadbod_profile(vim.api.nvim_get_current_buf()).path == root .. "/test-data/example.duckdb")
end

vim.api.nvim_buf_set_lines(0, 0, -1, false, { "SELECT {{ 1 + 2 }}" })
vim.bo.filetype = "jinjaduckdbsql"
assert(vim.wait(5000, function()
  local status = request("duckdb/status", { uri = uri }).documents[1]
  return status and status.languageId == "jinjaduckdbsql" and status.rendered
end), "Existing buffer must reopen with the new languageId")

vim.cmd("edit! " .. vim.fn.fnameescape(root .. "/test-data/template.duckdbsql.j2"))
assert(vim.bo.filetype == "jinjaduckdbsql", "Jinja filetype")
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "SELECT {{ 1 + 2 }}" })
assert(vim.wait(10000, function()
  return #vim.lsp.get_clients({ name = "duckdb_lsp", bufnr = 0 }) > 0
end), "Jinja LSP attachment")
client = vim.lsp.get_clients({ name = "duckdb_lsp", bufnr = 0 })[1]
uri = vim.uri_from_bufnr(0)
assert(vim.wait(5000, function() return request("duckdb/status", { uri = uri }).documents[1].rendered end), "Jinja rendering")
print("Neovim integration passed: raw/Jinja attachment, completion, Dadbod profile, highlighting ownership")
vim.lsp.stop_client(vim.lsp.get_clients(), true)
vim.cmd("qa!")
