# Completeness policy (#1071)

Opcode 1 remains byte-compatible with its 450 independent frozen Node fixture rows.
Opcode 2 supplies Rust-selected report checks, evidence selections, detail templates,
overall status and the reviewing guard through an incremental host protocol. It has no
runtime selector or JavaScript business-policy fallback.

Each continuation has a program counter and 32 registers of opaque raw-reference/type
projections. The host performs only property reads, native collection calls/callbacks,
coercion and materialization. Rust makes every check/guard decision. Each callback uses
an independent small program; no request history or growing journal prefix is replayed.
Raw evidence stays in a per-invocation host pool, preserving exact original references
and allowing reentrant getters without sharing another report's continuation state.

Frames are bounded to 64 KiB and strings cross only as a bounded prefix/length/type.
There is no aggregate 8 MiB acceptance cap on the new policy. Huge/cyclic/BigInt evidence
can produce the same useful reports as the old host. Host references/output use linear
memory; total runtime/output is not bounded for arbitrarily large or endless iterables.

The original property/method/coercion schedule is observable and is preserved, including
custom collection methods, sparse iteration, changing getters and first thrown errors.
TypeError text is qualified against the same pinned Node 22 source. The mechanical
canonical/reportHash codec is unchanged. Opcode 1 remains the independent final
representation/refusal comparison: ordinary over-limit or non-JSON inputs retain their
same conservative report and unverified classification. A new native module/protocol
fault is a strict failure and cannot fabricate a fallback report.

Correctly checksummed old modules that omit opcode 2 must refuse activation. A production
host lock may advance only after the new Rust source is merged and its archive/module
hashes are independently verified. Private QA hash substitution is not a shipping pin.

The actual WASM host suite retains the frozen generator/report data and compares report
property graphs, source reference identity, callback/read/coercion traces, custom methods,
iterators, reentrancy, large/opaque inputs and fail-closed guard behavior. The old worker
assertion body remains historical input; the #2084 native replacement rethrows worker
assertions outside jobs.run, verifies no journal bytes were appended on a fault, and
requires successful restored review/hash and completed worker outcome.
