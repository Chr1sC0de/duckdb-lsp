"""Required integration job: real local and PostgreSQL DuckLake metadata.

Run explicitly; failure to load an extension is a failure, never a skipped test.
Only fixture setup installs extensions or writes data.
"""
import importlib.util
import os
from pathlib import Path

import duckdb
import pytest

spec = importlib.util.spec_from_file_location("worker", Path(__file__).parents[2] / "workers/worker.py")
w = importlib.util.module_from_spec(spec)
spec.loader.exec_module(w)


@pytest.mark.parametrize("backend", ["local", "postgres"])
def test_ducklake_catalog_snapshots_and_readonly(tmp_path, backend):
    if backend == "postgres":
        metadata = "postgres:" + os.environ["DUCKLAKE_TEST_POSTGRES"]
    else:
        metadata = str(tmp_path / "metadata.ducklake")
    with duckdb.connect(config={"custom_extension_repository": "https://extensions.duckdb.org"}) as c:
        c.execute("INSTALL ducklake; LOAD ducklake")
        if backend == "postgres": c.execute("INSTALL postgres; LOAD postgres")
        c.execute(f"ATTACH {w.literal('ducklake:' + metadata)} AS lake (DATA_PATH {w.literal(str(tmp_path / 'data'))})")
        c.execute("CREATE TABLE lake.people (id INTEGER)")
        first = c.execute("SELECT max(snapshot_id) FROM lake.snapshots()").fetchone()[0]
        c.execute("ALTER TABLE lake.people ADD COLUMN name VARCHAR")
    profile = {"attachments": [{"type": "ducklake", "alias": "lake", "path": metadata}]}
    cat = w.catalog({"root": str(tmp_path), "profile": profile})
    table = next(t for t in cat["tables"] if t["catalog"] == "lake" and t["name"] == "people")
    assert table["ducklake"]
    assert [c["name"] for c in table["columns"]] == ["id", "name"]
    assert len(cat["snapshots"]["lake"]) >= 2
    assert not any(t["catalog"].startswith("__ducklake_metadata_") for t in cat["tables"])
    conn, _ = w.connect(profile, str(tmp_path))
    try:
        with pytest.raises(duckdb.Error): conn.execute("DROP TABLE lake.people")
    finally: conn.close()
    profile["attachments"][0]["snapshotVersion"] = first
    historical = w.catalog({"root": str(tmp_path), "profile": profile})
    assert [c["name"] for t in historical["tables"] if t["name"] == "people" for c in t["columns"]] == ["id"]
