"""Black-box JSON-RPC tests against the compiled stdio server."""
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import threading
import time

import pytest

ROOT = Path(__file__).parents[1]
BIN = os.environ.get("DUCKDB_LSP_BIN", str(ROOT / "target/debug/duckdb-lsp"))


class Lsp:
    def __init__(self, root, **settings):
        self.process = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.messages = queue.Queue()
        self.notifications = []
        self.seq = 0
        self.lock = threading.Lock()
        self.root = root
        threading.Thread(target=self.reader, daemon=True).start()
        self.capabilities = self.request("initialize", {"rootUri": root.as_uri(), "capabilities": {}, "initializationOptions": {
            "includeUserConfig": False, "diagnosticsDelayMs": 20, "catalogTtlSeconds": 0,
            "workerTimeoutMs": 3000, **settings}})["capabilities"]
        self.notify("initialized", {})

    def reader(self):
        try:
            while True:
                headers = {}
                while (line := self.process.stdout.readline()) not in (b"\r\n", b"\n", b""):
                    k, v = line.decode().split(":", 1)
                    headers[k.lower()] = v.strip()
                if not headers: return
                value = json.loads(self.process.stdout.read(int(headers["content-length"])))
                if "method" in value and "id" in value:
                    self.send({"jsonrpc": "2.0", "id": value["id"], "result": None})
                else: self.messages.put(value)
        except (OSError, ValueError): pass

    def send(self, payload):
        data = json.dumps(payload).encode()
        with self.lock:
            self.process.stdin.write(f"Content-Length: {len(data)}\r\n\r\n".encode() + data)
            self.process.stdin.flush()

    def notify(self, method, params): self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method, params=None, timeout=8):
        self.seq += 1
        self.send({"jsonrpc": "2.0", "id": self.seq, "method": method, **({"params": params} if method != "shutdown" else {})})
        until = time.monotonic() + timeout
        while True:
            value = self.messages.get(timeout=max(.01, until - time.monotonic()))
            if value.get("id") == self.seq:
                assert "error" not in value, value
                return value.get("result")
            self.notifications.append(value)

    def open(self, text, language="duckdbsql", name="test.duckdbsql"):
        uri = (self.root / name).as_uri()
        self.notify("textDocument/didOpen", {"textDocument": {"uri": uri, "languageId": language, "version": 1, "text": text}})
        self.request("duckdb/status", {"uri": uri})
        return uri

    def at(self, uri, line=0, character=0):
        return {"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}

    def change(self, uri, text, version=2):
        self.notify("textDocument/didChange", {"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]})

    def status(self, uri): return self.request("duckdb/status", {"uri": uri})["documents"][0]

    def wait(self, predicate):
        until = time.monotonic() + 8
        while time.monotonic() < until:
            if predicate(): return
            time.sleep(.02)
        raise AssertionError("Timed out waiting for LSP state")

    def close(self):
        try:
            self.request("shutdown")
            self.send({"jsonrpc": "2.0", "method": "exit"})
            self.process.wait(timeout=5)
        finally:
            self.process.kill()
            self.process.communicate()


@pytest.fixture
def lsp(tmp_path):
    client = Lsp(tmp_path)
    yield client
    client.close()


def mock(name="people", column="id"):
    return {"mockCatalog": {"defaultCatalog": "db", "defaultSchema": "main", "tables": [
        {"catalog": "db", "schema": "main", "name": name, "columns": [{"name": column, "type": "INTEGER"}]}]}}


def connect(lsp, uri, profile):
    lsp.request("duckdb/setConnection", {"uri": uri, "profile": profile})
    lsp.wait(lambda: lsp.status(uri)["catalogTables"] > 0 and not lsp.status(uri)["catalogRefreshing"])


def labels(lsp, uri, line, col):
    return [i["label"] for i in lsp.request("textDocument/completion", lsp.at(uri, line, col))["items"]]


def test_offline_lifecycle_completion_and_unicode_edits(lsp):
    assert "semanticTokensProvider" not in lsp.capabilities
    uri = lsp.open("-- 🦆\nsel")
    assert "SELECT" in labels(lsp, uri, 1, 3)
    lsp.notify("textDocument/didChange", {"textDocument": {"uri": uri, "version": 2}, "contentChanges": [
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 5}}, "text": "duck"}]})
    assert "SELECT" in labels(lsp, uri, 1, 3)
    lsp.notify("textDocument/didClose", {"textDocument": {"uri": uri}})
    assert not lsp.request("duckdb/status")["documents"]


