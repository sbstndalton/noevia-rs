//! Read-only adapters for the benchmark observer, separate from telemetry ingest.
use super::*;
impl BackendStats {
    /// Cached sampled usage only, matching hw.stats_for (not a refresh or a true peak).
    pub(crate) fn benchmark_peak(&self, name: &str) -> Option<String> {
        let st = lock(&self.state);
        let Some((_, sample)) = st.cache.iter().find(|(n, _)| n == name) else {
            return Some("[]".into());
        };
        let Some(gpu) = sample.gpu.as_ref() else {
            return Some("[]".into());
        };
        let values: Vec<Num> = if gpu.cards.is_empty() {
            vec![gpu.vram_used_gb]
        } else {
            gpu.cards.iter().map(|c| c.vram_used_gb).collect()
        };
        if values.len() > 256 || values.iter().any(|v| !v.as_f64().is_finite()) {
            return None;
        }
        let mut out = Vec::new();
        for value in values {
            let mut rendered = String::new();
            match value {
                Num::Float(v) => Num::Float(round_nd(v, 2)),
                integer => integer,
            }
            .render(&mut rendered)
            .ok()?;
            out.push(rendered);
        }
        Some(format!("[{}]", out.join(", ")))
    }
    /// Exact read-only Docker log requests; never reuses a different-sized telemetry tail.
    pub(crate) async fn benchmark_logs(&self, name: &str, tail: u16) -> Logs {
        if !matches!(tail, 400 | 800) {
            return Logs::TooLarge;
        }
        let docker = self.docker.as_ref();
        let Some(api) = client(docker).await else {
            return Logs::Skip;
        };
        let Ok(attrs) = inspect(docker, &api, name).await else {
            return Logs::Skip;
        };
        let Some(J::Str(id)) = attrs.get("Id") else {
            return Logs::Skip;
        };
        let tty = match attrs.get("Config") {
            Some(c @ J::Obj(_)) => match c.get("Tty") {
                Some(t) => t.truthy(),
                None => return Logs::Skip,
            },
            _ => return Logs::Skip,
        };
        if !sendable(id) {
            return Logs::Skip;
        }
        let path = format!(
            "/containers/{}/logs?stderr=1&stdout=1&timestamps=1&follow=0&tail={tail}",
            quote(id)
        );
        let raw = match docker.get(&path).await {
            Ok(r) => r,
            Err(crate::docker::EXCEEDED) => return Logs::TooLarge,
            Err(_) => return Logs::Skip,
        };
        if raw.body.len() > crate::bench_observe::LOG_CAP {
            return Logs::TooLarge;
        }
        if !(200..300).contains(&raw.status) {
            return Logs::Skip;
        }
        let bytes = if tty {
            raw.body
        } else {
            demultiplex(&raw.body)
        };
        Logs::Text(String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    #[test]
    fn cached_peak_matches_actual_python_and_never_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let fx = fit::tests::fixture();
        let stats = fit::tests::stats_for(&fx["cases"][0], dir.path());
        let template = fx["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|c| {
                let cache: serde_json::Value = serde_json::from_str(c["cache"].as_str()?).ok()?;
                cache
                    .as_object()?
                    .values()
                    .find(|v| v["gpu"]["cards"].as_array().is_some_and(|c| !c.is_empty()))
                    .cloned()
            })
            .unwrap();
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/bench-observe-python.v1.json"
        ))
        .unwrap();
        for c in oracle["peak"].as_array().unwrap() {
            let mut sample = template.clone();
            if c["aggregate"].is_null() {
                sample["gpu"] = serde_json::Value::Null;
            } else {
                let card = sample["gpu"]["cards"][0].clone();
                sample["gpu"]["vram_used_gb"] = c["aggregate"].clone();
                sample["gpu"]["cards"] = serde_json::Value::Array(
                    c["cards"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| {
                            let mut card = card.clone();
                            card["vram_used_gb"] = v.clone();
                            card
                        })
                        .collect(),
                );
            }
            // Failed samples with retained GPU still have the same usage: Python does not inspect `ok`.
            sample["ok"] = serde_json::Value::Bool(false);
            stats
                .seed_cache_json(&serde_json::json!({"qa-backend":sample}).to_string())
                .unwrap();
            assert_eq!(
                stats.benchmark_peak("qa-backend").unwrap(),
                c["want"].as_str().unwrap()
            );
        }
        assert_eq!(stats.benchmark_peak("qa-not-cached").unwrap(), "[]");
    }
}
