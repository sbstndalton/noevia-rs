//! noevia#1318 step 1b: the live fields of `GET /api/v1/models/detail`, `GET /api/v1/models` and
//! `GET /api/v1/overview`, natively.
//!
//! These are the answers the child's private calls gave (`main._model_detail_runtime`,
//! `_models_runtime`, `_overview_runtime`), byte for byte, so the routes splice them exactly as they
//! spliced the child's:
//!
//! - `loaded`: `api._loaded_map()`, every model id a backend's llama-server reports loaded (its
//!   `loaded_model`, split on commas and stripped), with the backends reporting it, in snapshot
//!   order;
//! - `fit`: `services.vram_fit_chips(bytes)` (`fit::chips`), against one inventory per answer;
//! - `backends`: `[_plain(b) for b in services.snapshot_llama_backends()]`, the snapshot
//!   `GET /api/v1/backends` answers without its `stats` and `history`;
//! - `activeDownloads`: this process's own download table (the caller passes its count).
//!
//! A snapshot Python would have ended with an exception is `Fail::Raised` (the route's plain 500),
//! one this side cannot follow exactly `Fail::Refused` (the native 503), as for
//! `GET /api/v1/backends`; an inventory that cannot be judged (HOST_RAM_RESERVE_GB unread) is
//! `Fail::Refused`.
use super::{fit, render_backend, s, Backend, BackendStats, Fail};

/// `api._loaded_map()` as JSON.
fn loaded_json(backends: &[Backend]) -> String {
    let mut map: Vec<(String, Vec<String>)> = Vec::new();
    for b in backends {
        for part in b.loaded_model.as_deref().unwrap_or("").split(',') {
            let id = model_autoconfig::pyval::py_strip(part);
            if id.is_empty() {
                continue;
            }
            match map.iter_mut().find(|(k, _)| k == id) {
                Some((_, names)) => names.push(b.name.clone()),
                None => map.push((id.to_owned(), vec![b.name.clone()])),
            }
        }
    }
    let mut out = String::from("{");
    for (i, (id, names)) in map.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        s(&mut out, id);
        out.push_str(":[");
        for (j, n) in names.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            s(&mut out, n);
        }
        out.push(']');
    }
    out.push('}');
    out
}

impl BackendStats {
    /// `{"loaded": api._loaded_map(), "fit": vram_fit_chips(bytes) if fit else []}` as
    /// `JSONResponse` renders it.
    pub async fn detail_runtime(&self, bytes: u128, fit: bool) -> Result<Vec<u8>, Fail> {
        let loaded = loaded_json(&self.snapshot().await?);
        let chips = if fit {
            let inv = self.inventory().await.ok_or(Fail::Refused)?;
            fit::chips(bytes, &inv, fit::Floats::Raise)?
        } else {
            "[]".to_owned()
        };
        Ok(format!("{{\"loaded\":{loaded},\"fit\":{chips}}}").into_bytes())
    }

    /// `{"loaded": api._loaded_map(), "fit": [vram_fit_chips(b) for b in sizes]}` as the routes
    /// render a dict (pydantic's JSON mode).
    pub async fn models_runtime(&self, sizes: &[u128]) -> Result<Vec<u8>, Fail> {
        let loaded = loaded_json(&self.snapshot().await?);
        let mut out = format!("{{\"loaded\":{loaded},\"fit\":[");
        if !sizes.is_empty() {
            let inv = self.inventory().await.ok_or(Fail::Refused)?;
            for (i, size) in sizes.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&fit::chips(*size, &inv, fit::Floats::Null)?);
            }
        }
        out.push_str("]}");
        Ok(out.into_bytes())
    }

    /// `{"backends": [...], "activeDownloads": active}` as the routes render a dict.
    pub async fn overview_runtime(&self, active: usize) -> Result<Vec<u8>, Fail> {
        let backends = self.snapshot().await?;
        let mut out = String::from("{\"backends\":[");
        for (i, b) in backends.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            render_backend(&mut out, b)?;
            out.push('}');
        }
        out.push_str(&format!("],\"activeDownloads\":{active}}}"));
        Ok(out.into_bytes())
    }
}

#[cfg(test)]
mod tests;
