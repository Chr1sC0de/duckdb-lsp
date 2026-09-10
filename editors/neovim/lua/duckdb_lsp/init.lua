local M = {}
local options = {}
local selected, last, languages = {}, {}, {}

local function clients(buf)
  return vim.lsp.get_clients({ bufnr = buf, name = "duckdb_lsp" })
end

local function request(buf, method, params, callback)
  for _, client in ipairs(clients(buf)) do
    client:request(method, params, function(err, result)
      if err then
        vim.notify("duckdb-lsp: " .. err.message, vim.log.levels.WARN)
      elseif callback then
        callback(result)
      end
    end, buf)
  end
end

-- Dadbod's public resolver handles b:, w:, t:, g:db and named aliases.
-- The adapter exposes the file used for an empty duckdb: session as well.
function M.dadbod_profile(buf)
  local ok, url = pcall(vim.api.nvim_buf_call, buf, function()
    return vim.fn["db#resolve"]("")
  end)
  if not ok or type(url) ~= "string" or not url:match("^duckdb:") then
    return nil
  end
  local resolved, info = pcall(vim.fn["db#adapter#duckdb#dbext"], url)
  if not resolved or type(info) ~= "table" or type(info.dbname) ~= "string" then return nil end
  local path = info.dbname
  if path == ":memory:" then return {} end
  return { path = vim.fn.fnamemodify(vim.fn.expand(path), ":p"), alias = "db" }
end

function M.sync(buf, force)
  buf = buf or vim.api.nvim_get_current_buf()
  if not vim.api.nvim_buf_is_valid(buf) or #clients(buf) == 0 then return end
  local profile = selected[buf]
  if profile == nil and options.dadbod ~= false then profile = M.dadbod_profile(buf) end
  if profile == nil then
    if last[buf] == nil then return end
    profile = (options.settings or {}).defaultConnection or {}
  end
  local key = vim.json.encode(profile)
  if not force and last[buf] == key then return end
  last[buf] = key
  request(buf, "duckdb/setConnection", { uri = vim.uri_from_bufnr(buf), profile = profile })
end

function M.set_connection(profile, buf)
  buf = buf or vim.api.nvim_get_current_buf()
  if type(profile) == "string" then
    profile = assert((options.profiles or {})[profile], "Unknown DuckDB profile")
  end
  selected[buf] = profile or {}
  M.sync(buf, true)
end

function M.refresh(buf)
  buf = buf or vim.api.nvim_get_current_buf()
  request(buf, "workspace/executeCommand", {
    command = "duckdb.refreshCatalog", arguments = { { uri = vim.uri_from_bufnr(buf) } },
  })
end

