#!/usr/bin/env python3
"""Run the actual service db.init against QA databases; bind outputs to source and generator bytes."""
import argparse
import ast
import hashlib
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import types

def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

def schema(conn):
    out = {}
    for name, sql in conn.execute("SELECT name,sql FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"):
        columns = [[row[1], row[2], row[3], row[4], row[5]] for row in conn.execute(f'PRAGMA table_info("{name}")')]
        indexes = {}
        for _, index, unique, origin, partial in conn.execute(f'PRAGMA index_list("{name}")'):
            indexes[index] = dict(unique=unique, origin=origin, partial=partial,
                columns=[[r[2], r[3], r[4], r[5]] for r in conn.execute(f'PRAGMA index_xinfo("{index}")')])
        out[name] = dict(columns=columns, indexes=indexes, autoincrement="AUTOINCREMENT" in sql.upper())
    return out

def rows(conn):
    tables = [r[0] for r in conn.execute("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")]
    return {table: [list(row) for row in conn.execute(f'SELECT * FROM "{table}" ORDER BY rowid')]
            for table in tables}

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--db-file", required=True)
    parser.add_argument("--services-ref", required=True)
    parser.add_argument("--output-root", required=True)
    args = parser.parse_args()
    source = Path(args.db_file).resolve()
    tree = ast.parse(source.read_text())
    init = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == "init")
    script = next(ast.literal_eval(n.args[0]) for n in ast.walk(init)
                  if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute) and n.func.attr == "executescript")
    # Strip comments only; every actual SQL statement stays as written in the source.
    script = "\n".join(line.split("--", 1)[0].rstrip() for line in script.splitlines() if line.split("--", 1)[0].strip()) + "\n"
    package = types.ModuleType("qa_db_source")
    package.__path__ = [str(source.parent)]
    config = types.ModuleType("qa_db_source.config")
    config.settings = types.SimpleNamespace(data_dir=None)
    sys.modules[package.__name__] = package
    sys.modules[config.__name__] = config
    spec = importlib.util.spec_from_file_location("qa_db_source.db", source)
    db = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(db)
    cases = []
    with tempfile.TemporaryDirectory(prefix="qa-db-init-") as folder:
        for name in ("fresh", "legacy", "repeated"):
            root = Path(folder) / name
            root.mkdir()
            config.settings.data_dir = root
            db._DB_PATH = None
            input_sql = ""
            if name != "fresh":
                with sqlite3.connect(root / "model_loader.db") as conn:
                    legacy = script
                    if name == "legacy":
                        for line in ["                ttft_answer_ms REAL,\n", "                truncated   INTEGER NOT NULL DEFAULT 0,\n",
                                     "                response_text TEXT NOT NULL DEFAULT ''\n"]:
                            legacy = legacy.replace(line, "")
                        legacy = legacy.replace("err         TEXT NOT NULL DEFAULT '',", "err         TEXT NOT NULL DEFAULT ''")
                    conn.executescript(legacy)
                    for table, definition in schema(conn).items():
                        columns, values = [], []
                        for column, kind, notnull, default, primary in definition["columns"]:
                            if primary and kind == "INTEGER":
                                continue
                            columns.append(column)
                            values.append(("qa-" + table + "-" + column) if kind == "TEXT" else 7 if kind == "INTEGER" else 1.25)
                        marks = ",".join("?" for _ in columns)
                        conn.execute(f'INSERT INTO "{table}" ({",".join(columns)}) VALUES({marks})', values)
                    conn.commit()
                    input_sql = "\n".join(line for line in conn.iterdump() if line not in ("BEGIN TRANSACTION;", "COMMIT;"))
            db.init()
            if name == "repeated":
                db.init()
            with sqlite3.connect(root / "model_loader.db") as conn:
                cases.append(dict(name=name, input_sql=input_sql, schema=schema(conn), rows=rows(conn)))
        metadata = cases[0]["schema"]
    output = Path(args.output_root)
    (output / "bins/noevia-model-manager/src/prompts").mkdir(parents=True, exist_ok=True)
    (output / "bins/noevia-model-manager/tests/fixtures").mkdir(parents=True, exist_ok=True)
    (output / "bins/noevia-model-manager/src/prompts/db_schema.sql").write_text(script)
    (output / "bins/noevia-model-manager/src/prompts/db_schema.json").write_text(json.dumps(metadata, sort_keys=True, indent=2) + "\n")
    oracle = dict(services_ref=args.services_ref, source_sha256=digest(source),
                  generator_sha256=digest(__file__), cases=cases)
    (output / "bins/noevia-model-manager/tests/fixtures/db-init-python.v1.json").write_text(json.dumps(oracle, sort_keys=True, indent=2) + "\n")

if __name__ == "__main__":
    main()
