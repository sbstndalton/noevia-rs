//! `size_plan` of autoconfig_core.py: the fit sweep, the pick, the context cap, the offload
//! presets and the prompt cache. Each function names its Python original; the arithmetic is
//! written in the same order, with the same comparisons, so that floats agree to the bit.

use crate::kv::kv_shape_bytes;
use crate::pyfloat::{self, f, round_int, round_nd, trunc_cmp};
use crate::{Backend, Error, Request, Work};

const CTX_CANDIDATES: [i128; 52] = [
    4096, 8192, 12288, 16384, 24576, 32768, 40960, 49152, 57344, 65536, 73728, 81920, 90112, 98304,
    106496, 114688, 122880, 131072, 139264, 147456, 151552, 155648, 159744, 163840, 172032, 180224,
    188416, 196608, 204800, 212992, 221184, 229376, 237568, 245760, 253952, 262144, 294912, 327680,
    360448, 393216, 425984, 458752, 491520, 524288, 589824, 655360, 720896, 786432, 851968, 917504,
    983040, 1048576,
];
const BYTES_PER_ELEM_Q8_0: f64 = 1.0625;
const RESERVE_PER_GPU: f64 = 1.0;
const CACHE_RAM_DEFAULT_MIB: i128 = 8192;
const CACHE_RAM_CONVOS: f64 = 4.0;
const CACHE_RAM_HEADROOM_GB: f64 = 8.0;
const MODEL_OVERHEAD_SINGLE: f64 = 1.00;
const MODEL_OVERHEAD_SPLIT: f64 = 1.08;
const CPU_LAYER_PENALTY: f64 = 20.0;
const UNMEASURED_CTX_CAP: i128 = 32768;
const GIB: f64 = 1073741824.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offload {
    None,
    CpuMoe,
    NCpuMoe,
    Ngl,
}

