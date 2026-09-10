"""Optional native SQL and template worker. UTF-8 JSON lines over stdio.

No user query is executed against a source database. Connections are opened only
for explicit metadata refresh and always closed before returning.
"""
import ast
import contextlib
import configparser
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from collections import defaultdict, OrderedDict


def quote(value):
    return '"' + value.replace('"', '""') + '"'


def literal(value):
    return "'" + value.replace("'", "''") + "'"


def expand(value):
    def substitute(match):
        if match[1] not in os.environ:
            raise ValueError("Required connection environment variable is missing")
        return os.environ[match[1]]
    return re.sub(r"\$\{env:([A-Za-z_][A-Za-z_0-9]*)\}", substitute, value)


def position(text, index):
    before = text[:index]
    return {"line": before.count("\n"), "character": len(before.rsplit("\n", 1)[-1].encode("utf-16-le")) // 2}


def diagnostic(text, start, end, message, source="DuckDB", severity=1, code=None):
    result = {"range": {"start": position(text, start), "end": position(text, end)},
              "message": message, "source": source, "severity": severity}
    if code:
        result["code"] = code
    return result


def byte_index(text, index):
    return len(text[:index].encode("utf-8"))


def provider_config(params):
    from sqlfluff.core import FluffConfig
    values = params.get("config", {}).get("values", {})
    nested = {}
    for key, value in values.items():
        # Rule names contain dots; SQLFluff represents the rule name as one key.
        if key.startswith("rules."):
            pieces = ["rules", key[len("rules."):].rsplit(".", 1)[0], key.rsplit(".", 1)[1]]
        else:
            pieces = key.split(".")
        current = nested
        for piece in pieces[:-1]:
            current = current.setdefault(piece, {})
        if key.startswith("templater.jinja.context.") and isinstance(value, str):
            try:
                value = ast.literal_eval(value)
            except (ValueError, SyntaxError):
                pass
        current[pieces[-1]] = value
    nested.setdefault("core", {})["dialect"] = "duckdb"
    nested["core"]["templater"] = "jinja" if params.get("render") else "raw"
    # Resolve template paths relative to their originating configuration file.
    jinja = nested.get("templater", {}).get("jinja", {})
    for key in ("load_macros_from_path", "loader_search_path", "library_path"):
        if key in jinja and isinstance(jinja[key], str):
            origin = params.get("config", {}).get("sources", {}).get("templater.jinja." + key)
            base = Path(origin).parent if origin and origin != "inline directive" else Path(params.get("root", "."))
            jinja[key] = ",".join(str((base / v.strip()).resolve()) for v in jinja[key].split(","))
    # Configured Python libraries are executed only by the isolated helper. Users
    # opt into such SQLFluff behavior through their own project configuration.
    return FluffConfig(configs=nested)


def render(params):
    from sqlfluff.core.templaters.jinja import JinjaTemplater
    text = params["text"]
    config = provider_config(params)
    templated, errors = JinjaTemplater().process(in_str=text, fname=params["path"], config=config)
    diagnostics = []
    for error in errors:
        line, column = error.line_no or 1, error.line_pos or 1
        index = sum(map(len, text.splitlines(keepends=True)[:line - 1])) + column - 1
        # Undefined values can be secrets; expose the failure location, not context values.
        diagnostics.append(diagnostic(text, min(index, len(text)), min(index + 1, len(text)),
                                      "Jinja rendering failed: " + error.desc(), "Jinja"))
    if templated is None or diagnostics:
        return {"diagnostics": diagnostics, "rendered": None, "mappings": []}
    mappings = []
    for item in templated.sliced_file:
        mappings.append({"kind": item.slice_type,
                         "sourceStart": byte_index(text, item.source_slice.start),
                         "sourceEnd": byte_index(text, item.source_slice.stop),
                         "targetStart": byte_index(templated.templated_str, item.templated_slice.start),
                         "targetEnd": byte_index(templated.templated_str, item.templated_slice.stop)})
    return {"diagnostics": [], "rendered": templated.templated_str, "mappings": mappings,
            "_templated": templated}


def connect(profile, root):
    import duckdb
    c = duckdb.connect(config={"autoinstall_known_extensions": False,
                                "autoload_known_extensions": False, "threads": 2})
    try:
        attachments = list(profile.get("attachments", []))
        if profile.get("path"):
            attachments.insert(0, {"path": profile["path"], "alias": profile.get("alias", "db"), "type": "duckdb"})
        seen = {"memory", "system", "temp"}
        for extension in profile.get("extensions", []):
            if not re.fullmatch(r"[a-z][a-z0-9_]*", extension):
                raise ValueError("Invalid extension name")
            c.execute("LOAD " + extension)
        for attachment in attachments:
            alias = attachment["alias"]
            if not alias or alias.lower() in seen:
                raise ValueError("Attachment aliases must be unique")
            seen.add(alias.lower())
            kind = attachment.get("type", "duckdb")
            if kind not in ("duckdb", "ducklake"):
                raise ValueError("Unsupported attachment type")
            path = expand(attachment["path"])
            options = "READ_ONLY"
            if kind == "ducklake":
                c.execute("LOAD ducklake")
                path = path.removeprefix("ducklake:")
                if path.startswith("postgres:"):
                    c.execute("LOAD postgres")
                elif path.startswith("sqlite:"):
                    c.execute("LOAD sqlite")
                else:
                    path = str((Path(root) / Path(path).expanduser()).resolve())
                path = "ducklake:" + path
                options += ", CREATE_IF_NOT_EXISTS false"
                if "snapshotVersion" in attachment:
                    options += ", SNAPSHOT_VERSION " + str(int(attachment["snapshotVersion"]))
            else:
                path = str((Path(root) / Path(path).expanduser()).resolve())
            c.execute(f"ATTACH {literal(path)} AS {quote(alias)} ({options})")
        return c, attachments
    except BaseException:
        c.close()
        raise


def rows(c, query):
    result = c.execute(query)
    names = [x[0] for x in result.description]
    return [dict(zip(names, row)) for row in result.fetchall()]


def catalog(params):
    import duckdb
    profile = params.get("profile") or {}
    if "mockCatalog" in profile:
        return profile["mockCatalog"]
    c, attachments = connect(profile, params.get("root", "."))
    try:
        hidden = {"__ducklake_metadata_" + a["alias"] for a in attachments if a.get("type") == "ducklake"}
        lakes = {a["alias"] for a in attachments if a.get("type") == "ducklake"}
        tables = rows(c, """SELECT table_catalog AS catalog, table_schema AS schema,
            table_name AS name, table_type AS kind FROM information_schema.tables
            WHERE table_schema NOT IN ('information_schema','pg_catalog') ORDER BY 1,2,3""")
        columns = defaultdict(list)
        for cat, schema, table, name, typ, comment in c.execute("""SELECT database_name,
                schema_name,table_name,column_name,data_type,comment FROM duckdb_columns()
                WHERE NOT internal ORDER BY column_index""").fetchall():
            columns[(cat, schema, table)].append({"name": name, "type": typ, "comment": comment or ""})
        tables = [t for t in tables if t["catalog"] not in hidden]
        for table in tables:
            table["columns"] = columns[(table["catalog"], table["schema"], table["name"])]
            table["ducklake"] = table["catalog"] in lakes
        functions = {}
        for name, kind, desc, arguments, types, ret in c.execute("""SELECT function_name,
            function_type,description,parameters,parameter_types,return_type FROM duckdb_functions()""").fetchall():
            f = functions.setdefault(name, {"name": name, "kind": kind, "description": desc or "", "parameters": arguments or [], "signatures": []})
            signature = name + "(" + ", ".join(p + ": " + (t or "ANY") for p, t in zip(arguments or [], types or [])) + ")" + (" → " + ret if ret else "")
            if signature not in f["signatures"] and len(f["signatures"]) < 8:
                f["signatures"].append(signature)
        snapshots = {name: rows(c, f"SELECT * FROM {quote(name)}.snapshots() ORDER BY snapshot_id DESC LIMIT 100") for name in lakes}
        default = profile.get("defaultCatalog", attachments[0]["alias"] if attachments else "memory")
        return {"tables": tables, "functions": list(functions.values()), "snapshots": snapshots,
                "defaultCatalog": default, "defaultSchema": profile.get("defaultSchema", "main"), "engineVersion": duckdb.__version__}
    finally:
        c.close()


def split_sql(sql):
    # Native extraction accepts all DuckDB syntax but aborts on the first syntax
    # error. This lexer lets diagnostics recover at statement boundaries.
    pattern = re.compile(r"(?:E)?'(?:''|\\.|[^'])*'|\"(?:\"\"|[^\"])*\"|\$(?P<tag>\w*)\$.*?\$(?P=tag)\$|--[^\n]*|/\*|;", re.S | re.I)
    start = 0
    i = 0
    while (match := pattern.search(sql, i)):
        i = match.end()
        if match.group() == "/*":
            depth = 1
            while depth and i < len(sql):
                nested = re.search(r"/\*|\*/", sql[i:])
                if not nested:
                    i = len(sql)
                    break
                depth += 1 if nested.group() == "/*" else -1
                i += nested.end()
        elif match.group() == ";":
            yield start, sql[start:match.start()]
            start = i
    if sql[start:].strip():
        yield start, sql[start:]


_SHADOWS = OrderedDict()


def shadow_catalog(catalog_data):
    import duckdb
    key = hashlib.sha256(json.dumps({k: catalog_data.get(k) for k in ("tables", "defaultCatalog", "defaultSchema")}, sort_keys=True).encode()).hexdigest()
    if key in _SHADOWS:
        _SHADOWS.move_to_end(key)
        return _SHADOWS[key]
    c = duckdb.connect(config={"autoinstall_known_extensions": False, "autoload_known_extensions": False,
                                "threads": 2, "memory_limit": "256MB", "max_temp_directory_size": "0B"})
    try:
        catalogs = {"memory"}
        omitted = set()
        for table in catalog_data.get("tables", []):
            name = table["catalog"]
            if name not in catalogs:
                c.execute(f"ATTACH ':memory:' AS {quote(name)}")
                catalogs.add(name)
            prefix = quote(name) + "." + quote(table["schema"])
            c.execute(f"CREATE SCHEMA IF NOT EXISTS {prefix}")
            fields = ", ".join(quote(col["name"]) + " " + col["type"] for col in table["columns"])
            try:
                c.execute(f"CREATE TABLE {prefix}.{quote(table['name'])} ({fields})")
            except duckdb.Error:
                omitted.add(table["name"].lower())
        default = catalog_data.get("defaultCatalog", "memory")
        if default in catalogs:
            try:
                c.execute(f"USE {quote(default)}.{quote(catalog_data.get('defaultSchema') or 'main')}")
            except duckdb.Error:
                pass
        c.execute("SET enable_external_access = false")
        c.execute("SET lock_configuration = true")
        _SHADOWS[key] = (c, omitted)
        while len(_SHADOWS) > 2:
            _, (old, _) = _SHADOWS.popitem(last=False)
            old.close()
        return c, omitted
    except BaseException:
        c.close()
        raise


def native_diagnostics(sql, catalog_data):
    import duckdb
    import sqlglot
    from sqlglot import exp
    c, omitted = shadow_catalog(catalog_data)
    result = []
    local_tables = set()
    c.execute("BEGIN TRANSACTION")
    try:
        for start, statement in split_sql(sql):
            prefix = 0
            try:
                parsed = c.extract_statements(statement)
                if not parsed:
                    continue
                try:
                    node = sqlglot.parse_one(statement, read="duckdb")
                except sqlglot.errors.SqlglotError:
                    node = None
                if isinstance(node, exp.Create) and node.args.get("kind") == "TABLE" and isinstance(node.this, exp.Schema):
                    fields = [quote(col.name) + " " + col.args["kind"].sql(dialect="duckdb") for col in node.this.expressions
                              if isinstance(col, exp.ColumnDef) and isinstance(col.args.get("kind"), exp.DataType)]
                    if fields and isinstance(node.this.this, exp.Table):
                        try:
                            c.execute(f"CREATE TABLE {node.this.this.sql(dialect='duckdb')} ({', '.join(fields)})")
                        except duckdb.Error:
                            pass
                        local_tables.add(node.this.this.name.lower())
                if parsed[0].type != duckdb.StatementType.SELECT or node is None:
                    continue
                if any(t.args.get("when") or t.name.lower() in omitted for t in node.find_all(exp.Table)):
                    continue
                visible_names = local_tables | {cte.alias_or_name.lower() for cte in node.find_all(exp.CTE)}
                if not catalog_data.get("tables") and any(isinstance(t.this, exp.Identifier) and t.name.lower() not in visible_names for t in node.find_all(exp.Table)):
                    continue
                prefix = 8
                c.execute("EXPLAIN " + statement)
            except duckdb.Error as error:
                msg = str(error)
                if any(s in msg for s in ("disabled through configuration", "Permission Error", "not allowed", "Autoloading", "Extension Autoloading")):
                    continue
                # Do not label functions supplied by attached extensions/macros as absent.
                missing = re.search(r"Function with name (\S+) does not exist", msg)
                if missing and any(f["name"].lower() == missing[1].lower() for f in catalog_data.get("functions", [])):
                    continue
                loc = re.search(r"LINE (\d+): [^\n]*\n([ ]*)\^", msg)
                at = 0
                if loc:
                    line = int(loc[1]) - 1
                    col = max(0, len(loc[2]) - len(f"LINE {line + 1}: ") - (prefix if line == 0 else 0))
                    at = sum(map(len, statement.splitlines(keepends=True)[:line])) + col
                at = min(start + at, len(sql))
                result.append(diagnostic(sql, at, min(at + 1, len(sql)), msg.split("\n\nLINE ")[0]))
    finally:
        c.execute("ROLLBACK")
    return result


def char_offset(text, pos):
    index = sum(map(len, text.splitlines(keepends=True)[:pos["line"]]))
    units = 0
    for ch in text[index:]:
        if units >= pos["character"] or ch in "\r\n":
            break
        units += len(ch.encode("utf-16-le")) // 2
        index += 1
    return index


def analyze(params):
    text = params["text"]
    result = {"diagnostics": [], "rendered": None, "mappings": [], "native": False}
    sql = text
    templated = None
    if params.get("render"):
        result.update(render(params))
        templated = result.pop("_templated", None)
        if result["rendered"] is None:
            return result
        sql = result["rendered"]
    elif "{{" in text or "{%" in text:
        return result
    if params.get("native", True) and importlib.util.find_spec("duckdb") and importlib.util.find_spec("sqlglot"):
        diagnostics = native_diagnostics(sql, params.get("catalog") or {})
        result["native"] = True
        for item in diagnostics:
            if templated:
                start = char_offset(sql, item["range"]["start"])
                end = char_offset(sql, item["range"]["end"])
                try:
                    source = templated.templated_slice_to_source_slice(slice(start, end))
                    # Reject ambiguous generated locations rather than misplacing diagnostics.
                    if not templated.is_source_slice_literal(source):
                        continue
                    item["range"] = {"start": position(text, source.start), "end": position(text, source.stop)}
                except (ValueError, IndexError):
                    continue
            result["diagnostics"].append(item)
    if params.get("lint") and params.get("config", {}).get("provider") != "none":
        result["diagnostics"].extend(lint(params))
    return result


def lint(params):
    provider = params.get("config", {}).get("provider")
    if provider == "sqruff":
        return sqruff_run(params, fix=False)
    from sqlfluff.core import Linter
    linted = Linter(config=provider_config(params)).lint_string(params["text"], fname=params["path"])
    output = []
    for violation in linted.get_violations():
        row = violation.to_dict()
        start = sum(map(len, params["text"].splitlines(keepends=True)[:row.get("start_line_no", 1) - 1])) + row.get("start_line_pos", 1) - 1
        output.append(diagnostic(params["text"], start, min(start + 1, len(params["text"])), row["description"], "SQLFluff", 2, row.get("code")))
    return output


def sqruff_run(params, fix):
    # Preserve provider semantics and project discovery by using a temporary
    # sibling file. Cleanup is guaranteed; original buffers/files never change.
    parent = Path(params["path"]).parent
    with tempfile.NamedTemporaryFile(mode="w", suffix=".sql", prefix=".duckdb-lsp-", dir=parent, delete=False) as f:
        f.write(params["text"])
        filename = f.name
    try:
        config = configparser.ConfigParser(interpolation=None)
        for key, value in params.get("config", {}).get("values", {}).items():
            group, name = key.rsplit(".", 1)
            if group == "core": section = "sqruff"
            elif group.startswith("rules."): section = "sqruff:rules:" + group[6:]
            else: section = "sqruff:" + group.replace(".", ":")
            if not config.has_section(section): config.add_section(section)
            if group == "core" and isinstance(value, list): value = ",".join(map(str, value))
            if isinstance(value, (dict, list, bool)) or value is None: value = repr(value)
            if group == "templater.jinja" and name in ("load_macros_from_path", "loader_search_path", "library_path"):
                origin = params.get("config", {}).get("sources", {}).get(key)
                base = Path(origin).parent if origin and origin != "inline directive" else Path(params.get("root", "."))
                value = ",".join(str((base / v.strip()).resolve()) for v in str(value).split(","))
            config.set(section, name, str(value))
        if not config.has_section("sqruff"): config.add_section("sqruff")
        config.set("sqruff", "dialect", "duckdb")
        config.set("sqruff", "templater", "jinja" if params.get("render") else "raw")
        config_file = filename + ".cfg"
        with open(config_file, "w") as output: config.write(output)
        executable = ["sqruff"] if shutil.which("sqruff") else [sys.executable, "-c", "from sqruff.main import main; main()"]
        args = executable + ["fix" if fix else "lint", filename, "--config", config_file, "--dialect", "duckdb"]
        if not fix:
            args.extend(["--format", "json"])
        result = subprocess.run(args, cwd=params.get("root", str(parent)), capture_output=True, text=True, timeout=15)
        if fix:
            if result.returncode not in (0, 1):
                raise ValueError("sqruff formatting failed")
            return Path(filename).read_text()
        payload = json.loads(result.stdout)
        if isinstance(payload, dict):
            payload = [{"violations": v} for v in payload.values()]
        output = []
        for file in payload:
            for row in file.get("violations", []):
                start = sum(map(len, params["text"].splitlines(keepends=True)[:row.get("start_line_no", 1) - 1])) + row.get("start_line_pos", 1) - 1
                output.append(diagnostic(params["text"], start, min(start + 1, len(params["text"])), row.get("description", "SQL style violation"), "sqruff", 2, row.get("code")))
        return output
    finally:
        Path(filename).unlink(missing_ok=True)
        Path(filename + ".cfg").unlink(missing_ok=True)


def format_sql(params):
    if params.get("config", {}).get("provider") == "sqruff":
        return {"text": sqruff_run(params, fix=True)}
    if params.get("config", {}).get("provider") != "sqlfluff":
        raise ValueError("Select sqruff or SQLFluff to format")
    from sqlfluff.core import Linter
    fixed = Linter(config=provider_config(params)).lint_string(params["text"], fname=params["path"], fix=True)
    from sqlfluff.core import SQLTemplaterError, SQLParseError
    if fixed.get_violations(types=(SQLTemplaterError, SQLParseError)):
        raise ValueError("Template rendering or SQL parsing failed")
    return {"text": fixed.fix_string()[0]}


OPERATIONS = {"catalog": catalog, "analyze": analyze, "format": format_sql,
              "capabilities": lambda _: {name: importlib.util.find_spec(name) is not None for name in ("duckdb", "sqlglot", "sqlfluff", "jinja2")}}


def main():
    for line in sys.stdin:
        try:
            request = json.loads(line)
            with contextlib.redirect_stdout(sys.stderr):
                result = OPERATIONS[request["method"]](request.get("params") or {})
            response = {"result": result}
        except Exception as error:
            # Driver messages can contain connection credentials. Keep IPC errors generic.
            response = {"error": type(error).__name__ + ": worker operation failed; check configuration, dependencies, and connection access."}
        output = json.dumps(response, default=str)
        if len(output.encode()) > 32 * 1024 * 1024:
            output = json.dumps({"error": "Worker response exceeded 32 MiB limit"})
        print(output, flush=True)


if __name__ == "__main__":
    main()