def test_catalog_completion_hover_definition_and_isolation(lsp):
    uri = lsp.open("SELECT p. FROM people p")
    connect(lsp, uri, mock())
    assert labels(lsp, uri, 0, 9) == ["id"]
    other = lsp.open("SELECT p. FROM people p", name="other.duckdbsql")
    connect(lsp, other, mock(column="different"))
    assert labels(lsp, other, 0, 9) == ["different"]
    assert labels(lsp, uri, 0, 9) == ["id"]
    location = lsp.request("textDocument/definition", lsp.at(uri, 0, 17))
    content = lsp.request("duckdb/catalogDocument", {"uri": location["uri"]})
    assert '"id" INTEGER' in content["text"]
    lsp.change(uri, "SELECT p.id FROM people p")
    assert "INTEGER" in lsp.request("textDocument/hover", lsp.at(uri, 0, 10))["contents"]["value"]


def test_scope_navigation_rename_and_signature(lsp):
    text = "WITH d AS (SELECT 1 AS id) SELECT a.id FROM d a WHERE EXISTS (SELECT a.id FROM d a)"
    uri = lsp.open(text)
    at = lsp.at(uri, 0, text.index("a.id"))
    refs = lsp.request("textDocument/references", {**at, "context": {"includeDeclaration": True}})
    assert len(refs) == 2
    rename = lsp.request("textDocument/rename", {**at, "newName": "outer_alias"})
    assert len(rename["documentChanges"][0]["edits"]) == 2
    assert rename["documentChanges"][0]["textDocument"]["version"] == 1
    assert len(lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": uri}})) == 3
    lsp.change(uri, "select coalesce(1, ")
    sig = lsp.request("textDocument/signatureHelp", lsp.at(uri, 0, 19))
    assert sig["signatures"]


def test_jinja_render_fallback_and_config_completion(lsp, tmp_path):
    (tmp_path / ".sqlfluff").write_text("[sqlfluff]\ndialect=duckdb\n[sqlfluff:templater:jinja:context]\nrelation=people\n")
    uri = lsp.open("SELECT p.id FROM {{ relation }} p", "jinjaduckdbsql")
    connect(lsp, uri, mock())
    lsp.wait(lambda: lsp.status(uri)["rendered"])
    assert "id" in labels(lsp, uri, 0, 9)
    lsp.change(uri, "SELECT p. FROM {{ missing }} p")
    assert "SELECT" in labels(lsp, uri, 0, 3)
    lsp.change(uri, "SELECT {{ rel", 3)
    assert "relation" in labels(lsp, uri, 0, 13)
    assert lsp.request("textDocument/prepareRename", lsp.at(uri, 0, 10)) is None


def test_native_diagnostics_are_current(lsp):
    uri = lsp.open("SELECT bad FROM people")
    connect(lsp, uri, mock())
    lsp.wait(lambda: (lsp.status(uri), any(n.get("method") == "textDocument/publishDiagnostics" and n["params"].get("diagnostics") for n in lsp.notifications))[1])
    for version in range(2, 20): lsp.change(uri, f"SELECT {version} AS good", version)
    lsp.wait(lambda: (lsp.status(uri), any(n.get("method") == "textDocument/publishDiagnostics" and n["params"].get("version") == 19 for n in lsp.notifications))[1])
    final = [n["params"] for n in lsp.notifications if n.get("method") == "textDocument/publishDiagnostics"][-1]
    assert final["version"] == 19 and not final["diagnostics"]


def test_missing_worker_preserves_completion(tmp_path):
    client = Lsp(tmp_path, python="/does/not/exist/python")
    try:
        uri = client.open("sel")
        client.wait(lambda: client.status(uri)["workerError"])
        assert "SELECT" in labels(client, uri, 0, 3)
    finally: client.close()


def test_slow_catalog_times_out_without_blocking_completion(tmp_path):
    worker = tmp_path / "slow-worker"
    worker.write_text(f"#!{sys.executable}\n" + """
import json, sys, time
for line in sys.stdin:
    req = json.loads(line)
    if req['method'] == 'catalog':
        time.sleep(3)
        result = {'tables': []}
    else:
        result = {'native': True, 'diagnostics': []}
    print(json.dumps({'result': result}), flush=True)
""")
    worker.chmod(0o755)
    client = Lsp(tmp_path, python=str(worker), workerTimeoutMs=500)
    try:
        uri = client.open("sel")
        client.wait(lambda: client.status(uri)["catalogRefreshing"])
        start = time.monotonic()
        assert "SELECT" in labels(client, uri, 0, 3)
        assert time.monotonic() - start < .5
        client.wait(lambda: client.status(uri)["catalogError"])
        assert "SELECT" in labels(client, uri, 0, 3)
    finally: client.close()
