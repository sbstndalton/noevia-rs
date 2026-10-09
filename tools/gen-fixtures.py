#!/usr/bin/env python3
"""Generate the differential GGUF corpus for crates/gguf.

Builds small synthetic GGUF headers (never real model files), runs noevia's reference
gguf_meta.py over each, and writes NAME.gguf + NAME.json into crates/gguf/tests/fixtures/.
NAME.json is the Python summary, or {"__error__": {"kind": ..., "message": ...}} when the
reference raises.

    python3 tools/gen-fixtures.py --gguf-meta <noevia>/services/model-manager/app/gguf_meta.py
    # or: NOEVIA_GGUF_META=<path> python3 tools/gen-fixtures.py

gguf_meta.py is stdlib-only and is loaded straight from its file, so no venv is needed.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import struct
import sys
from pathlib import Path

U8, I8, U16, I16, U32, I32, F32, BOOL, STRING, ARRAY, U64, I64, F64 = range(13)
_FMT = {U8: "<B", I8: "<b", U16: "<H", I16: "<h", U32: "<I", I32: "<i", F32: "<f",
        BOOL: "<B", U64: "<Q", I64: "<q", F64: "<d"}


def s(x: str | bytes) -> bytes:
    b = x.encode() if isinstance(x, str) else x
    return struct.pack("<Q", len(b)) + b


def header(kv_count: int, tensors: int = 0, version: int = 3) -> bytes:
    return b"GGUF" + struct.pack("<I", version) + struct.pack("<Q", tensors) + struct.pack("<Q", kv_count)


def scalar(t: int, v) -> bytes:
    return struct.pack("<I", t) + struct.pack(_FMT[t], v)


def kv(key: str | bytes, t: int, v) -> bytes:
    if t == STRING:
        return s(key) + struct.pack("<I", STRING) + s(v)
    return s(key) + scalar(t, v)


def arr_body(sub: int, items: list, count: int | None = None) -> bytes:
    n = len(items) if count is None else count
    out = struct.pack("<IQ", sub, n)
    for it in items:
        out += s(it) if sub == STRING else struct.pack(_FMT[sub], it)
    return out


def kv_arr(key: str, sub: int, items: list, count: int | None = None) -> bytes:
    return s(key) + struct.pack("<I", ARRAY) + arr_body(sub, items, count)


def gguf(*pairs: bytes, kv_count: int | None = None, tensors: int = 0) -> bytes:
    return header(len(pairs) if kv_count is None else kv_count, tensors) + b"".join(pairs)


TEMPLATE = ("{% for m in messages %}{{ '<|im_start|>' + m.role }}{% endfor %}"
            "{% if enable_thinking %}<think>{% endif %}")


def cases() -> dict[str, bytes]:
    c: dict[str, bytes] = {}
    c["llama_full"] = gguf(
        kv("general.architecture", STRING, "llama"),
        kv("general.name", STRING, "Synthetic Llama 8B"),
        kv("general.description", STRING, "fixture"),
        kv("general.author", STRING, "noevia tests"),
        kv("general.license", STRING, "mit"),
        kv("general.url", STRING, "https://example.invalid/m"),
        kv("general.file_type", U32, 15),
        kv("general.quantization_version", U32, 2),
        kv("general.parameter_count", U64, 8_030_261_248),
        kv("llama.context_length", U32, 131072),
        kv("llama.embedding_length", U32, 4096),
        kv("llama.block_count", U32, 32),
        kv("llama.feed_forward_length", U32, 14336),
        kv("llama.attention.head_count", U32, 32),
        kv("llama.attention.head_count_kv", U32, 8),
        kv("llama.rope.freq_base", F32, 500000.0),
        kv("llama.rope.scaling.type", STRING, "yarn"),
        kv("llama.rope.scaling.factor", F32, 8.0),
        kv("llama.rope.scaling.original_context_length", U32, 8192),
        kv("llama.attention.key_length", U32, 128),
        kv("llama.attention.value_length", U32, 128),
        kv("tokenizer.ggml.model", STRING, "gpt2"),
        kv("tokenizer.ggml.pre", STRING, "llama-bpe"),
        kv_arr("tokenizer.ggml.tokens", STRING, [f"t{i}" for i in range(40)]),
        kv("tokenizer.ggml.bos_token_id", U32, 1),
        kv("tokenizer.ggml.eos_token_id", U32, 2),
        kv("tokenizer.ggml.unknown_token_id", I32, -1),
        kv("tokenizer.ggml.padding_token_id", U32, 0),
        kv("tokenizer.ggml.add_bos_token", BOOL, 1),
        kv("tokenizer.ggml.add_eos_token", BOOL, 0),
        kv("tokenizer.chat_template", STRING, TEMPLATE),
    )
    c["gemma_swa_arrays"] = gguf(
        kv("general.architecture", STRING, "gemma3"),
        kv("general.file_type", U32, 32),
        kv("gemma3.context_length", U32, 32768),
        kv_arr("gemma3.attention.head_count_kv", U32, [4, 4, 4, 4, 4, 8, 4, 4, 4, 4, 4, 8]),
        kv_arr("gemma3.attention.head_count", U32, [8, 8, 16, 16, 8, 16]),
        kv_arr("gemma3.attention.sliding_window_pattern", BOOL, [1, 1, 1, 1, 1, 0]),
        kv("gemma3.attention.sliding_window", U32, 1024),
        kv("gemma3.attention.key_length_swa", U32, 128),
        kv("gemma3.attention.value_length_swa", U32, 128),
        kv("gemma3.attention.shared_kv_layers", U32, 10),
        kv_arr("gemma3.feed_forward_length", U32, [7, 7, 9, 9, 9, 7, 3, 3, 3, 3]),
        kv("tokenizer.chat_template", STRING, "<start_of_turn>{{ '<|channel|>thought' }}"),
    )
    c["mode_tie_first_seen_wins"] = gguf(
        kv("general.architecture", STRING, "qwen3moe"),
        kv_arr("qwen3moe.block_count", I64, [5, 3, 3, 5, -1, 2, 9, 1, 1, 1], count=10),
        kv_arr("qwen3moe.expert_count", F32, [2.5, 2.9, 3.0, 3.0, -0.5, 0.2, 7.0, 7.0, 1.0]),
        kv_arr("qwen3moe.context_length", BOOL, [1, 0, 0, 1, 1, 0, 2, 0, 0]),
        kv("qwen3moe.expert_used_count", U8, 8),
        kv("qwen3moe.nextn_predict_layers", U16, 1),
        kv("qwen3moe.full_attention_interval", I8, -4),
        kv("qwen3moe.ssm.state_size", I16, 16),
        kv("qwen3moe.ssm.inner_size", F64, 3072.9),
        kv("qwen3moe.ssm.conv_kernel", U32, 4),
        kv("qwen3moe.ssm.group_count", F32, 8.0),
        kv_arr("qwen3moe.rope.freq_base", F32, [1.5, 2.5]),
        kv_arr("qwen3moe.rope.scaling.factor", STRING, ["x"]),
        kv_arr("qwen3moe.embedding_length", STRING, [f"s{i}" for i in range(12)]),
        kv("qwen3moe.vocab_size", U32, 151936),
    )
    c["list_first_element"] = gguf(
        kv("general.architecture", STRING, "phi3"),
        kv_arr("phi3.context_length", U32, [4096, 8192]),
        kv_arr("phi3.block_count", F64, [31.9, 2.0]),
        kv_arr("phi3.embedding_length", STRING, ["3072"]),
        kv_arr("phi3.feed_forward_length", U32, []),
        kv_arr("phi3.attention.head_count_kv", U32, [32, 32]),
        kv("tokenizer.ggml.tokens", U32, 5),
        kv("phi3.vocab_size", U64, 32064),
    )
    # params / quant formatting
    for name, t, v in [("params_zero", U64, 0), ("params_999", U32, 999), ("params_1000", U32, 1000),
                       ("params_tie_2_25b", U64, 2_250_000_000), ("params_1_5t", U64, 1_500_000_000_000),
                       ("params_float", F64, 12345.678), ("params_bool_true", BOOL, 1),
                       ("params_negative", I64, -5), ("params_u64_max", U64, (1 << 64) - 1),
                       ("params_small_float", F64, 0.5),
                       ("params_string", STRING, "7B")]:
        c[name] = gguf(kv("general.parameter_count", t, v), kv("general.architecture", STRING, "llama"))
    for name, t, v in [("quant_unknown", U32, 99), ("quant_bool", BOOL, 1), ("quant_float_int", F32, 15.0),
                       ("quant_float_frac", F64, 15.5), ("quant_neg_zero", F64, -0.0),
                       ("quant_string", STRING, "Q9_X"), ("quant_big_float", F64, 1e20),
                       ("quant_i64_neg", I64, -3)]:
        c[name] = gguf(kv("general.file_type", t, v))
    c["quant_list_raises"] = gguf(kv_arr("general.file_type", U32, [15]))
    c["quant_summary_raises"] = gguf(kv_arr("general.file_type", U32, list(range(9))))
    # non-string architectures make odd but deterministic key prefixes
    c["arch_int"] = gguf(kv("general.architecture", U32, 7), kv("7.block_count", U32, 3))
    c["arch_zero_is_empty"] = gguf(kv("general.architecture", U32, 0), kv("0.block_count", U32, 3))
    c["arch_bool"] = gguf(kv("general.architecture", BOOL, 1), kv("True.block_count", U32, 4))
    c["arch_float"] = gguf(kv("general.architecture", F64, 1e16), kv("1e+16.block_count", U32, 5))
    c["arch_float_small"] = gguf(kv("general.architecture", F32, 1.5), kv("1.5.block_count", U32, 6))
    c["arch_list"] = gguf(kv_arr("general.architecture", STRING, ["a", "b'c"]),
                          kv("['a', \"b'c\"].block_count", U32, 7))
    c["arch_array_summary"] = gguf(kv_arr("general.architecture", U8, list(range(9))),
                                   kv("{'_array': True, 'count': 9, 'sample': [0, 1, 2, 3, 4, 5, 6, 7]}"
                                      ".block_count", U32, 8))
    c["arch_empty_string"] = gguf(kv("general.architecture", STRING, ""), kv(".block_count", U32, 9))
    # strings and text
    c["invalid_utf8"] = gguf(
        kv("general.architecture", STRING, "llama"),
        s("general.name") + struct.pack("<I", STRING) + s(b"ok\xff\xfe \xed\xa0\x80 \xe2\x82 \xf0\x9f\x98"),
        s(b"llama.\xc3(") + scalar(U32, 1),
        kv("general.description", STRING, "café ☃ \U0001F600 \"q\" \\ \n\t\x01"),
    )
    c["template_uppercase"] = gguf(kv("tokenizer.chat_template", STRING,
                                      "<THINK> Reasoning_Effort PRESERVE_THINKING channel>thought"))
    c["template_non_string"] = gguf(kv("tokenizer.chat_template", U32, 5))
    c["template_empty"] = gguf(kv("tokenizer.chat_template", STRING, ""))
    c["template_no_features"] = gguf(kv("tokenizer.chat_template", STRING, "{{ messages }}"))
    # floats
    c["floats_to_ints"] = gguf(
        kv("general.architecture", STRING, "llama"),
        kv("llama.context_length", F64, 1e300),
        kv("llama.embedding_length", F64, -2.7),
        kv("llama.block_count", F32, 3.4028234663852886e38),
        kv("llama.rope.freq_base", U64, (1 << 64) - 1),
        kv("llama.rope.scaling.factor", BOOL, 1),
        kv("llama.rope.scaling.type", F32, 0.1),
        kv("llama.attention.head_count", I64, -(1 << 63)),
    )
    # non-finite floats become None before any int()/float() coercion (noevia#901, #913)
    nan, inf = float("nan"), float("inf")
    nonfinite = [("nan", nan), ("inf", inf), ("neginf", -inf)]
    int_fields = ["context_length", "embedding_length", "block_count", "feed_forward_length",
                  "attention.head_count", "rope.scaling.original_context_length", "vocab_size",
                  "expert_count", "nextn_predict_layers", "expert_used_count",
                  "attention.key_length", "attention.value_length", "full_attention_interval",
                  "attention.sliding_window", "attention.key_length_swa",
                  "attention.value_length_swa", "attention.shared_kv_layers", "ssm.state_size",
                  "ssm.inner_size", "ssm.conv_kernel", "ssm.group_count"]
    for field in int_fields:
        slug = field.replace(".", "_")
        key = f"llama.{field}"
        for tag, v in nonfinite:
            c[f"nonfinite_{slug}_{tag}"] = gguf(kv("general.architecture", STRING, "llama"),
                                                kv(key, F64, v))
        c[f"nonfinite_{slug}_f32_inf"] = gguf(kv("general.architecture", STRING, "llama"),
                                              kv(key, F32, inf))
        c[f"nonfinite_{slug}_list_first"] = gguf(kv("general.architecture", STRING, "llama"),
                                                 kv_arr(key, F64, [nan, 4.0]))
        c[f"nonfinite_{slug}_sample_mode"] = gguf(
            kv("general.architecture", STRING, "llama"),
            kv_arr(key, F32, [inf, -inf, nan, nan, inf, 6.0, 7.0, 7.0, 1.0]))
        c[f"nonfinite_{slug}_sample_all"] = gguf(
            kv("general.architecture", STRING, "llama"),
            kv_arr(key, F64, [nan, inf, -inf, nan, inf, -inf, nan, inf, 3.0]))
    for tag, v in nonfinite:
        c[f"nonfinite_params_{tag}"] = gguf(kv("general.parameter_count", F64, v))
        c[f"nonfinite_quant_{tag}"] = gguf(kv("general.file_type", F64, v))
        c[f"nonfinite_arch_{tag}"] = gguf(kv("general.architecture", F64, v),
                                          kv(".block_count", U32, 3), kv("nan.block_count", U32, 4),
                                          kv("inf.block_count", U32, 5))
        c[f"nonfinite_rope_{tag}"] = gguf(kv("general.architecture", STRING, "llama"),
                                          kv("llama.rope.freq_base", F64, v),
                                          kv("llama.rope.scaling.factor", F32, v),
                                          kv_arr("llama.rope.scaling.type", F64, [v, 1.0]))
    c["nonfinite_quant_list"] = gguf(kv_arr("general.file_type", F64, [nan]))
    c["nonfinite_arch_list"] = gguf(kv_arr("general.architecture", F64, [nan, 1.5]),
                                    kv("[None, 1.5].block_count", U32, 6),
                                    kv("[nan, 1.5].block_count", U32, 7))
    c["nonfinite_arch_sample"] = gguf(kv_arr("general.architecture", F32, [inf] * 9),
                                      kv("{'_array': True, 'count': 9, 'sample': "
                                         "[None, None, None, None, None, None, None, None]}"
                                         ".block_count", U32, 8))
    c["nonfinite_passthrough"] = gguf(
        kv("general.architecture", STRING, "llama"),
        kv("general.name", F64, nan),
        kv("general.quantization_version", F32, inf),
        kv("tokenizer.chat_template", F64, -inf),
        kv("tokenizer.ggml.bos_token_id", F64, nan),
        kv("tokenizer.ggml.eos_token_id", F32, inf),
        kv_arr("llama.attention.head_count_kv", F32, [nan, 8.0, inf]),
        kv_arr("llama.attention.sliding_window_pattern", F64, [nan] * 10),
        kv_arr("tokenizer.ggml.tokens", F32, [nan] * 12),
    )
    c["nonfinite_vocab_list"] = gguf(kv("general.architecture", STRING, "llama"),
                                     kv_arr("tokenizer.ggml.tokens", F32, [nan, 1.0]),
                                     kv("llama.vocab_size", F64, inf))
    # dict semantics
    c["duplicate_keys_last_wins"] = gguf(kv("general.name", STRING, "first"), kv("general.name", STRING, "second"))
    c["file_overrides_internal_keys"] = gguf(kv("_gguf_version", STRING, "spoof"), kv("_error", U32, 7),
                                             kv("_kv_count", BOOL, 0))
    c["file_error_key_then_ran_out"] = gguf(kv("_error", STRING, "mine"),
                                            kv_arr("big", U32, list(range(8)), count=1000))
    # arrays
    c["nested_one_level"] = gguf(s("k") + struct.pack("<I", ARRAY) + struct.pack("<IQ", ARRAY, 2)
                                 + arr_body(U32, [7, 8]) + arr_body(U16, list(range(10))))
    c["nested_inner_strings"] = gguf(s("k") + struct.pack("<I", ARRAY) + struct.pack("<IQ", ARRAY, 1)
                                     + arr_body(STRING, ["a", "b"]))
    c["scalar_array_skipped_exactly"] = gguf(kv_arr("a", U16, list(range(20))), kv("after", U32, 1))
    c["string_array_skipped_exactly"] = gguf(kv_arr("tokenizer.ggml.tokens", STRING,
                                                    [f"w{i}" for i in range(30)]),
                                             kv("general.architecture", STRING, "llama"))
    c["range_cut_tokens"] = gguf(kv("llama.block_count", U32, 32),
                                 kv_arr("tokenizer.ggml.tokens", STRING, [f"t{i}" for i in range(20)],
                                        count=150_000),
                                 kv("general.architecture", STRING, "llama"), kv_count=3)
    c["range_cut_tokens_last_kv"] = gguf(kv("general.architecture", STRING, "llama"),
                                         kv_arr("tokenizer.ggml.tokens", STRING,
                                                [f"t{i}" for i in range(20)], count=150_000))
    c["range_cut_scalars_last_kv"] = gguf(kv_arr("tokenizer.ggml.scores", F32, [0.5] * 12, count=150_000))
    # header faults that become _error
    c["err_unknown_value_type"] = gguf(kv("a", U32, 1), s("b") + struct.pack("<I", 13), kv_count=3)
    c["err_unknown_array_type"] = gguf(s("a") + struct.pack("<I", ARRAY) + struct.pack("<IQ", 77, 1))
    c["err_nested_too_deep"] = gguf(s("a") + struct.pack("<I", ARRAY) + struct.pack("<IQ", ARRAY, 1)
                                    + struct.pack("<IQ", ARRAY, 1) + arr_body(U8, [1]))
    c["err_nested_count_over_kept"] = gguf(s("a") + struct.pack("<I", ARRAY) + struct.pack("<IQ", ARRAY, 9))
    c["err_string_past_end"] = gguf(kv("a", U32, 1), s("b") + struct.pack("<I", STRING) + struct.pack("<Q", 1 << 40))
    c["err_key_past_end"] = gguf(struct.pack("<Q", 50) + b"short")
    c["err_kv_truncated"] = gguf(kv("a", U32, 1), kv_count=5)
    c["err_value_truncated"] = gguf(s("a") + struct.pack("<I", U64) + b"\x01\x02")
    c["err_implausible_kv_count"] = header((1 << 64) - 1)
    c["err_sample_truncated"] = gguf(s("a") + struct.pack("<I", ARRAY) + struct.pack("<IQ", U32, 100)
                                     + struct.pack("<III", 1, 2, 3))
    c["version_and_tensors"] = gguf(kv("general.architecture", STRING, "llama"), tensors=291)
    c["version_2"] = header(0, 5, version=2)
    c["zero_kv"] = header(0)
    # raised before any KV
    c["raise_bad_magic"] = b"NOPE" + b"\0" * 32
    c["raise_magic_quote"] = b"G'\x00\\" + b"\0" * 20
    c["raise_empty"] = b""
    c["raise_short_magic"] = b"GG"
    c["raise_truncated_fixed_header"] = b"GGUF\x03\x00"
    c["raise_truncated_kv_count"] = b"GGUF" + struct.pack("<IQ", 3, 0) + b"\x01"
    # per-layer arrays (#1186): a top-level numeric array whose length equals <arch>.block_count
    # is kept whole; every other long array keeps the 8-element sample + count.
    hyb = [0, 0, 8, 0, 0, 8, 0, 0, 8, 0, 8, 0, 8, 0, 8, 0, 8, 0, 8, 0, 8, 0, 0, 8, 0, 0, 8, 0, 0, 0]
    arch_lfm2 = (kv("general.architecture", STRING, "lfm2"), kv("lfm2.block_count", U32, 30),
                 kv("lfm2.attention.head_count", U32, 32), kv("lfm2.embedding_length", U32, 2048))
    c["per_layer_hybrid_kv_kept_whole"] = gguf(*arch_lfm2, kv_arr("lfm2.attention.head_count_kv", U32, hyb),
                                               kv_arr("lfm2.feed_forward_length", I64, [8192 + i for i in range(30)]),
                                               kv_arr("lfm2.rope.freq_base", F32, [1.5] * 30),
                                               kv("lfm2.context_length", U32, 128000))
    c["per_layer_len_mismatch_summarised"] = gguf(*arch_lfm2, kv_arr("lfm2.attention.head_count_kv", U32, hyb[:29]),
                                                  kv_arr("lfm2.x", U32, list(range(31))))
    c["per_layer_bool_summarised"] = gguf(*arch_lfm2, kv_arr("lfm2.attention.sliding_window_pattern", BOOL, [1, 0] * 15))
    c["per_layer_string_summarised"] = gguf(*arch_lfm2, kv_arr("lfm2.names", STRING, [f"l{i}" for i in range(30)]))
    c["per_layer_before_block_count"] = gguf(kv("general.architecture", STRING, "lfm2"),
                                             kv_arr("lfm2.attention.head_count_kv", U32, hyb),
                                             kv("lfm2.block_count", U32, 30))
    c["per_layer_other_arch_block_count"] = gguf(kv("general.architecture", STRING, "lfm2"), kv("llama.block_count", U32, 30),
                                                 kv_arr("lfm2.attention.head_count_kv", U32, hyb))
    c["per_layer_no_arch"] = gguf(kv("lfm2.block_count", U32, 30), kv_arr("lfm2.attention.head_count_kv", U32, hyb))
    c["per_layer_block_count_bool"] = gguf(kv("general.architecture", STRING, "lfm2"), kv("lfm2.block_count", BOOL, 1),
                                           kv_arr("lfm2.a", U32, [1]))
    c["per_layer_block_count_float"] = gguf(kv("general.architecture", STRING, "lfm2"), kv("lfm2.block_count", F32, 30.0),
                                            kv_arr("lfm2.attention.head_count_kv", U32, hyb))
    c["per_layer_block_count_max"] = gguf(kv("general.architecture", STRING, "lfm2"), kv("lfm2.block_count", U64, 4096),
                                          kv_arr("lfm2.attention.head_count_kv", U16, [i % 9 for i in range(4096)]))
    c["per_layer_block_count_over_max"] = gguf(kv("general.architecture", STRING, "lfm2"), kv("lfm2.block_count", U64, 4097),
                                               kv_arr("lfm2.attention.head_count_kv", U16, [i % 9 for i in range(4097)]))
    c["per_layer_block_count_negative"] = gguf(kv("general.architecture", STRING, "lfm2"), kv("lfm2.block_count", I32, -30),
                                               kv_arr("lfm2.attention.head_count_kv", U32, hyb))
    c["per_layer_nested_not_kept"] = gguf(*arch_lfm2, s("lfm2.n") + struct.pack("<I", ARRAY) + struct.pack("<IQ", ARRAY, 30)
                                          + b"".join(struct.pack("<IQ", U8, 1) + b"\x01" for _ in range(30)))
    c["per_layer_cut_off_keeps_sample"] = gguf(*arch_lfm2, kv_arr("lfm2.attention.head_count_kv", U32, hyb[:12], count=30))
    c["per_layer_floats_kept_whole"] = gguf(*arch_lfm2, kv_arr("lfm2.attention.scale", F64, [0.5 * i for i in range(30)]))
    return c


def load_reference(path: Path):
    # gguf_meta.py imports its sibling autoconfig_core relatively (both stdlib-only). Load it as a
    # submodule of a bare package over its directory, so that import resolves without running
    # the service package's __init__ (and its dependencies).
    import types
    pkg = types.ModuleType("gguf_meta_pkg")
    pkg.__path__ = [str(path.resolve().parent)]
    sys.modules["gguf_meta_pkg"] = pkg
    spec = importlib.util.spec_from_file_location("gguf_meta_pkg.gguf_meta", path)
    if spec is None or spec.loader is None:
        sys.exit(f"cannot load {path}")
    mod = importlib.util.module_from_spec(spec)
    sys.modules["gguf_meta_pkg.gguf_meta"] = mod
    spec.loader.exec_module(mod)
    return mod


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--gguf-meta", default=os.environ.get("NOEVIA_GGUF_META"),
                    help="path to noevia services/model-manager/app/gguf_meta.py")
    ap.add_argument("--out", default=str(Path(__file__).resolve().parent.parent / "crates/gguf/tests/fixtures"))
    args = ap.parse_args()
    if not args.gguf_meta:
        sys.exit("pass --gguf-meta or set NOEVIA_GGUF_META")
    ref = load_reference(Path(args.gguf_meta))
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    for old in list(out.glob("*.gguf")) + list(out.glob("*.json")):
        old.unlink()
    for name, data in sorted(cases().items()):
        try:
            expected = ref.summarize(ref.read_raw_bytes(data))
        except Exception as e:  # noqa: BLE001 - the error itself is the expected result
            expected = {"__error__": {"kind": type(e).__name__, "message": str(e)}}
        (out / f"{name}.gguf").write_bytes(data)
        (out / f"{name}.json").write_text(json.dumps(expected, indent=1, sort_keys=True, allow_nan=False) + "\n")
    print(f"wrote {len(cases())} fixtures to {out}")


if __name__ == "__main__":
    main()
