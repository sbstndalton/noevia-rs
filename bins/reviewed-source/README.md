# reviewed-source (inert native port)

Native port of noevia-core `code-sandbox/verify-native-source.py`, the build-time check that binds the
native verifier's source to a reviewed Git-export content digest before anything is extracted or built.
Nothing in a shipped image selects it yet. noevia-core branch `council/p3-2-reviewed-source-recipe` adds a
`NATIVE_VERIFIER_SOURCE_CHECK=rust` stage to `code-sandbox/Dockerfile`; the default stays `python`.

    reviewed-source digest <archive> <root>                       print the digest
    reviewed-source extract <archive> <root> <digest> <dest>      extract into a fresh dest if it matches
    reviewed-source extract-ref <archive> <ref> <digest> <dest>   extract, root noevia-rs-<40-hex ref>

The reference's `fetch` (download plus extract) is split: the image recipe downloads, and `extract-ref`
checks the result. Every failure prints the reference's single line and exits 1.

Digest: sha256 of `noevia-reviewed-git-export-v1\0` followed by the canonical JSON record list (Python
`json.dumps(ensure_ascii=True, separators=(',', ':'))`), sorted by UTF-8 name bytes. A record is
`[name,"directory"]`, `[name,"file",mode&0o111,size,sha256]` or `[name,"symlink",target]`. Limits as
in the reference: 64 MiB compressed, 128 MiB expanded, 20,000 members, 128 MiB of file content.

The archive reader follows CPython 3.12 `tarfile`: see `src/tar.rs` for the semantics it follows and the
three places where it refuses and the reference accepts (noevia#2706).

Parity: `tests/oracle.rs` runs 78 archive cases. Their reference verdicts are committed in
`tests/fixtures/oracle-verdicts.tsv`. To check live and re-record (python3 3.12; core exported under wt/):

    n5run --pull --with <core worktree> -- 'cd bins/reviewed-source && REVIEWED_SOURCE_RECORD=1 \
      REVIEWED_SOURCE_ORACLE=<core>/code-sandbox/verify-native-source.py cargo test --locked -p reviewed-source'

`REVIEWED_SOURCE_EXPORT`, `REVIEWED_SOURCE_EXPORT_ROOT` and `REVIEWED_SOURCE_EXPORT_DIGEST` check a real
codeload export against its pin. `REVIEWED_SOURCE_DOCKERFILE=<core>/code-sandbox/Dockerfile` checks the
image recipe stage (`tests/recipe.rs`).