function M.setup(opts)
  options = opts or {}
  assert(vim.fn.has("nvim-0.11") == 1, "duckdb-lsp requires Neovim 0.11+")
  vim.filetype.add({
    extension = { duckdbsql = "duckdbsql" },
    pattern = {
      [".*%.duckdb%.sql"] = "duckdbsql",
      [".*%.duckdbsql%.j2"] = "jinjaduckdbsql",
      [".*%.duckdbsql%.jinja"] = "jinjaduckdbsql",
      [".*%.duckdb%.sql%.j2"] = "jinjaduckdbsql",
      [".*%.duckdb%.sql%.jinja"] = "jinjaduckdbsql",
    },
  })
  local settings = options.settings or {}
  vim.lsp.config("duckdb_lsp", {
    cmd = options.cmd or { "duckdb-lsp" },
    filetypes = options.filetypes or { "duckdbsql", "jinjaduckdbsql" },
    root_markers = { ".sqruff", ".sqruff.ini", ".sqlfluff", "sqruff.toml", "pyproject.toml", ".git" },
    workspace_required = false,
    capabilities = options.capabilities,
    init_options = settings,
    settings = { duckdbLsp = settings },
    on_attach = function(client, buf)
      last[buf] = nil
      languages[buf] = vim.bo[buf].filetype
      M.sync(buf)
      if options.on_attach then options.on_attach(client, buf) end
    end,
  })
  local group = vim.api.nvim_create_augroup("DuckDBLsp", { clear = true })
  vim.api.nvim_create_autocmd("FileType", { group = group, callback = function(e)
    local language = vim.bo[e.buf].filetype
    if languages[e.buf] == nil or languages[e.buf] == language then return end
    languages[e.buf] = language
    local attached = clients(e.buf)
    for _, client in ipairs(attached) do vim.lsp.buf_detach_client(e.buf, client.id) end
    if not vim.tbl_contains(options.filetypes or { "duckdbsql", "jinjaduckdbsql" }, language) then return end
    vim.schedule(function()
      if not vim.api.nvim_buf_is_valid(e.buf) then return end
      for _, client in ipairs(attached) do
        if not client:is_stopped() then vim.lsp.buf_attach_client(e.buf, client.id) end
      end
    end)
  end })
  vim.api.nvim_create_autocmd("BufEnter", { group = group, callback = function(e) M.sync(e.buf) end })
  vim.api.nvim_create_autocmd("BufWipeout", { group = group, callback = function(e)
    selected[e.buf], last[e.buf], languages[e.buf] = nil, nil, nil
  end })
  vim.api.nvim_create_autocmd("User", { group = group, pattern = "*/DBExecutePost", callback = function()
    -- The event names the output file, not necessarily the source buffer.
    for _, client in ipairs(vim.lsp.get_clients({ name = "duckdb_lsp" })) do
      for buf in pairs(client.attached_buffers) do M.refresh(buf) end
    end
  end })
  vim.api.nvim_create_autocmd("BufReadCmd", { group = group, pattern = "duckdb-lsp://*", callback = function(e)
    local uri = e.file
    local client = vim.lsp.get_clients({ name = "duckdb_lsp" })[1]
    if not client then return end
    client:request("duckdb/catalogDocument", { uri = uri }, function(err, result)
      if not vim.api.nvim_buf_is_valid(e.buf) then return end
      local content = err and ("-- " .. err.message) or result.text
      vim.bo[e.buf].modifiable = true
      vim.api.nvim_buf_set_lines(e.buf, 0, -1, false, vim.split(content, "\n", { plain = true }))
      vim.bo[e.buf].buftype = "nofile"
      vim.bo[e.buf].bufhidden = "wipe"
      vim.bo[e.buf].swapfile = false
      vim.bo[e.buf].filetype = "duckdbsql"
      vim.bo[e.buf].modified = false
      vim.bo[e.buf].modifiable = false
    end)
  end })
  vim.api.nvim_create_user_command("DuckDBLspStatus", function()
    request(0, "duckdb/status", { uri = vim.uri_from_bufnr(0) }, function(value)
      vim.notify(vim.inspect(value), vim.log.levels.INFO)
    end)
  end, {})
  vim.api.nvim_create_user_command("DuckDBLspRefresh", function() M.refresh() end, {})
  vim.api.nvim_create_user_command("DuckDBLspConnection", function(e) M.set_connection(e.args) end,
    { nargs = 1, complete = function() return vim.tbl_keys(options.profiles or {}) end })
  vim.api.nvim_create_user_command("DuckDBLspDadbod", function()
    local buf = vim.api.nvim_get_current_buf()
    selected[buf], last[buf] = nil, nil
    M.sync(buf, true)
  end, {})
  vim.api.nvim_create_user_command("DuckDBLspConfig", function()
    request(0, "workspace/executeCommand", {
      command = "duckdb.explainConfiguration", arguments = { { uri = vim.uri_from_bufnr(0) } },
    }, function(value) vim.notify(vim.inspect(value), vim.log.levels.INFO) end)
  end, {})
  vim.lsp.enable("duckdb_lsp")
end

return M
