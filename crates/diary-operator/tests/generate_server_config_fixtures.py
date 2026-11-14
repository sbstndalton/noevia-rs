"""Records the start-up config the Diary's Python child sends its Rust parent (main mode), for
diary-operator's `server_config` (tests/server_config.rs).

Run with CPython 3.12 and the frozen noevia-services checkout (read-only):

    python3.12 -I tests/generate_server_config_fixtures.py <services>/diary tests/fixtures/server-config-python312.json

For each synthetic case (a config.yaml text and an environment) it calls, in a clean process
environment: `agent.config.load_config`, `agent.app.AppState` (with an in-memory corpus backend,
its index file in a scratch directory), `agent.rust_forward.server_config`, and the main-mode
additions of app.py's lifespan (`native_routes`, `native_writers`, `embedding`). The record is the
JSON value Python would send, or the exception that stops its start. Synthetic values only; no
network call is made (no model is resolved at start).
"""
import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path

if sys.version_info[:2] != (3, 12):
    sys.exit("record with CPython 3.12")
diary = Path(sys.argv[1]).resolve()
out = Path(sys.argv[2]).resolve()
SOURCES = ["agent/config.py", "agent/app.py", "agent/rust_forward.py", "agent/rust_parent.py",
           "agent/rust_answer.py", "agent/rust_index.py", "agent/llm.py", "agent/external_sources.py",
           "agent/workspace_export_native.py", "agent/tenant_assertion.py", "agent/diary_migration.py",
           "config/config.yaml"]
PORT, CONTROL, FORWARD = 8010, "qa-control-token", "qa-forward-secret"
scratch = Path(tempfile.mkdtemp(prefix="qa-server-config-"))
# Nothing of the recording host's environment reaches a case.
os.environ.clear()
os.environ.update({"PATH": "/usr/bin:/bin", "HOME": str(scratch / "home"), "DIARY_CONFIG": str(scratch / "none.yaml"),
                   "DB_PATH": str(scratch / "import" / "index.db")})
(scratch / "import").mkdir()
os.chdir(scratch)
sys.path.insert(0, str(diary))
from agent import app as app_mod  # noqa: E402  (import-time config: the clean environment above)
from agent import rust_forward, rust_parent  # noqa: E402
from agent.config import load_config  # noqa: E402
from agent.workspace_files import MemoryBackend  # noqa: E402

# app.py's import-time default of retrieval.embedding_state_path (its module-level `if`),
# taken from the source and run on each case's loaded config as the child's import runs it.
import ast  # noqa: E402
_tree = ast.parse((diary / "agent/app.py").read_text(encoding="utf-8"))
_default = [n for n in _tree.body if isinstance(n, ast.If) and "embedding_state_path" in ast.unparse(n.test)]
assert len(_default) == 1, "app.py's embedding_state_path default moved"
STATE_PATH_DEFAULT = compile(ast.Module(body=_default, type_ignores=[]), "app.py", "exec")

shipped = (diary / "config/config.yaml").read_text(encoding="utf-8")
UID_A, UID_B = "0b5f6c1e-3a2d-4c8e-9f10-1a2b3c4d5e6f", "00000000-0000-4000-8000-0000000000b9"
BASE = {"DIARY_PARENT": "rust", "HOME": "/home/qa-synthetic"}
FULL = {
    "LLM_BASE_URL": "http://qa-engine.invalid:8080/v1//", "LLM_API_KEY": "qa-engine-key",
    "LLM_CHAT_MODEL": "qa-chat", "LLM_CHAT_FALLBACK_MODEL": "  qa-chat-fallback ",
    "LLM_AUX_BASE_URL": "http://qa-engine.invalid:8080/v1//", "LLM_AUX_MODEL": "qa-aux",
    "LLM_AUX_API_KEY": "qa-aux-key", "LLM_AUX_FALLBACK_MODEL": " qa-aux-fallback",
    "LLM_EMBED_MODEL": "qa-embed-a", "LLM_EMBED_BASE_URL": "http://qa-embed.invalid:8081/v1/",
    "LLM_EMBED_PROFILE": "nomic-v1", "LLM_EMBED_NEXT_MODEL": "qa-embed-b",
    "LLM_EMBED_NEXT_BASE_URL": "http://qa-embed.invalid:8082/v1", "LLM_EMBED_NEXT_PROFILE": "nomic-v1",
    "EMBEDDING_REINDEX_DUTY": "0.25", "LLM_MAX_REASONING_TOKENS": " 512 ",
    "DIARY_AUTH_TOKEN": "  qa-service-token ", "DIARY_TENANT_KEY": " qa-tenant-key ",
    "DIARY_LEGACY_USER_ID": "QA-Legacy", "DIARY_CORPUS_WRITE_IMPL": " Rust ",
    "WORKSPACE_EXPORT_IMPL": "rust", "DIARY_EXTERNAL_SOURCES": "~/qa-notes, /srv/qa-a ,/srv/qa-a,,~",
    "DIARY_RUST_ROUTES": "/api/months, /api/file,/api/qa-unknown,/api/day",
    "DIARY_RUST_WRITES": f"{UID_B.upper()}, {UID_A},not-a-uuid", "DIARY_RUST_EMBEDDING": " ON ",
    "DIARY_RUST_ANSWER": "shadow", "DIARY_RUST_INDEX": "", "DB_PATH": "/data/qa/diary.db",
    "DIARY_ENTRY_LAYOUT": "daily", "DIARY_ENTRIES_PREFIX": "Entries", "DIARY_INDEX_ENABLED": " off ",
    "DIARY_MONTH_FILE_TEMPLATE": "{year}/{month02}.md",
}


