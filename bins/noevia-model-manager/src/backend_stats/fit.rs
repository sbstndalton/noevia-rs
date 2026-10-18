//! noevia#1318 step 1: the fit primitives the model reads judge against, natively.
//!
//! Python (`services._fit_backends`, `services._cpu_backends`, `services.vram_fit_chips`,
//! `hw.host_ram_gb`, `hw.usable_ram_gb`, `hw.vram_gb_for`, `hw.gpu_count_for`,
//! `hw.card_vram_gb_for` and `helpers._backend_list`):
//!
//! - the **inventory**: the effective backends (LLAMA_CONTAINERS, then the discovered llama.cpp
//!   containers) with a VRAM figure (a positive `GPU_VRAM` declaration, else the sampled
//!   `vram_total_gb` of an `ok` sample), and the discovered backends whose image names no GPU
//!   vendor (`cpu` or `unknown`) and whose sample has no VRAM: those run on the CPU;
//! - the **usable RAM**: `MemTotal` of `/proc/meminfo` in GiB (rounded to 0.1) less
//!   HOST_RAM_RESERVE_GB, never below 0, rounded to 0.1; 0.0 when it cannot be read;
//! - the **chips** of one file size: a single machine-level `impossible` chip when the size is past
//!   the largest VRAM plus the usable RAM; else one chip per VRAM backend (`fits` below 65 %,
//!   `tight` below 90 %, else `oom`) then one per CPU backend against the usable RAM (`oom` from
//!   100 %, `slow` from 30 GiB, `fits` below 75 %, else `tight`);
//! - the **descriptors** autoconfig and the search budget read (`_backend_list`).
//!
//! The samples are this process's own sampler cache (the same values it pushes to the child, which
//! `hw.stats_for` answers there); discovery is the sampler's read-only Docker discovery. Python
//! rediscovers the containers up to four times per chip list; this side reads one inventory per
//! answer and judges every size of that answer against it.
use super::{
    client, discover, effective, first_tag_or_empty, image, inspect, lock, BackendStats, Fail, Num,
    Stats, J,
};
use std::path::Path;

/// `hw.host_ram_gb` reads this file.
pub const MEMINFO: &str = "/proc/meminfo";
/// `settings.host_ram_reserve_gb` when HOST_RAM_RESERVE_GB is unset.
pub const DEFAULT_RESERVE_GB: f64 = 32.0;
/// `services._CPU_CRAWL_GB`.
const CPU_CRAWL_GB: f64 = 30.0;
/// `MemTotal` is the first line; a meminfo larger than this is not one.
const MEMINFO_CAP: u64 = 1024 * 1024;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// What one answer's chips are judged against.
#[derive(Debug, Clone, PartialEq)]
pub struct Inventory {
    /// `_fit_backends()`: `(name, VRAM GiB)`, every VRAM positive, in effective order.
    pub gpu: Vec<(String, f64)>,
    /// `_cpu_backends()`, in discovery order.
    pub cpu: Vec<String>,
    /// `hw.usable_ram_gb()`.
    pub ram: f64,
}

/// `hw.host_ram_gb()` of the meminfo at `path`.
pub fn host_ram_gb(path: &Path) -> f64 {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return 0.0;
    };
    let mut raw = Vec::new();
    if file.take(MEMINFO_CAP).read_to_end(&mut raw).is_err() {
        return 0.0;
    }
    // `open(..., encoding="utf-8")`: undecodable text is a ValueError, answered 0.0.
    let Ok(text) = std::str::from_utf8(&raw) else {
        return 0.0;
    };
    // Text mode reads universal newlines: "\r\n", "\r" and "\n" all end a line.
    for line in text.split(['\n', '\r']) {
        if !line.starts_with("MemTotal:") {
            continue;
        }
        // `line.split()[1]`, then `int()`: any failure is answered 0.0.
        let mut words = line
            .split(model_autoconfig::pyval::is_py_space)
            .filter(|w| !w.is_empty());
        let Some(kib) = words.nth(1) else {
            return 0.0;
        };
        let Ok(kib) = model_autoconfig::pyval::int_str(kib, "MemTotal") else {
            return 0.0;
        };
        // `int / 1024 / 1024`: the first division is a correctly rounded true division (exact
        // for a power of two once the integer is a float), the second a float one.
        let gib = model_autoconfig::pyfloat::f(kib) / 1024.0 / 1024.0;
        return model_autoconfig::pyfloat::round_nd(gib, 1);
    }
    0.0
}

