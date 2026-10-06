import importlib.util
import os
from pathlib import Path

import duckdb
import pytest

spec = importlib.util.spec_from_file_location("worker", Path(__file__).parents[1] / "workers/worker.py")
w = importlib.util.module_from_spec(spec)
spec.loader.exec_module(w)


def params(tmp_path, text, **kwargs):
    return {"text": text, "path": str(tmp_path / "query.sql"), "root": str(tmp_path),
            "config": {"provider": "sqlfluff", "values": {}}, **kwargs}


def test_metadata_is_readonly_and_releases_lock(tmp_path):
    path = tmp_path / "db.duckdb"
    with duckdb.connect(str(path)) as c:
        c.execute('CREATE TABLE people (id INTEGER, "full name" VARCHAR)')
    cat = w.catalog({"root": str(tmp_path), "profile": {"path": str(path)}})
    table = next(t for t in cat["tables"] if t["name"] == "people")
    assert [c["name"] for c in table["columns"]] == ["id", "full name"]
    connection, _ = w.connect({"path": str(path)}, str(tmp_path))
    with pytest.raises(duckdb.Error):
        connection.execute("CREATE TABLE db.main.unsafe (id INTEGER)")
    connection.close()
    with duckdb.connect(str(path)) as c:
        c.execute("INSERT INTO people VALUES (1, 'Duck')")
    assert not list(tmp_path.glob("*.wal"))


def test_native_recovery_and_schema_only_validation(tmp_path):
    path = tmp_path / "source.duckdb"
    with duckdb.connect(str(path)) as c:
        c.execute("CREATE TABLE people(id INTEGER)")
    cat = w.catalog({"profile": {"path": str(path)}})
    query = f"DROP TABLE people; COPY people TO '{tmp_path / 'escape.csv'}'; SELECT missing FROM people; SELECT FROM; SELECT 1"
    errors = w.native_diagnostics(query, cat)
    assert any("missing" in e["message"] for e in errors)
    assert any("syntax" in e["message"].lower() for e in errors)
    assert not (tmp_path / "escape.csv").exists()
    with duckdb.connect(str(path), read_only=True) as c:
        assert c.execute("SELECT * FROM people").description[0][0] == "id"


def test_native_ddl_and_external_files(tmp_path):
    assert not w.native_diagnostics("CREATE TABLE t(id INTEGER); SELECT id FROM t; SELECT 1::INT AS n QUALIFY row_number() over() = 1", {})
    assert not w.native_diagnostics("SELECT * FROM read_csv('/not/a/real/file.csv')", {})


def test_jinja_context_macros_and_utf16_mapping(tmp_path):
    p = params(tmp_path, "-- 🦆\nselect {{ amount }} as total, bad", render=True)
    p["config"]["values"] = {"templater.jinja.context.amount": 42}
    result = w.analyze(p)
    assert result["rendered"] == "-- 🦆\nselect 42 as total, bad"
    assert result["diagnostics"][0]["range"]["start"] == {"line": 1, "character": 30}
    assert all(m["sourceStart"] <= m["sourceEnd"] for m in result["mappings"])


def test_undefined_template_has_no_authoritative_sql(tmp_path):
    result = w.analyze(params(tmp_path, "select * from {{ missing }}", render=True))
    assert result["rendered"] is None
    assert result["diagnostics"]


def test_includes_loops_and_whitespace(tmp_path):
    (tmp_path / "part.sql").write_text("1 AS id")
    p = params(tmp_path, "SELECT {% include 'part.sql' %}{% for x in [2,3] %}, {{x}} AS n{{x}}{% endfor %}", render=True)
    p["config"]["values"]["templater.jinja.loader_search_path"] = str(tmp_path)
    result = w.analyze(p)
    assert result["rendered"] == "SELECT 1 AS id, 2 AS n2, 3 AS n3"
    assert not result["diagnostics"]
    assert len(result["mappings"]) > 3


def test_sqlfluff_format_preserves_template(tmp_path):
    p = params(tmp_path, "select  {{x}} as foo", render=True)
    p["config"]["values"] = {"templater.jinja.context.x": 1, "rules.capitalisation.keywords.capitalisation_policy": "upper"}
    fixed = w.format_sql(p)["text"]
    assert "{{x}}" in fixed or "{{ x }}" in fixed
    assert "SELECT" in fixed


def test_sqruff_real_cli(tmp_path):
    (tmp_path / ".sqruff").write_text("[sqruff]\ndialect=duckdb\n[sqruff:rules:capitalisation.keywords]\ncapitalisation_policy=upper\n")
    p = params(tmp_path, "select  1 as foo")
    p["config"]["provider"] = "sqruff"
    p["config"]["values"]["rules.capitalisation.keywords.capitalisation_policy"] = "upper"
    assert w.lint(p)
    assert "SELECT" in w.format_sql(p)["text"]


def test_environment_profiles_do_not_expand_arbitrary_code(monkeypatch):
    monkeypatch.setenv("DUCKDB_TEST_PATH", "demo.duckdb")
    assert w.expand("${env:DUCKDB_TEST_PATH}") == "demo.duckdb"
    assert w.expand("$(not_a_shell)") == "$(not_a_shell)"
    with pytest.raises(ValueError): w.expand("${env:UNDEFINED_DUCKDB_TEST_ENV}")