def case(name, yaml_text, env):
    return {"case": name, "yaml": yaml_text, "env": {**BASE, **env}}


CASES = [
    case("shipped", shipped, {}),
    case("shipped-full-env", shipped, FULL),
    case("aux-other-engine", shipped, {**FULL, "LLM_AUX_BASE_URL": "http://qa-aux.invalid:9090/v1"}),
    case("aux-default-model", shipped, {"LLM_CHAT_MODEL": "qa-chat", "LLM_AUX_MODEL": "default"}),
    case("writers-all", shipped, {"DIARY_RUST_WRITES": f"{UID_A},*", "DIARY_RUST_ROUTES": " , "}),
    case("writers-none-valid", shipped, {"DIARY_RUST_WRITES": "qa,,", "DIARY_RUST_EMBEDDING": "yes"}),
    case("index-on", shipped, {"DIARY_RUST_INDEX": " ON ", "DIARY_RUST_ANSWER": "qa-bogus"}),
    case("answer-on-reasoning-bad", shipped, {"DIARY_RUST_ANSWER": "on", "LLM_MAX_REASONING_TOKENS": "1.5"}),
    case("reasoning-negative", shipped, {"DIARY_RUST_ANSWER": "on", "LLM_MAX_REASONING_TOKENS": "-3"}),
    case("reasoning-underscore", shipped, {"DIARY_RUST_ANSWER": "on", "LLM_MAX_REASONING_TOKENS": "1_000"}),
    case("export-blank", shipped, {"WORKSPACE_EXPORT_IMPL": "   "}),
    case("export-raw", shipped, {"WORKSPACE_EXPORT_IMPL": " Rust "}),
    case("write-impl-empty", shipped, {"DIARY_CORPUS_WRITE_IMPL": ""}),
    case("db-memory", shipped, {"DB_PATH": ":memory:"}),
    case("db-relative", shipped, {"DB_PATH": "qa-index.db"}),
    case("db-nested-relative", shipped, {"DB_PATH": "qa/sub/qa-index.db"}),
    case("sources-only-commas", shipped, {"DIARY_EXTERNAL_SOURCES": " , ,"}),
    case("sources-home-forms", shipped, {"DIARY_EXTERNAL_SOURCES": "~,~/,~/qa//x,/qa/./y/,relative/qa"}),
    case("yaml-types", "llm:\n  base_url: http://qa-engine.invalid/v1\n  chat_model: qa-chat\n"
         "  timeout_s: '45.5'\n  max_retries: 2.9\n  max_reasoning_tokens: 64\n  aux:\n"
         "    base_url: http://qa-engine.invalid/v1\n    model: qa-aux\n  embed_model: qa-embed-a\n"
         "  embed_extra: [1, 2]\n  other: x\ncorpus:\n  monthly_prefix: 7\n  index_enabled: 0\n"
         "  entry_layout: monthly\n  index_file: INDEX.md\nretrieval:\n  db_path: /data/qa/index.db\n"
         "  reindex_duty: 0.5\n  embedding_state_path: /data/qa/embedding.json\nui:\n  auth_token: ' qa '\n",
         {"DIARY_RUST_ANSWER": "on", "DIARY_RUST_EMBEDDING": "on"}),
    case("yaml-bool-timeout", "llm:\n  base_url: http://qa-engine.invalid/v1\n  timeout_s: true\n"
         "  max_retries: '4'\n  aux:\n    base_url: http://qa-other.invalid/v1\n    model: qa-aux\n"
         "corpus:\n  index_enabled: ''\nretrieval:\n  db_path: /data/qa/index.db\n", {"DIARY_RUST_INDEX": "shadow"}),
    case("yaml-null-chat", "llm:\n  base_url: http://qa-engine.invalid/v1\n  chat_model:\n  aux:\n"
         "    base_url: http://qa-engine.invalid/v1\n    model:\nretrieval:\n  db_path: /data/qa/index.db\n", {}),
    case("yaml-int-chat", "llm:\n  base_url: http://qa-engine.invalid/v1\n  chat_model: 12\n  aux:\n"
         "    base_url: http://qa-engine.invalid/v1\n    model: qa-aux\nretrieval:\n  db_path: /data/qa/index.db\n", {}),
    case("yaml-empty", "", {"LLM_BASE_URL": "http://qa-engine.invalid/v1", "LLM_AUX_BASE_URL": "http://qa-engine.invalid/v1",
                            "DB_PATH": "/data/qa/index.db"}),
    case("state-path-relative-db", shipped, {"DIARY_RUST_EMBEDDING": "on", "DB_PATH": "qa//sub/./qa.db"}),
    case("state-path-memory-db", shipped, {"DIARY_RUST_EMBEDDING": "on", "DB_PATH": ":memory:"}),
    case("state-path-root-db", shipped, {"DIARY_RUST_EMBEDDING": "on", "DB_PATH": "/qa.db"}),
    case("state-path-set", "llm:\n  base_url: http://qa-engine.invalid/v1\n  aux:\n    base_url: http://qa-engine.invalid/v1\n"
         "retrieval:\n  db_path: /data/qa/index.db\n  embedding_state_path: ''\n", {"DIARY_RUST_EMBEDDING": "on"}),
    case("retrieval-not-a-mapping", "llm:\n  base_url: http://qa-engine.invalid/v1\n  aux:\n    base_url: http://qa-engine.invalid/v1\n"
         "retrieval: 5\n", {}),
    case("aux-base-missing", "llm:\n  base_url: http://qa-engine.invalid/v1\nretrieval:\n  db_path: /data/qa/index.db\n", {}),
]