/// `hw.usable_ram_gb()`: `round(max(0.0, total - reserve), 1)`, 0.0 without a total.
pub fn usable_ram_gb(total: f64, reserve: f64) -> f64 {
    if total <= 0.0 {
        return 0.0;
    }
    model_autoconfig::pyfloat::round_nd(model_autoconfig::pyfloat::max(0.0, total - reserve), 1)
}

/// HOST_RAM_RESERVE_GB: a strict subset of what pydantic reads as a float, a plain decimal
/// (surrounding ASCII whitespace, sign, digits, one optional fraction). Every other form, even one
/// pydantic would accept, is `None`, and the chips then refuse rather than guess.
pub fn reserve_gb(raw: Option<&str>) -> Option<f64> {
    let Some(raw) = raw else {
        return Some(DEFAULT_RESERVE_GB);
    };
    let t = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if int.is_empty() || !digits(int) || !digits(frac) || (body.contains('.') && frac.is_empty()) {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// `round(x)` as the text of Python's int (exact, however large); `Raised` where Python's
/// `round()` raises (a non-finite value).
fn round_text(x: f64) -> Result<String, Fail> {
    if !x.is_finite() {
        return Err(Fail::Raised);
    }
    let r = x.round_ties_even();
    // `{:.0}` writes the exact integer of an integral float; `+ 0.0` turns -0.0 into 0.
    Ok(format!("{:.0}", r + 0.0))
}

/// How a non-finite float renders: Starlette's `allow_nan=False` raises (the detail runtime's
/// `JSONResponse`); pydantic's JSON mode writes `null` (the routes' `-> dict` rendering).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Floats {
    Raise,
    Null,
}

fn float(out: &mut String, x: f64, mode: Floats) -> Result<(), Fail> {
    if x.is_finite() {
        out.push_str(&gguf::py_float_repr(x));
        return Ok(());
    }
    match mode {
        Floats::Raise => Err(Fail::Raised),
        Floats::Null => {
            out.push_str("null");
            Ok(())
        }
    }
}

fn name(out: &mut String, text: &str) {
    super::s(out, text);
}

/// `services.vram_fit_chips(size_bytes)` as JSON (`[...]`).
pub fn chips(size_bytes: u128, inv: &Inventory, mode: Floats) -> Result<String, Fail> {
    // `size_bytes / (1024 ** 3)`: int true division, correctly rounded (the divisor is a power of
    // two, so rounding the integer to a float first is the same).
    let gb = size_bytes as f64 / GIB;
    let ram = inv.ram;
    // `max(..., default=0.0)` keeps the first maximum.
    let mut pooled: Option<f64> = None;
    for (_, v) in &inv.gpu {
        if *v > 0.0 && pooled.is_none_or(|b| *v > b) {
            pooled = Some(*v);
        }
    }
    let pooled = pooled.unwrap_or(0.0);
    let mut out = String::from("[");
    if ram > 0.0 && pooled > 0.0 && gb > pooled + ram {
        let ceiling = pooled + ram;
        out.push_str("{\"name\":\"this machine\",\"vram_gb\":");
        float(
            &mut out,
            model_autoconfig::pyfloat::round_nd(pooled, 1),
            mode,
        )?;
        out.push_str(",\"host_ram_gb\":");
        float(&mut out, model_autoconfig::pyfloat::round_nd(ram, 1), mode)?;
        out.push_str(",\"verdict\":\"impossible\",\"ratio_pct\":");
        out.push_str(&round_text(gb / ceiling * 100.0)?);
        out.push_str(",\"needs_gb\":");
        out.push_str(&round_text(gb)?);
        out.push_str(",\"ceiling_gb\":");
        out.push_str(&round_text(ceiling)?);
        out.push_str("}]");
        return Ok(out);
    }
    let mut first = true;
    for (backend, vram) in &inv.gpu {
        if *vram <= 0.0 {
            continue;
        }
        let ratio = gb / vram;
        let verdict = if ratio < 0.65 {
            "fits"
        } else if ratio < 0.9 {
            "tight"
        } else {
            "oom"
        };
        if !std::mem::take(&mut first) {
            out.push(',');
        }
        out.push_str("{\"name\":");
        name(&mut out, backend);
        out.push_str(",\"vram_gb\":");
        float(&mut out, *vram, mode)?;
        out.push_str(&format!(",\"verdict\":\"{verdict}\",\"ratio_pct\":"));
        out.push_str(&round_text(ratio * 100.0)?);
        out.push('}');
    }
    for backend in &inv.cpu {
        if ram <= 0.0 {
            continue;
        }
        let ratio = gb / ram;
        let verdict = if ratio >= 1.0 {
            "oom"
        } else if gb >= CPU_CRAWL_GB {
            "slow"
        } else if ratio < 0.75 {
            "fits"
        } else {
            "tight"
        };
        if !std::mem::take(&mut first) {
            out.push(',');
        }
        out.push_str("{\"name\":");
        name(&mut out, backend);
        out.push_str(",\"vram_gb\":");
        float(&mut out, ram, mode)?;
        out.push_str(&format!(",\"verdict\":\"{verdict}\",\"ratio_pct\":"));
        out.push_str(&round_text(ratio * 100.0)?);
        out.push_str(",\"is_cpu\":true}");
    }
    out.push(']');
    Ok(out)
}

/// `stats.ok and stats.gpu and stats.gpu.vram_total_gb > 0`: that VRAM as `float()`.
fn sampled_vram(stats: Option<&Stats>) -> Option<f64> {
    let gpu = stats.filter(|s| s.ok)?.gpu.as_ref()?;
    match gpu.vram_total_gb {
        Num::Int(i) if i > 0 => Some(model_autoconfig::pyfloat::f(i)),
        Num::Float(f) if f > 0.0 => Some(f),
        _ => None,
    }
}

/// One `helpers._backend_list()` entry, without `baseline` (`autoconfig.parse_baseline` of
/// `baseline_args`, which its reader computes).
#[derive(Debug, Clone, PartialEq)]
pub struct Descriptor {
    pub name: String,
    /// `rocm`, `cuda` or `unknown`, from the first image tag.
    pub vendor: &'static str,
    pub vram_gb: f64,
    pub gpu_count: i128,
    pub card_vram_gb: Vec<Num>,
    pub host_ram_gb: f64,
    pub baseline_args: Vec<String>,
}

/// `max(float(b.get("vram_gb") or 0) for b in descriptors)`, 0.0 without one.
pub fn budget(descriptors: &[Descriptor]) -> f64 {
    let mut best: Option<f64> = None;
    for d in descriptors {
        if best.is_none_or(|b| d.vram_gb > b) {
            best = Some(d.vram_gb);
        }
    }
    best.unwrap_or(0.0)
}

/// A descriptor as `_plain` would write it (for the differential tests and the readers that
/// splice it), `baseline` left out.
pub fn render_descriptor(out: &mut String, d: &Descriptor, mode: Floats) -> Result<(), Fail> {
    out.push_str("{\"name\":");
    name(out, &d.name);
    out.push_str(&format!(",\"vendor\":\"{}\",\"vram_gb\":", d.vendor));
    float(out, d.vram_gb, mode)?;
    out.push_str(&format!(
        ",\"gpu_count\":{},\"card_vram_gb\":[",
        d.gpu_count
    ));
    for (i, v) in d.card_vram_gb.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match v {
            Num::Int(n) => out.push_str(&n.to_string()),
            Num::Float(f) => float(out, *f, mode)?,
        }
    }
    out.push_str("],\"host_ram_gb\":");
    float(out, d.host_ram_gb, mode)?;
    out.push_str(",\"baseline_args\":[");
    for (i, a) in d.baseline_args.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        name(out, a);
    }
    out.push_str("]}");
    Ok(())
}

