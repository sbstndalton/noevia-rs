# Diary corpus write transform protocol v1

One process handles exactly one request. Invoke `diary-corpus-write-cli transform-v1`.
Input: `DCW1` + unsigned 64-bit big-endian JSON byte count + UTF-8 JSON + `END1` + EOF.
Output on success: `DWR1` + unsigned 64-bit big-endian JSON byte count + UTF-8 JSON + `END1` + EOF and exit 0. Input whole frame <=272 MiB; output whole frame <=512 MiB. Invalid framing/schema/capacity writes no successful response frame and exits nonzero with one short stderr code. No paths, credentials, ETags, DB access or wall clock are accepted.

Request JSON rejects duplicate fields, has `version:1` and exact operation keys:
- `prepare_append`: `time_token`, `topic`, `me`, `assistant`, `xid` strings. Reply `sub_header`, `body` strings.
- `apply_append`: `current` string or null, `day_header`, `sub_header`, `body`, `xid` strings. Reply `changed` boolean and `text` string.
- `inspect_edit`: `current` string or null, `xid`, `new_me`, `new_assistant` strings. Reply `found` boolean, `current_text`, `current_hash`, `edited_text`, `edited_hash` strings or null.
- `apply_edit`: `current` string or null, `xid`, `new_me`, `new_assistant` strings. Reply `found`, `changed` booleans and `text` string or null.

Every reply also has exact `version:1` and echoed `op`. `time_token` and `day_header` are computed by Python from its locale and supplied as data; native code has no ambient locale/clock. Python owns XID creation, consent, journal, paths, ETag retry loop and write result. A `found:false` edit is a format-level absence/malformed target; process failure is a separate unavailable condition.