def record(c):
    cfg_path = scratch / (c["case"] + ".yaml")
    cfg_path.write_text(c["yaml"], encoding="utf-8")
    os.environ.clear()
    os.environ.update({"PATH": "/usr/bin:/bin", **c["env"]})
    data_dir = scratch / ("data-" + c["case"])
    try:
        cfg = load_config(str(cfg_path))
        exec(STATE_PATH_DEFAULT, {"_base_cfg": cfg, "Path": Path})
        # The index file goes to a scratch copy of retrieval.db_path; server_config reads the
        # configured value itself (data_root = its directory).
        real = cfg.get("retrieval.db_path")
        state_cfg = load_config(str(cfg_path))
        retrieval = state_cfg.as_dict().setdefault("retrieval", {})
        if isinstance(retrieval, dict):
            data_dir.mkdir()
            retrieval["db_path"] = str(data_dir / "index.db") if real != ":memory:" else ":memory:"
        st = app_mod.AppState(state_cfg, backend=MemoryBackend({}))
        config = rust_forward.server_config(cfg, st, PORT, CONTROL, FORWARD)
        writers_set = rust_forward.configured_writers()
        writers = ["*"] if writers_set == rust_forward.ALL_TENANTS else sorted(writers_set)
        owned = rust_parent.embedding_owned()
        sent = {**config, "native_routes": sorted(rust_forward.configured_routes(main=rust_parent.active())),
                "native_writers": writers, **({"embedding": rust_parent.embedding_config(cfg)} if owned else {})}
        return {**c, "config": json.loads(json.dumps(sent, allow_nan=False)), "error": None}
    except Exception as exc:  # noqa: BLE001 - the start would stop here
        return {**c, "config": None, "error": type(exc).__name__}


records = [record(c) for c in CASES]
fixture = {
    "source": {p: hashlib.sha256((diary / p).read_bytes()).hexdigest() for p in SOURCES},
    "python": "%d.%d.%d" % sys.version_info[:3],
    "port": PORT, "control_token": CONTROL, "forward_secret": FORWARD,
    "cases": records,
}
out.write_text(json.dumps(fixture, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
print("recorded", len(records), "cases;", sum(r["error"] is not None for r in records), "refused at start")