impl Offload {
    pub fn as_str(self) -> &'static str {
        match self {
            Offload::None => "",
            Offload::CpuMoe => "cpu-moe",
            Offload::NCpuMoe => "n-cpu-moe",
            Offload::Ngl => "ngl",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FitRow {
    pub ctx: i128,
    pub total_ctx: i128,
    pub model_gb: f64,
    pub kv_gb: f64,
    pub total_gb: f64,
    pub fits: bool,
    pub free_gb: f64,
    pub offload_kind: Offload,
    pub n_cpu_moe: i128,
    pub gpu_pct: i128,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PresetOption {
    pub key: String,
    pub label: String,
    pub icon: &'static str,
    pub ctx: i128,
    pub n_cpu_moe: i128,
    pub offload_kind: Offload,
    pub gpu_layers: i128,
    pub total_layers: i128,
    pub gpu_gb: f64,
    pub kv_gb: f64,
    pub speed_score: f64,
    pub ngl: i128,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackendPlan {
    pub rows: Vec<FitRow>,
    pub max_ctx: i128,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub plans: Vec<BackendPlan>,
    pub recommended: Option<usize>,
    pub offload: (Offload, i128),
    pub estimated_ctx: i128,
    pub initial_ctx: i128,
    pub capped_ctx: i128,
    pub cap: &'static str,
    pub sized: bool,
    pub ctx: i128,
    pub presets: Vec<PresetOption>,
    pub frontier: Vec<PresetOption>,
    pub active_preset: String,
    pub fits_full_gpu: bool,
    pub ngl: Option<i128>,
    pub fit: bool,
    pub cache_ram: Option<i128>,
}

/// A frontier point: (offloaded count, max per-session ctx, GPU weight GB, KV GB at that ctx).
type Point = (i128, i128, f64, f64);
/// `card_ok(gpu_gb, kind, n)` of the fit search.
type FitCardOk<'a> = &'a mut dyn FnMut(f64, Offload, i128, &mut Work) -> Result<bool, Error>;
/// `card_ok(weight_gb, kv_gb, n_cpu_moe)` of the MoE frontier.
type MoeCardOk<'a> = &'a dyn Fn(f64, f64, i128, &mut Work) -> Result<bool, Error>;

/// `_split_feasible`: does any contiguous per-card partition fit? One greedy pass.
#[allow(clippy::too_many_arguments)]
fn split_feasible(
    layers: i128,
    n_cpu_moe: i128,
    attention_gb: f64,
    expert_per_layer_gb: f64,
    kv_gb: f64,
    gpu_count: i128,
    pinned_gb: f64,
    caps: &[f64],
    work: &mut Work,
) -> Result<bool, Error> {
    if gpu_count <= 1 || caps.is_empty() {
        return Ok(true);
    }
    if layers <= 0 {
        return Ok(true); // `_layer_costs` is empty
    }
    // `_layer_costs`: attention + KV share, plus experts above the n-cpu-moe threshold.
    let att = attention_gb / f(layers);
    let kv = kv_gb / f(layers);
    let cost = |i: i128| {
        att + kv
            + if i >= n_cpu_moe {
                expert_per_layer_gb
            } else {
                0.0
            }
    };
    work.charge(usize::try_from(layers + gpu_count).unwrap_or(usize::MAX))?;
    let n = layers;
    let mut idx: i128 = 0;
    for card in 0..gpu_count {
        let cap = usize::try_from(card)
            .ok()
            .and_then(|c| caps.get(c))
            .copied()
            .ok_or(Error::Python("IndexError"))?;
        let mut budget = cap - if card == 0 { pinned_gb } else { 0.0 };
        let mut took = 0;
        while idx < n && cost(idx) <= budget {
            budget -= cost(idx);
            idx += 1;
            took += 1;
        }
        if idx >= n {
            return Ok(true);
        }
        if took == 0 {
            return Ok(false);
        }
    }
    Ok(idx >= n)
}

/// `_find_fit`: (fits, gpu_model_gb, offload_kind, n) for a given (model, kv, budget).
fn find_fit(
    model_gb_full: f64,
    kv_gb: f64,
    budget_gb: f64,
    layers: i128,
    moe_ratio: f64,
    card_ok: FitCardOk<'_>,
    work: &mut Work,
) -> Result<(bool, f64, Offload, i128), Error> {
    let mut ok = |gpu: f64, kind: Offload, n: i128, work: &mut Work| -> Result<bool, Error> {
        if gpu + kv_gb > budget_gb {
            return Ok(false);
        }
        card_ok(gpu, kind, n, work)
    };
    if ok(model_gb_full, Offload::None, 0, work)? {
        return Ok((true, model_gb_full, Offload::None, 0));
    }
    if layers <= 0 {
        return Ok((false, model_gb_full, Offload::None, 0));
    }
    if moe_ratio <= 0.0 {
        let per_layer_gb = model_gb_full / f(layers);
        for cpu_layers in 1..layers {
            work.charge(1)?;
            let gpu = per_layer_gb * f(layers - cpu_layers);
            if ok(gpu, Offload::Ngl, cpu_layers, work)? {
                return Ok((true, gpu, Offload::Ngl, cpu_layers));
            }
        }
        return Ok((false, model_gb_full, Offload::None, 0));
    }
    let attention_gb = model_gb_full * (1.0 - moe_ratio);
    let expert_per_layer_gb = model_gb_full * moe_ratio / f(layers);
    for n in 1..=layers {
        work.charge(1)?;
        let gpu = attention_gb + f(0.max(layers - n)) * expert_per_layer_gb;
        let kind = if n >= layers {
            Offload::CpuMoe
        } else {
            Offload::NCpuMoe
        };
        if ok(gpu, kind, n, work)? {
            if n >= layers {
                return Ok((true, attention_gb, Offload::CpuMoe, 0));
            }
            return Ok((true, gpu, Offload::NCpuMoe, n));
        }
    }
    Ok((false, attention_gb, Offload::CpuMoe, 0))
}

/// `_pareto_frontier`: for a MoE, sweep n-cpu-moe from 0..layers.
#[allow(clippy::too_many_arguments)]
fn pareto_frontier(
    layers: i128,
    kv_gb_at: &dyn Fn(i128, &mut Work) -> Result<f64, Error>,
    model_gb: f64,
    moe_ratio: f64,
    budget_gb: f64,
    cands: &[i128],
    n_sessions: i128,
    card_ok: MoeCardOk<'_>,
    work: &mut Work,
) -> Result<Vec<Point>, Error> {
    if moe_ratio <= 0.0 || layers <= 0 {
        return Ok(Vec::new());
    }
    let attention_gb = model_gb * (1.0 - moe_ratio);
    let per_layer_gb = model_gb * moe_ratio / f(layers);
    let mut frontier: Vec<Point> = Vec::new();
    let n = 1.max(n_sessions);
    for ncm in 0..=layers {
        work.charge(1)?;
        let gpu_layers = layers - ncm;
        let weight_gb = attention_gb + f(gpu_layers) * per_layer_gb;
        if weight_gb >= budget_gb {
            continue;
        }
        let (mut best_ctx, mut best_kv) = (0i128, 0.0f64);
        for &ctx in cands {
            let kv_gb = kv_gb_at(ctx * n, work)?;
            if !card_ok(weight_gb, kv_gb, ncm, work)? {
                continue;
            }
            if weight_gb + kv_gb <= budget_gb && ctx > best_ctx {
                best_ctx = ctx;
                best_kv = kv_gb;
            }
        }
        if best_ctx == 0 {
            continue;
        }
        if frontier.last().is_some_and(|last| best_ctx <= last.1) {
            continue;
        }
        frontier.push((ncm, best_ctx, weight_gb, best_kv));
    }
    Ok(frontier)
}

/// `_dense_frontier`: context-vs-speed frontier for a dense model, moving whole layers.
#[allow(clippy::too_many_arguments)]
fn dense_frontier(
    layers: i128,
    kv_gb_at: &dyn Fn(i128, &mut Work) -> Result<f64, Error>,
    model_gb: f64,
    budget_gb: f64,
    cands: &[i128],
    n_sessions: i128,
    card_ok: &dyn Fn(f64, f64, &mut Work) -> Result<bool, Error>,
    work: &mut Work,
) -> Result<Vec<Point>, Error> {
    // Python: `layers <= 0 or model_gb <= 0` (a NaN model_gb carries on, as it does there).
    if layers <= 0 || model_gb <= 0.0 {
        return Ok(Vec::new());
    }
    let per_layer_gb = model_gb / f(layers);
    let n = 1.max(n_sessions);
    let mut frontier: Vec<Point> = Vec::new();
    for cpu_layers in 0..layers {
        work.charge(1)?;
        let gpu_layers = layers - cpu_layers;
        let weight_gb = per_layer_gb * f(gpu_layers);
        if weight_gb >= budget_gb {
            continue;
        }
        let (mut best_ctx, mut best_kv) = (0i128, 0.0f64);
        for &ctx in cands {
            let kv_gb = kv_gb_at(ctx * n, work)?;
            if weight_gb + kv_gb <= budget_gb && ctx > best_ctx {
                if !card_ok(weight_gb, kv_gb, work)? {
                    continue;
                }
                best_ctx = ctx;
                best_kv = kv_gb;
            }
        }
        if best_ctx == 0 {
            continue;
        }
        if frontier.last().is_some_and(|last| best_ctx <= last.1) {
            continue;
        }
        frontier.push((cpu_layers, best_ctx, weight_gb, best_kv));
    }
    Ok(frontier)
}

fn moe_kind(off: i128, layers: i128) -> Offload {
    if off >= layers {
        Offload::CpuMoe
    } else if off > 0 {
        Offload::NCpuMoe
    } else {
        Offload::None
    }
}

fn dense_speed(layers: i128, cpu_layers: i128) -> f64 {
    if layers <= 0 {
        return 0.0;
    }
    let gpu = layers - cpu_layers;
    round_nd(f(layers) / (f(gpu) + f(cpu_layers) * CPU_LAYER_PENALTY), 3)
}

/// `_frontier_options`: every frontier point as an option, ascending by ctx.
fn frontier_options(frontier: &[Point], layers: i128, dense: bool) -> Vec<PresetOption> {
    let mut sorted: Vec<Point> = frontier.to_vec();
    sorted.sort_by_key(|p| p.1); // stable, as Python's sorted
    sorted
        .into_iter()
        .map(|(off, ctx, gpu_gb, kv_gb)| {
            let gpu_layers = layers - off;
            if dense {
                PresetOption {
                    key: format!("pt{off}"),
                    label: format!("{gpu_layers}/{layers} layers"),
                    icon: "sliders",
                    ctx,
                    n_cpu_moe: 0,
                    offload_kind: if off > 0 { Offload::Ngl } else { Offload::None },
                    gpu_layers,
                    total_layers: layers,
                    gpu_gb: round_nd(gpu_gb, 2),
                    kv_gb: round_nd(kv_gb, 2),
                    // `if layers` (nonzero), not `layers > 0`.
                    speed_score: if layers != 0 {
                        round_nd(f(layers) / (f(gpu_layers) + f(off) * CPU_LAYER_PENALTY), 3)
                    } else {
                        0.0
                    },
                    ngl: if off > 0 { gpu_layers } else { 999 },
                }
            } else {
                PresetOption {
                    key: format!("pt{off}"),
                    label: format!("ncm {off}"),
                    icon: "sliders",
                    ctx,
                    n_cpu_moe: off,
                    offload_kind: moe_kind(off, layers),
                    gpu_layers,
                    total_layers: layers,
                    gpu_gb: round_nd(gpu_gb, 2),
                    kv_gb: round_nd(kv_gb, 2),
                    speed_score: if layers != 0 {
                        round_nd(f(layers - off) / f(layers), 3)
                    } else {
                        0.0
                    },
                    ngl: -1,
                }
            }
        })
        .collect()
}

/// Python `min(xs, key=k)`: the first element with the smallest key.
fn first_min_by<K: PartialOrd>(xs: &[Point], key: impl Fn(&Point) -> K) -> Option<usize> {
    let mut best: Option<(usize, K)> = None;
    for (i, x) in xs.iter().enumerate() {
        let k = key(x);
        if best.as_ref().is_none_or(|(_, b)| k < *b) {
            best = Some((i, k));
        }
    }
    best.map(|(i, _)| i)
}

/// Python `max(xs, key=k)`: the first element with the largest key.
fn first_max_by<K: PartialOrd>(xs: &[Point], key: impl Fn(&Point) -> K) -> Option<usize> {
    let mut best: Option<(usize, K)> = None;
    for (i, x) in xs.iter().enumerate() {
        let k = key(x);
        if best.as_ref().is_none_or(|(_, b)| k > *b) {
            best = Some((i, k));
        }
    }
    best.map(|(i, _)| i)
}

/// Fast / Balanced / Long-ctx picks from frontier indices `fast` and `long`, deduplicated.
fn three_picks(frontier: &[Point], fast: usize, long: usize) -> Vec<(&'static str, Point)> {
    let (mut fi, mut li) = (fast, long);
    if fi > li {
        std::mem::swap(&mut fi, &mut li);
    }
    let mut mi = (fi + li) / 2;
    if (mi == fi || mi == li) && li - fi >= 2 {
        mi = fi + 1;
    }
    let mut out: Vec<(&'static str, Point)> = Vec::new();
    let mut seen: Vec<(i128, i128)> = Vec::new();
    for (key, idx) in [("fast", fast), ("balanced", mi), ("long-ctx", long)] {
        let Some(&entry) = frontier.get(idx) else {
            continue;
        };
        let sig = (entry.0, entry.1);
        if seen.contains(&sig) {
            continue;
        }
        seen.push(sig);
        out.push((key, entry));
    }
    out
}

fn label_icon(key: &str) -> (&'static str, &'static str) {
    match key {
        "fast" => ("Fast", "gauge"),
        "balanced" => ("Balanced", "cpu"),
        _ => ("Long context", "layers-3"),
    }
}

/// `_presets_from_dense_frontier`.
fn presets_from_dense_frontier(frontier: &[Point], layers: i128) -> Vec<PresetOption> {
    let (Some(fast), Some(long)) = (
        first_min_by(frontier, |p| p.0),
        first_max_by(frontier, |p| (p.1, -p.0)),
    ) else {
        return Vec::new();
    };
    three_picks(frontier, fast, long)
        .into_iter()
        .map(|(key, (cpu_layers, ctx, gpu_gb, kv_gb))| {
            let gpu_layers = layers - cpu_layers;
            let (label, icon) = label_icon(key);
            PresetOption {
                key: key.to_owned(),
                label: label.to_owned(),
                icon,
                ctx,
                n_cpu_moe: 0,
                offload_kind: if cpu_layers > 0 {
                    Offload::Ngl
                } else {
                    Offload::None
                },
                gpu_layers,
                total_layers: layers,
                gpu_gb: round_nd(gpu_gb, 2),
                kv_gb: round_nd(kv_gb, 2),
                speed_score: dense_speed(layers, cpu_layers),
                ngl: if cpu_layers > 0 { gpu_layers } else { 999 },
            }
        })
        .collect()
}

/// `_presets_from_frontier` (MoE).
fn presets_from_frontier(frontier: &[Point], layers: i128) -> Vec<PresetOption> {
    if frontier.is_empty() {
        return Vec::new();
    }
    // Fast: highest speed (lowest ncm) where ctx meets a chat minimum.
    let fast = if frontier.iter().any(|p| p.1 >= 8192) {
        let mut best: Option<usize> = None;
        for (i, p) in frontier.iter().enumerate() {
            if p.1 >= 8192 && best.and_then(|b| frontier.get(b)).is_none_or(|b| p.0 < b.0) {
                best = Some(i);
            }
        }
        best
    } else {
        first_min_by(frontier, |p| p.0)
    };
    let (Some(fast), Some(long)) = (fast, first_max_by(frontier, |p| (p.1, -p.0))) else {
        return Vec::new();
    };
    three_picks(frontier, fast, long)
        .into_iter()
        .map(|(key, (ncm, ctx, gpu_gb, kv_gb))| {
            let (label, icon) = label_icon(key);
            let speed = if layers > 0 {
                f(layers - ncm) / f(layers)
            } else {
                0.0
            };
            PresetOption {
                key: key.to_owned(),
                label: label.to_owned(),
                icon,
                ctx,
                n_cpu_moe: ncm,
                offload_kind: moe_kind(ncm, layers),
                gpu_layers: layers - ncm,
                total_layers: layers,
                gpu_gb: round_nd(gpu_gb, 2),
                kv_gb: round_nd(kv_gb, 2),
                speed_score: round_nd(speed, 3),
                ngl: -1,
            }
        })
        .collect()
}

/// `cap_context_kind`.
fn cap_context_kind(
    memory_ctx: i128,
    candidates: &[i128],
    prompt_tps: f64,
    prompt_budget_s: f64,
    verified_ctx: i128,
) -> Result<(i128, &'static str), Error> {
    if memory_ctx <= 0 {
        return Ok((memory_ctx, ""));
    }
    let (mut limit, mut reason) = (memory_ctx, "");
    if verified_ctx != 0 && verified_ctx > 0 {
        if verified_ctx < limit {
            (limit, reason) = (verified_ctx, "verified");
        }
    } else if prompt_tps != 0.0 && prompt_tps > 0.0 {
        // max(prompt_budget_s, 1): the budget unless 1 > budget; tps * 1 == tps exactly.
        let product = if 1.0 > prompt_budget_s {
            prompt_tps
        } else {
            prompt_tps * prompt_budget_s
        };
        // None: at least 2^100, never below the limit.
        if let Some(by_time) = trunc_cmp(product)? {
            if by_time < limit {
                (limit, reason) = (by_time, "time");
            }
        }
    } else if UNMEASURED_CTX_CAP < limit {
        (limit, reason) = (UNMEASURED_CTX_CAP, "unmeasured");
    }
    if reason.is_empty() {
        return Ok((memory_ctx, ""));
    }
    let best = candidates
        .iter()
        .copied()
        .filter(|&c| 0 < c && c <= limit)
        .max();
    Ok((best.unwrap_or(limit.min(memory_ctx)), reason))
}

/// Per-card capacities: `[c - reserve for c in cards]`, or an even division of the pool when
/// `fallback(cards)` says the list cannot be used.
fn card_caps(b_cards: &[f64], vram: f64, gpu_count: i128, use_fallback: bool) -> Vec<f64> {
    if use_fallback {
        let each = (vram / f(gpu_count)) - RESERVE_PER_GPU;
        // gpu_count is validated to 1..=MAX_GPUS.
        vec![each; usize::try_from(gpu_count).unwrap_or(0)]
    } else {
        b_cards.iter().map(|c| c - RESERVE_PER_GPU).collect()
    }
}

/// `size_plan`.
pub fn size_plan(req: &Request, work: &mut Work) -> Result<Plan, Error> {
    let layers = req.layers;
    let n_sessions = req.n_sessions;
    let kv_gb_at = |total_ctx: i128, work: &mut Work| -> Result<f64, Error> {
        work.charge(1)?;
        Ok(kv_shape_bytes(
            &req.shape,
            total_ctx,
            BYTES_PER_ELEM_Q8_0,
            BYTES_PER_ELEM_Q8_0,
            work,
        )? / GIB)
    };

    let mut cands: Vec<i128> = CTX_CANDIDATES.to_vec();
    if req.native_ctx != 0 {
        cands.push(req.native_ctx);
    }
    cands.sort_unstable();
    cands.dedup();
    if req.native_ctx != 0 {
        cands.retain(|&c| c <= req.native_ctx);
    }

    // The per-backend fit sweep (`fit_backend`).
    let mut plans: Vec<BackendPlan> = Vec::with_capacity(req.backends.len());
    let mut offload_by_name: Vec<Option<(Offload, i128)>> = vec![None; req.backends.len()];
    for b in &req.backends {
        let gpu_count = b.gpu_count;
        let vram = b.vram_gb;
        let budget = vram - RESERVE_PER_GPU * f(gpu_count) - req.mmproj_vram_gb;
        let caps = card_caps(
            &b.cards,
            vram,
            gpu_count,
            gpu_count > 1 && b.cards.is_empty(),
        );
        let pinned_gb = req.mmproj_vram_gb;
        let overhead_mul = if gpu_count > 1 {
            MODEL_OVERHEAD_SPLIT
        } else {
            MODEL_OVERHEAD_SINGLE
        };
        let model_gb = req.model_gb_raw * overhead_mul;
        let eff_moe = req.moe_ratio;
        let mut max_fit: i128 = 0;
        let mut max_fit_offload = (Offload::None, 0i128);
        let mut rows: Vec<FitRow> = Vec::with_capacity(cands.len());
        for &per_session_ctx in &cands {
            let total_ctx = per_session_ctx * n_sessions;
            let kv_gb = kv_gb_at(total_ctx, work)?;
            let mut card_ok =
                |gpu_gb: f64, kind: Offload, n: i128, work: &mut Work| -> Result<bool, Error> {
                    if gpu_count <= 1 || caps.is_empty() {
                        return Ok(true);
                    }
                    let (att, exp, ncm) = if eff_moe > 0.0 {
                        let att = model_gb * (1.0 - eff_moe);
                        let exp = if layers != 0 {
                            model_gb * eff_moe / f(layers)
                        } else {
                            0.0
                        };
                        let ncm = match kind {
                            Offload::NCpuMoe => n,
                            Offload::CpuMoe => layers,
                            _ => 0,
                        };
                        (att, exp, ncm)
                    } else {
                        (gpu_gb, 0.0, 0)
                    };
                    split_feasible(
                        layers, ncm, att, exp, kv_gb, gpu_count, pinned_gb, &caps, work,
                    )
                };
            let (fits, gpu_model_gb, offload_kind, n_cm) =
                find_fit(model_gb, kv_gb, budget, layers, eff_moe, &mut card_ok, work)?;
            let total = gpu_model_gb + kv_gb;
            let gpu_pct = if model_gb > 0.0 {
                round_int(100.0 * gpu_model_gb / model_gb)?
            } else {
                100
            };
            rows.push(FitRow {
                ctx: per_session_ctx,
                total_ctx,
                model_gb: round_nd(gpu_model_gb, 2),
                kv_gb: round_nd(kv_gb, 2),
                total_gb: round_nd(total, 2),
                fits,
                free_gb: round_nd(vram - total, 2),
                offload_kind,
                n_cpu_moe: n_cm,
                gpu_pct,
            });
            if fits {
                max_fit = per_session_ctx;
                max_fit_offload = (offload_kind, n_cm);
            }
        }
        if max_fit != 0 {
            if let Some(slot) = offload_by_name.get_mut(b.same_as) {
                *slot = Some(max_fit_offload);
            }
        }
        plans.push(BackendPlan {
            rows,
            max_ctx: max_fit,
        });
    }

    // Pick: the largest ctx anyone reaches; the smallest such GPU, unless a MoE needs
    // offload there, then the biggest.
    let mut rec: Option<usize> = None;
    let mut rec_ctx: i128 = 0;
    let mut estimated_ctx: i128 = 0;
    let mut capped_ctx: i128 = 0;
    let mut cap: &'static str = "";
    let fitting: Vec<usize> = (0..plans.len())
        .filter(|&i| plans.get(i).is_some_and(|p| p.max_ctx > 0))
        .collect();
    if let Some(global_max_ctx) = fitting
        .iter()
        .filter_map(|&i| plans.get(i).map(|p| p.max_ctx))
        .max()
    {
        let top: Vec<usize> = fitting
            .iter()
            .copied()
            .filter(|&i| plans.get(i).is_some_and(|p| p.max_ctx >= global_max_ctx))
            .collect();
        let needs_offload_at = |i: usize| {
            plans
                .get(i)
                .and_then(|p| p.rows.iter().find(|r| r.ctx == global_max_ctx))
                .is_some_and(|r| r.offload_kind != Offload::None)
        };
        let vram_of = |i: usize| req.backends.get(i).map_or(0.0, |b| b.vram_gb);
        let biggest = req.is_moe && top.iter().any(|&i| needs_offload_at(i));
        let mut pick: Option<usize> = None;
        for &i in &top {
            let better = match pick {
                None => true,
                Some(p) if biggest => vram_of(i) > vram_of(p),
                Some(p) => vram_of(i) < vram_of(p),
            };
            if better {
                pick = Some(i);
            }
        }
        let r = pick.ok_or(Error::Schema("no backend"))?;
        let plan = plans.get(r).ok_or(Error::Schema("no backend"))?;
        rec = Some(r);
        rec_ctx = plan.max_ctx;
        estimated_ctx = rec_ctx;
        let candidates: Vec<i128> = plan
            .rows
            .iter()
            .map(|row| row.ctx)
            .filter(|&c| c <= plan.max_ctx)
            .collect();
        (rec_ctx, cap) = cap_context_kind(
            rec_ctx,
            &candidates,
            req.prompt_tps,
            req.prompt_budget_s,
            req.verified_ctx,
        )?;
        capped_ctx = rec_ctx;
    }
    let initial_ctx = rec_ctx;

    let mut active_preset = String::new();
    let mut presets: Vec<PresetOption> = Vec::new();
    let mut frontier_opts: Vec<PresetOption> = Vec::new();
    let mut fits_full_gpu = false;
    let mut sized = false;
    let mut ngl: Option<i128> = None;
    let mut fit = false;
    let mut cache_ram: Option<i128> = None;
    let mut offload = (Offload::None, 0i128);

    if let Some(r) = rec {
        let plan = plans.get(r).ok_or(Error::Schema("no backend"))?;
        let rb_index = req.backends.get(r).map(|b| b.same_as).unwrap_or(r);
        offload = offload_by_name
            .get(rb_index)
            .copied()
            .flatten()
            .unwrap_or((Offload::None, 0));
        if rec_ctx > 0 && req.native_ctx > 0 {
            let no_offload_ctx = plan
                .rows
                .iter()
                .filter(|row| row.fits && row.offload_kind == Offload::None)
                .map(|row| row.ctx)
                .max()
                .unwrap_or(0);
            fits_full_gpu = no_offload_ctx >= req.native_ctx;
        }
        if rec_ctx > 0 {
            sized = true;
            let rec_vram = req.backends.get(r).map_or(0.0, |b| b.vram_gb);
            let rb: &Backend = req.backends.get(rb_index).ok_or(Error::Schema("same_as"))?;
            let (mut off_kind, _n_cm) = offload;
            let gpu_count = rb.gpu_count;
            let overhead_mul = if gpu_count > 1 {
                MODEL_OVERHEAD_SPLIT
            } else {
                MODEL_OVERHEAD_SINGLE
            };
            let model_gb_rec = req.model_gb_raw * overhead_mul;
            let mmproj = req.mmproj_vram_gb;
            let moe_ratio = req.moe_ratio;
            if req.is_moe && layers > 0 {
                let budget = rec_vram - RESERVE_PER_GPU * f(gpu_count) - mmproj;
                let mcaps = card_caps(
                    &rb.cards,
                    rec_vram,
                    gpu_count,
                    gpu_count > 1 && rb.cards.len() as i128 != gpu_count,
                );
                let moe_card_ok = |_weight_gb: f64,
                                   kv_gb: f64,
                                   ncm: i128,
                                   work: &mut Work|
                 -> Result<bool, Error> {
                    if gpu_count <= 1 || mcaps.is_empty() {
                        return Ok(true);
                    }
                    let att = model_gb_rec * (1.0 - moe_ratio);
                    let exp = if layers != 0 {
                        model_gb_rec * moe_ratio / f(layers)
                    } else {
                        0.0
                    };
                    split_feasible(
                        layers, ncm, att, exp, kv_gb, gpu_count, mmproj, &mcaps, work,
                    )
                };
                let frontier = pareto_frontier(
                    layers,
                    &kv_gb_at,
                    model_gb_rec,
                    moe_ratio,
                    budget,
                    &cands,
                    n_sessions,
                    &moe_card_ok,
                    work,
                )?;
                presets = presets_from_frontier(&frontier, layers);
                frontier_opts = frontier_options(&frontier, layers, false);
                let middle = presets.len() / 2;
                let chosen = presets
                    .iter()
                    .find(|p| p.key == req.preset)
                    .or_else(|| presets.get(middle));
                if let Some(chosen) = chosen {
                    active_preset = chosen.key.clone();
                    rec_ctx = chosen.ctx;
                    off_kind = chosen.offload_kind;
                }
            } else if !req.is_moe && layers > 0 {
                let budget = rec_vram - RESERVE_PER_GPU * f(gpu_count) - mmproj;
                let fcaps = card_caps(
                    &rb.cards,
                    rec_vram,
                    gpu_count,
                    gpu_count > 1 && rb.cards.len() as i128 != gpu_count,
                );
                let front_card_ok =
                    |weight_gb: f64, kv_gb: f64, work: &mut Work| -> Result<bool, Error> {
                        if gpu_count <= 1 || fcaps.is_empty() {
                            return Ok(true);
                        }
                        split_feasible(
                            layers, 0, weight_gb, 0.0, kv_gb, gpu_count, mmproj, &fcaps, work,
                        )
                    };
                let dfront = dense_frontier(
                    layers,
                    &kv_gb_at,
                    model_gb_rec,
                    budget,
                    &cands,
                    n_sessions,
                    &front_card_ok,
                    work,
                )?;
                presets = presets_from_dense_frontier(&dfront, layers);
                frontier_opts = frontier_options(&dfront, layers, true);
                let want = if req.preset.is_empty() {
                    "fast"
                } else {
                    req.preset.as_str()
                };
                let chosen = presets
                    .iter()
                    .find(|p| p.key == want)
                    .or_else(|| presets.first());
                if let Some(chosen) = chosen {
                    active_preset = chosen.key.clone();
                    rec_ctx = chosen.ctx;
                    if chosen.ngl > 0 {
                        ngl = Some(chosen.ngl);
                    }
                }
            }
            if !cap.is_empty() && rec_ctx > capped_ctx {
                rec_ctx = capped_ctx;
            }
            fit = matches!(off_kind, Offload::CpuMoe | Offload::NCpuMoe);

            let kv_convo_gb = kv_gb_at(rec_ctx * n_sessions, work)?;
            if kv_convo_gb > 0.0 {
                let host_ram_gb = rb.host_ram_gb;
                let cpu_weight_gb = if fit {
                    pyfloat::max(0.0, req.model_gb_raw - rec_vram)
                } else {
                    0.0
                };
                let upper_gb = host_ram_gb - cpu_weight_gb - CACHE_RAM_HEADROOM_GB;
                let want_gb = CACHE_RAM_CONVOS * kv_convo_gb;
                let cache_gb = if upper_gb > 0.0 {
                    pyfloat::min(want_gb, upper_gb)
                } else {
                    0.0
                };
                if cache_gb > 0.0 {
                    let mut mib = round_int(cache_gb * 1024.0)?;
                    if upper_gb * 1024.0 >= f(CACHE_RAM_DEFAULT_MIB) {
                        mib = mib.max(CACHE_RAM_DEFAULT_MIB);
                    }
                    cache_ram = Some(mib.min(req.cache_ram_cap_mib));
                }
            }
        }
    }

    Ok(Plan {
        plans,
        recommended: rec,
        offload,
        estimated_ctx,
        initial_ctx,
        capped_ctx,
        cap,
        sized,
        ctx: rec_ctx,
        presets,
        frontier: frontier_opts,
        active_preset,
        fits_full_gpu,
        ngl,
        fit,
        cache_ram,
    })
}
