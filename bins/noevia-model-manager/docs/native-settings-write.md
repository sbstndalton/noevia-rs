# Native settings write (noevia#1318)

`native-settings-write` with `MODEL_MANAGER_SETTINGS_WRITE_IMPL=rust` (and `MODEL_MANAGER_IMPL=rust`) serves `PUT /api/v1/settings`. Default `python`. It is independent of `MODEL_MANAGER_SETTINGS_IMPL`, which still decides who answers the GET.

## Behaviour (`api.put_settings`)
- **Body:** a FastAPI `dict` body (the shared reader: same 422s and caps).
- **Token:** if the body has an `hfToken` key, `str(value or "").strip()` is upserted into `kv` (`hf_token`). Empty clears the token.
- **Answer:** the settings answer (`{hasToken, tokenHint}`) read back natively.
- **Test:** when `test` is truthy, the result gets `test: {ok, message}`. Checking the token calls Hugging Face, which this side does not do yet, so the Python child's private `GET /api/v1/internal/hf-token-test` runs `put_settings`' own check on the stored token. Its answer is placed verbatim, and the token itself is never sent.
- **Store:** opened read-write without CREATE; the journal mode is never changed, and a missing store is 500 and not created. A token over 64 KiB is refused with 400 (`hfToken exceeds 65536 bytes`), because the native read refuses values over that.
- **Concurrency:** one in-process lock serialises writes, and the wait for it is bounded by the 4.5 s request deadline (503 `service is busy`).

## Decisions (technical)
- If the child cannot run the test (unreachable, or an answer that is not exactly `{ok: bool, message: str}`), the token stays saved and `test` is `{"ok": false, "message": "Test failed: the token test is unavailable"}`. That matches the shape of Python's own `Test failed: …` answer instead of a 5xx after a successful write.
- The over-64 KiB refusal is new; Python stored such tokens.

## Differential evidence
`tools/gen-settings-write-oracle.py` runs the actual services app (pinned `8db7b08`) step by step on stores built by Python's own `db.init()`: the full schema, WAL, every connection closed. `hf.validate_token` is replaced by a synthetic check. It records each answer, the stored token after it, and the child's test answer.

The cases cover setting, clearing, `null`/`false`/numbers/lists (`repr`), unicode, duplicate keys, test on/off, a raising check, and body errors. `tests/settings_write.rs` replays them byte for byte (answers and stored token, WAL preserved) against a fake child. It also covers the unavailable test, the 64 KiB refusal, and a missing store that is not created.
