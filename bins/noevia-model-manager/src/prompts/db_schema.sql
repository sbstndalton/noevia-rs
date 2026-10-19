            CREATE TABLE IF NOT EXISTS kv (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS download_history (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                repo_id      TEXT NOT NULL,
                filename     TEXT NOT NULL,
                dest_path    TEXT NOT NULL,
                total_bytes  INTEGER NOT NULL,
                status       TEXT NOT NULL,
                error        TEXT,
                started_at   REAL NOT NULL,
                completed_at REAL
            );
            CREATE TABLE IF NOT EXISTS avatar_cache (
                owner       TEXT PRIMARY KEY,
                url         TEXT NOT NULL,
                fetched_at  REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS update_check (
                filename          TEXT PRIMARY KEY,
                repo_id           TEXT NOT NULL,
                hf_last_modified  TEXT,
                checked_at        REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS req_timing (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                ts            REAL NOT NULL,
                backend       TEXT NOT NULL,
                instance      TEXT NOT NULL,
                task          INTEGER NOT NULL,
                model_path    TEXT NOT NULL DEFAULT '',
                alias         TEXT NOT NULL DEFAULT '',
                spec_type     TEXT NOT NULL DEFAULT '',
                prompt_tokens INTEGER NOT NULL DEFAULT 0,
                prompt_tps    REAL    NOT NULL DEFAULT 0,
                gen_tokens    INTEGER NOT NULL DEFAULT 0,
                gen_tps       REAL    NOT NULL DEFAULT 0,
                draft_acc     REAL,
                draft_len     REAL,
                UNIQUE(backend, instance, task)
            );
            CREATE INDEX IF NOT EXISTS req_timing_model
                ON req_timing(model_path, ts DESC);
            CREATE INDEX IF NOT EXISTS req_timing_alias
                ON req_timing(alias, ts DESC);
            CREATE INDEX IF NOT EXISTS req_timing_dedupe
                ON req_timing(backend, task, ts);
            CREATE TABLE IF NOT EXISTS bench_run (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                backend     TEXT NOT NULL,
                status      TEXT NOT NULL,
                started_at  REAL NOT NULL,
                finished_at REAL,
                reps        INTEGER NOT NULL DEFAULT 1,
                max_tokens  INTEGER NOT NULL DEFAULT 256,
                note        TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS bench_variant (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id   INTEGER NOT NULL,
                alias    TEXT NOT NULL,
                argv_json TEXT NOT NULL DEFAULT '{}',
                load_ms  REAL,
                UNIQUE(run_id, alias)
            );
            CREATE TABLE IF NOT EXISTS bench_result (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                variant_id  INTEGER NOT NULL,
                prompt_name TEXT NOT NULL,
                rep         INTEGER NOT NULL,
                cold        INTEGER NOT NULL DEFAULT 0,
                ttft_ms     REAL,
                ttft_answer_ms REAL,
                truncated   INTEGER NOT NULL DEFAULT 0,
                total_ms    REAL,
                prompt_n    INTEGER,
                prompt_tps  REAL,
                gen_n       INTEGER,
                gen_tps     REAL,
                draft_n     INTEGER,
                draft_acc   REAL,
                peak_vram_json TEXT NOT NULL DEFAULT '[]',
                contended   INTEGER NOT NULL DEFAULT 0,
                err         TEXT NOT NULL DEFAULT '',
                response_text TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS bench_result_variant ON bench_result(variant_id);
            CREATE TABLE IF NOT EXISTS bench_sweep (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id       INTEGER NOT NULL,
                alias        TEXT NOT NULL DEFAULT '',
                model_type   TEXT NOT NULL DEFAULT '',
                test         TEXT NOT NULL DEFAULT '',
                n_prompt     INTEGER, n_gen INTEGER, n_depth INTEGER,
                avg_ts       REAL,    stddev_ts REAL,
                n_gpu_layers INTEGER, n_cpu_moe INTEGER, n_ubatch INTEGER,
                type_k       TEXT,    type_v TEXT, flash_attn INTEGER,
                split_mode   TEXT,
                raw_json     TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX IF NOT EXISTS bench_sweep_run ON bench_sweep(run_id);
            CREATE TABLE IF NOT EXISTS server_config (
                backend    TEXT NOT NULL,
                instance   TEXT NOT NULL,
                alias      TEXT NOT NULL DEFAULT '',
                model_path TEXT NOT NULL DEFAULT '',
                argv_json  TEXT NOT NULL DEFAULT '{}',
                first_seen REAL NOT NULL DEFAULT 0,
                PRIMARY KEY (backend, instance)
            );
            CREATE TABLE IF NOT EXISTS prompts (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                name        TEXT NOT NULL,
                body        TEXT NOT NULL,
                created_at  REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS model_badge (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                alias      TEXT NOT NULL,
                category   TEXT NOT NULL,
                rating     INTEGER NOT NULL,
                note       TEXT NOT NULL DEFAULT '',
                run_id     INTEGER,
                created_at REAL NOT NULL,
                UNIQUE(alias, category)
            );
            CREATE INDEX IF NOT EXISTS model_badge_alias ON model_badge(alias);
