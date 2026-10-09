//! `kv_shape_bytes`: KV cache bytes of a model at a context, from the shape Python resolved.

use crate::pyfloat::f;
use crate::{Error, Work};

pub const SSM_STATE_BYTES: i128 = 4 * 1024 * 1024;

/// `kv_shape()` of autoconfig_core.py: every context-independent quantity, already resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub gemma: bool,
    pub layers: i128,
    pub kv_heads: i128,
    pub k_dim: i128,
    pub v_dim: i128,
    pub hybrid_interval: Option<i128>,
    pub window: Option<i128>,
    pub k_swa: Option<i128>,
    pub v_swa: Option<i128>,
    pub shared: Option<i128>,
    /// One repeating period of (is_local, kv_heads as a float).
    pub period: Option<Vec<(bool, f64)>>,
    /// A hybrid model's fixed recurrent state, sized over its non-attention layers (#1159).
    /// Absent from the JSON unless set.
    pub recurrent_bytes: Option<i128>,
}

/// The bytes as Python's `int(...)` left them, still as a float (an integral one): the caller
/// divides by 1024^3, which is exact scaling either way.
pub fn kv_shape_bytes(
    s: &Shape,
    ctx: i128,
    bytes_per_elem: f64,
    v_bytes: f64,
    work: &mut Work,
) -> Result<f64, Error> {
    if ctx <= 0 {
        return Ok(0.0);
    }
    let base = kv_core(s, ctx, bytes_per_elem, v_bytes, work)?;
    // Python adds the int to the int; the float sum rounds that exact sum once, and the caller's
    // division by 1024^3 is exact scaling, so the GiB figure is the same.
    Ok(match s.recurrent_bytes {
        Some(rec) if rec != 0 => base + f(rec),
        _ => base,
    })
}

fn kv_core(
    s: &Shape,
    ctx: i128,
    bytes_per_elem: f64,
    v_bytes: f64,
    work: &mut Work,
) -> Result<f64, Error> {
    let layers = s.layers;
    let per_layer_per_token = f(s.kv_heads) * (f(s.k_dim) * bytes_per_elem + f(s.v_dim) * v_bytes);

    if let Some(interval) = s.hybrid_interval {
        if interval <= 1 {
            return Err(Error::Schema("shape.hybrid_interval"));
        }
        let full_layers = ((layers + interval - 1).div_euclid(interval)).max(1);
        let ssm_layers = layers - full_layers;
        let kv_full = f(full_layers) * per_layer_per_token * f(ctx);
        return to_bytes(kv_full + f(ssm_layers * SSM_STATE_BYTES));
    }

    if let Some(window_decl) = s.window {
        let (Some(shared), Some(k_swa), Some(v_swa)) = (s.shared, s.k_swa, s.v_swa) else {
            return Err(Error::Schema(
                "shape: a window needs shared, k_swa and v_swa",
            ));
        };
        let alloc_frac = f(layers - shared) / f(layers);
        let per_local_elem = f(k_swa) * bytes_per_elem + f(v_swa) * v_bytes;
        let per_global_elem = f(s.k_dim) * bytes_per_elem + f(s.v_dim) * v_bytes;
        let window = window_decl.min(ctx);
        if let Some(period) = s.period.as_ref().filter(|p| !p.is_empty()) {
            work.charge(period.len())?;
            let reps = (f(layers) / f(period.len() as i128)) * alloc_frac;
            let mut total = 0.0f64;
            for &(is_local, h) in period {
                if is_local {
                    total += reps * h * per_local_elem * f(window);
                } else {
                    total += reps * h * per_global_elem * f(ctx);
                }
            }
            return to_bytes(total);
        }
        let local_layers = 0.max(layers.min(layers - 1.max(layers.div_euclid(6))));
        let global_layers = layers - local_layers;
        return to_bytes(
            alloc_frac
                * (f(global_layers * s.kv_heads) * per_global_elem * f(ctx)
                    + f(local_layers * s.kv_heads) * per_local_elem * f(window)),
        );
    }

    if s.gemma && layers >= 6 {
        let full_layers = 1.max(layers.div_euclid(6));
        let swa_layers = layers - full_layers;
        let kv_full = f(full_layers) * per_layer_per_token * f(ctx);
        let kv_swa = f(swa_layers) * per_layer_per_token * 4096.0;
        return to_bytes(kv_full + kv_swa);
    }

    let Some(shared) = s.shared else {
        return Err(Error::Schema("shape.shared"));
    };
    let effective_layers = 1.max(layers - 0.max(shared.min(layers - 1)));
    to_bytes(f(effective_layers) * per_layer_per_token * f(ctx))
}

fn to_bytes(total: f64) -> Result<f64, Error> {
    // Python's int(total): raises on NaN/infinity. The value itself stays a float.
    if total.is_nan() {
        return Err(Error::Python("ValueError"));
    }
    if total.is_infinite() {
        return Err(Error::Python("OverflowError"));
    }
    // The truncated float is exactly the int Python holds, and int / 1024**3 of it is the same
    // exact power-of-two scaling as the float division the caller does.
    Ok(total.trunc())
}
