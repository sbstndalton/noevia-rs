"""Record streaming-fixtures.json from the frozen Python Diary exchange stream (streaming.py).

Usage (CPython 3.12, the Diary requirements installed):

    <venv>/bin/python -I crates/diary-core/tests/generate_streaming_fixtures.py <noevia-services>/diary

Runs `exchange_stream` with synthetic work functions (each emits a few events, then returns a
synthetic result or raises one of the failures the stream maps) and records every frame the
response body yields. Sources are sha256 pinned (noevia-services origin/main 6d0ba0e).
"""

import asyncio
import hashlib
import json
import pathlib
import sys

sys.dont_write_bytecode = True

PINNED = {
    "agent/streaming.py": "c6d2abfeb405075bb12560d11ff9ef832b30c0ac9b39ed7d16aae3fa6d4369eb",
    "agent/deadline.py": "aaff80faa686659203b835865782230f030f5a94ec2d7e1ffbb0bb9a316534a2",
}
OUT = pathlib.Path(__file__).with_name("streaming-fixtures.json")


def load(diary_dir):
    root = pathlib.Path(diary_dir).resolve()
    for rel, want in PINNED.items():
        got = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        if got != want:
            sys.exit(f"refusing {rel}: sha256 {got} is not the pinned {want}")
    sys.path.insert(0, str(root))
    from agent import deadline, llm, streaming, temporal_native  # noqa: E402
    return deadline, llm, streaming, temporal_native


def cases(deadline, llm, temporal_native):
    def timeout(saved):
        exc = deadline.ExchangeTimeout("synthetic deadline")
        exc.nothing_saved = not saved
        return exc

    return [
        ("ok", None),
        ("temporal_unavailable", temporal_native.TemporalUnavailable("synthetic")),
        ("timeout_nothing_saved", timeout(False)),
        ("timeout_after_save", timeout(True)),
        ("length_cap", llm.LengthCapError()),
        ("empty_answer", llm.EmptyAnswerError()),
        ("llm_error", llm.LLMError("synthetic provider body that must never be shown")),
        ("other_error", ValueError("synthetic")),
    ]


async def collect(response):
    return [chunk if isinstance(chunk, str) else chunk.decode("utf-8") async for chunk in response.body_iterator]


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    if sys.version_info[:2] != (3, 12):
        sys.exit("record with CPython 3.12, the Diary image's interpreter")
    deadline, llm, streaming, temporal_native = load(sys.argv[1])
    out = []
    for name, failure in cases(deadline, llm, temporal_native):
        def work(emit, failure=failure):
            emit({"type": "status", "text": "QA step é"})
            emit({"type": "reasoning", "text": "never shown"})
            emit({"type": "delta", "text": "Synthetic 🌿 answer"})
            if failure is not None:
                raise failure
            return {"answer": "Synthetic answer", "saved": True, "xid": "qa-0001", "day": "2031-03-20"}
        response = streaming.exchange_stream(work)
        frames = asyncio.run(collect(response))
        out.append({"name": name, "frames": frames, "headers": dict(response.headers),
                    "media_type": response.media_type})
    describe = []
    a = llm.LLMError("outer")
    try:
        try:
            raise ValueError("inner")
        except ValueError as inner:
            raise a from inner
    except llm.LLMError as exc:
        describe.append({"case": "chained", "text": streaming.describe_failure(exc)})
    data = {"source": "noevia-services diary/agent at origin/main 6d0ba0e", "python": sys.version.split()[0],
            "pinned_sha256": PINNED, "streams": out, "describe": describe,
            "constants": {"EXCHANGE_DEADLINE_S": deadline.EXCHANGE_DEADLINE_S,
                          "FIRST_TOKEN_DEADLINE_S": deadline.FIRST_TOKEN_DEADLINE_S,
                          "AUX_STEP_DEADLINE_S": deadline.AUX_STEP_DEADLINE_S}}
    OUT.write_text(json.dumps(data, ensure_ascii=False, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({len(out)} streams)")


if __name__ == "__main__":
    main()
