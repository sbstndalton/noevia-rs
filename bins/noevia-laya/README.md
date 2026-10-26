# Native Laya front (S4, issue1326)

Build with cargo build --locked -p noevia-laya --features native-laya. This
launcher owns native HTTP when NOEVIA_LAYA_IMPL is absent or rust. Any other
value, including the retired python selection, fails closed. A build without
the compiled native-laya feature refuses startup before a listener or child
starts. Native failures never invoke Python HTTP. The retained Python engine
worker still runs only as /service/server.py --engine-worker.

This is the Rust launcher source change for noevia#1071. The service image,
its pinned Rust commit, image qualification and installed selector remain
separate integration work; this source change alone establishes no live mode.

Rust owns private port8040, GET `/health`, POST `/v1/decisions`, validation,
admission, deadlines, process supervision and bounded I/O. Fixed Python worker
`/service/server.py --engine-worker` remains the Laya/PyTorch engine exception.
One to four workers load independent copies; the existing private internal network,
offline environment and read-only `/model` mount remain the deployment boundary.
This source PR does not change Docker wiring, pins, network, models or owner flags.

Production defaults:90-second startup;1.3-second decision deadline with valid
`LAYA_DECISION_TIMEOUT_S`0.5–2; queue2 seconds with `LAYA_QUEUE_TIMEOUT_MS`0–10000;
`LAYA_MAX_CONCURRENCY`1–4; abandoned work grace15 seconds; replacement attempts
after0/2/8 seconds. Exhausted recovery makes the front exit nonzero. The engine
retains context-budget checks and `LAYA_KEEP_WARM_S` behavior. Deadlines or client
cancellation retain unfinished work only within grace, discard its reply, and
send no new work behind it. Health does not acquire inference slots.

Limits:16KiB request/frame/JSON response,16KiB headers,99 fields,64 connections,
3-second header/body/write deadlines. Chunked transfer, pipelining, incomplete
bodies, malformed HTTP and excessive connections are refused. These bounds harden
the existing private HTTP implementation; they are not arbitrary-protocol parity
claims. HTTP cancellation treats full close or write-half shutdown as abandonment.
Linux uses non-reaping waitid to detect an exited engine even when a descendant
retains its pipe. On macOS idle exit detection relies on pipe EOF; Linux CI is
the deployment qualification. Owned groups are killed before leaders are reaped.

Qualification uses actual services worker commit (services#35 head, #1074)
`920d57d82d8f4c817760255cff602e15ccc64448`, independently matching codeload SHA256
`540e14cf3e495e74387cb6f39b12395615b31e8737bf304e757a0d3ab8976ffd`.
Set `NOEVIA_LAYA_REFERENCE` to that checkout's `laya` directory. Run
`cargo run --locked -p sensitivity-service --example gen-fixtures -- "$NOEVIA_LAYA_REFERENCE" /tmp/laya-http.json`
and compare the committed fixture, then `cargo test --locked -p sensitivity-service`.
The oracle hashes actual server.py and runs its HTTP/Pool/Runtime/worker code with
fixed synthetic torch/laya collaborators. Cases, HTTP clients, fixture preparation
and serialization are Rust. Mandatory CI uses production Python3.12 and includes
the full unsupported-method HTML comparison; local3.14 has a different template
and cannot qualify that surface. No real ML import, inference, model loading,
download, model quality or speed qualification is included.