/// `_container_baseline(name)`'s `list(Config.Cmd or [])`: `None` when the command holds
/// something other than strings (this side does not render those).
fn baseline_args(attrs: &J) -> Option<Vec<String>> {
    let cmd = match attrs.get("Config") {
        Some(c @ J::Obj(_)) => c.get("Cmd"),
        // `.get("Config", {})`: a missing key is `{}`; any other value fails `.get` and the
        // `except` answers `[]`.
        _ => return Some(Vec::new()),
    };
    match cmd {
        None => Some(Vec::new()),
        Some(v) if !v.truthy() => Some(Vec::new()),
        Some(J::Arr(items)) => items
            .iter()
            .map(|i| match i {
                J::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        // `list("text")` is its characters.
        Some(J::Str(s)) => Some(s.chars().map(String::from).collect()),
        // `list(dict)` is its keys.
        Some(J::Obj(pairs)) => {
            let mut keys: Vec<String> = Vec::new();
            for (k, _) in pairs {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
            Some(keys)
        }
        // `list(<number or bool>)` raises TypeError: `[]`.
        Some(_) => Some(Vec::new()),
    }
}

impl BackendStats {
    fn cached(&self, name: &str) -> Option<Stats> {
        lock(&self.state)
            .cache
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }

    /// `hw.usable_ram_gb()`; `None` when HOST_RAM_RESERVE_GB was not a value this side reads.
    pub fn usable_ram(&self) -> Option<f64> {
        let reserve = self.config.ram_reserve_gb?;
        Some(usable_ram_gb(host_ram_gb(&self.config.meminfo), reserve))
    }

    /// The fit inventory of one answer; `None` when the usable RAM cannot be judged exactly.
    pub async fn inventory(&self) -> Option<Inventory> {
        let ram = self.usable_ram()?;
        let discovered = discover(self.docker.as_ref()).await;
        let names = effective(&self.config.configured, &discovered);
        let mut gpu = Vec::new();
        for n in &names {
            let declared = self.declared(n);
            let vram = if declared > 0.0 {
                declared
            } else {
                sampled_vram(self.cached(n).as_ref()).unwrap_or(0.0)
            };
            if vram > 0.0 {
                gpu.push((n.clone(), vram));
            }
        }
        let mut cpu = Vec::new();
        for (n, tag) in &discovered {
            let vendor = if tag.contains("rocm") {
                "rocm"
            } else if tag.contains("cuda") {
                "cuda"
            } else if tag.contains("server") {
                "cpu"
            } else {
                "unknown"
            };
            if !n.is_empty()
                && names.contains(n)
                && matches!(vendor, "cpu" | "unknown")
                && sampled_vram(self.cached(n).as_ref()).is_none()
            {
                cpu.push(n.clone());
            }
        }
        Some(Inventory { gpu, cpu, ram })
    }

    /// `helpers._backend_list()` without `baseline`; `Err(Refused)` for a container command this
    /// side does not render.
    pub async fn descriptors(&self) -> Result<Vec<Descriptor>, Fail> {
        let docker = self.docker.as_ref();
        let discovered = discover(docker).await;
        let names = effective(&self.config.configured, &discovered);
        let mut out = Vec::new();
        for n in &names {
            let stats = self.cached(n);
            // `hw.vram_gb_for`: a declaration wins whatever its value.
            let vram = match self.config.gpu_vram.iter().find(|(k, _)| k == n) {
                Some((_, v)) => *v,
                None => sampled_vram(stats.as_ref()).unwrap_or(0.0),
            };
            if vram <= 0.0 {
                continue;
            }
            let api = client(docker).await;
            let attrs = match &api {
                Some(api) => inspect(docker, api, n).await.ok(),
                None => None,
            };
            let mut vendor = "unknown";
            if let (Some(api), Some(attrs)) = (&api, &attrs) {
                if let Ok(img) = image(docker, api, attrs).await {
                    if let Some(tag) = first_tag_or_empty(&img) {
                        let tag = tag.to_lowercase();
                        if tag.contains("rocm") {
                            vendor = "rocm";
                        } else if tag.contains("cuda") {
                            vendor = "cuda";
                        }
                    }
                }
            }
            let baseline_args = match &attrs {
                Some(a) => baseline_args(a).ok_or(Fail::Refused)?,
                None => Vec::new(),
            };
            let gpu = stats.as_ref().filter(|s| s.ok).and_then(|s| s.gpu.as_ref());
            out.push(Descriptor {
                name: n.clone(),
                vendor,
                vram_gb: vram,
                gpu_count: gpu.map(|g| g.gpu_count).filter(|c| *c > 0).unwrap_or(1),
                card_vram_gb: gpu
                    .map(|g| g.cards.iter().map(|c| c.vram_total_gb).collect())
                    .unwrap_or_default(),
                host_ram_gb: host_ram_gb(&self.config.meminfo),
                baseline_args,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
