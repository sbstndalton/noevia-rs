# Native database bootstrap (#1318 step 4)

Compile native-db-init (which includes native-prompts) and select
MODEL_MANAGER_DB_INIT_IMPL=rust, with MODEL_MANAGER_PROMPTS_IMPL=rust.
The parent is permanently native; its retired MODEL_MANAGER_IMPL key is ignored. The selector defaults to python; an unknown
value, omitted feature or missing native prompt owner refuses startup.

The parent uses the existing secure prompt Store, its connection and lifetime writer lock.
DATA_DIR passes the prompt writer's canonical-path, trusted-ancestor, file ownership and
companion-file checks. It creates the two prompt tables as before, then bootstraps the full
schema and the three historical bench_result columns in one transaction before the existing
prompt seed and before spawning the child. The five-second startup deadline, SQLite progress
limit and cancellation flag cover that transaction; cancellation is rechecked before commit.
A failed bootstrap rolls back its transaction. The pre-existing secure open can still leave
the database, WAL/SHM, lock file and the two prompt tables; this is not a zero-files guarantee.

The source schema's column types, defaults, nullability, primary/unique keys, named indexes
and AUTOINCREMENT rules are checked. Column order is ignored because legacy ALTERs append
columns. Unknown columns, unique constraints, generated columns, triggers and incompatible
schemas refuse rather than being silently repaired. Unrelated tables and extra non-unique
indexes stay untouched. No data rows are rewritten by schema initialization.

The parent overwrites NOEVIA_NATIVE_MODEL_MANAGER_DB_INIT_CONFIRMED with 1 or 0.
Only exact 1 skips the child's db.init(). The service-first guard merged in noevia-services#102
(2328a993d09a78e1d8e5fb9e9c0e540f7a2f9303); the parent must not enable ownership against a child without that guard.
Prompt seed, benchmark orphan cleanup and models.ini migration retain their existing owners.

tools/gen-db-init-oracle.py executes the actual service db.py with synthetic settings and
fresh, legacy and repeated databases. Fixtures record the service revision, source digest and
generator digest, complete metadata and data values. CI regenerates those inputs before tests.
Rollback: select MODEL_MANAGER_DB_INIT_IMPL=python and restart; Python sees the same schema
and preserves its rows. Keep the native prompts selector consistent with existing ownership.

The service image's compiled-feature list must add noevia-model-manager/native-db-init and
check its advertisement when repinning to a merged Rust revision containing this change.
The service-first guard alone does not add that build capability or enable the selector.
