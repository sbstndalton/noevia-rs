//! One pure policy for native untuned discovery and Node's bulk runner. No I/O or cache.
//!
//! Version-1 requests have exactly `version`, `op`, and either `row` or `rows`. `signature`
//! returns the version-3 tune signature, `classify` returns eligible row indexes without reading
//! identities, and `scan` returns ordered `models`/`skipped`. Both consumers call these operations;
//! only the existing Node owner gathers raw router, role, file, preset, history and identity facts.
//! A row carries initial `configured`/long/system/missing facts, argv, a profile's defaults/options,
//! Q5 opt-in, latest history, and (only when eligible) the six-field tune identity. Skips preserve
//! long → system → missing file → non-chat → current tune priority. Identity order and WTF-16
//! strings remain intact when hashing Node's exact JSON; only finite canonical preset keys sort.
//! Unknown protocol/identity/option shapes and future tune versions refuse, never imply a current
//! tune. Limits are 4 MiB input, 512 rows, 512 argv values and 2048 encoded bytes per model id.
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
const VERSION: f64 = 3.0;
const KEYS: &[&str] = &[
    "ctx-size",
    "parallel",
    "n-gpu-layers",
    "cache-type-k",
    "cache-type-v",
    "flash-attn",
    "batch-size",
    "ubatch-size",
    "cache-ram",
    "image-max-tokens",
    "spec-type",
    "spec-draft-n-max",
    "spec-draft-p-min",
    "temp",
    "top-p",
    "top-k",
    "min-p",
    "repeat-penalty",
];
fn text(v: &JValue) -> Result<&str, ()> {
    v.as_str().ok_or(())
}
fn flag(v: &JValue) -> Result<bool, ()> {
    v.as_bool().ok_or(())
}
fn hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn obj(v: &JValue) -> Result<&[(String, JValue)], ()> {
    match v {
        JValue::Obj(o) => Ok(o),
        _ => Err(()),
    }
}
fn keys(v: &JValue, allowed: &[&str]) -> Result<(), ()> {
    if obj(v)?.iter().any(|(k, _)| !allowed.contains(&k.as_str())) {
        Err(())
    } else {
        Ok(())
    }
}
fn identity(v: &JValue) -> Result<(), ()> {
    let fields = obj(v)?;
    if fields.len() != 6
        || fields.iter().any(|(k, _)| {
            !["arch", "quant", "artifact", "build", "profile", "hardware"].contains(&k.as_str())
        })
    {
        return Err(());
    }
    for k in ["arch", "quant", "hardware"] {
        if !matches!(v.get(k), JValue::Null | JValue::Str(_)) {
            return Err(());
        }
    }
    if text(v.get("artifact"))?.is_empty()
        || !hex(text(v.get("profile"))?)
        || !matches!(
            v.get("build"),
            JValue::Null | JValue::Str(_) | JValue::Obj(_)
        )
    {
        return Err(());
    }
    Ok(())
}
fn options(profile: &JValue) -> Result<JValue, ()> {
    keys(profile, &["exists", "defaults", "options"])?;
    flag(profile.get("exists"))?;
    let mut pairs: Vec<(String, JValue)> = Vec::new();
    for key in ["defaults", "options"] {
        for (k, v) in obj(profile.get(key))? {
            if !KEYS.contains(&k.as_str()) || v.as_str().is_none() {
                return Err(());
            }
            if let Some(p) = pairs.iter_mut().find(|(name, _)| name == k) {
                p.1 = v.clone();
            } else {
                pairs.push((k.clone(), v.clone()));
            }
        }
    }
    // These recognized keys are lowercase ASCII words/hyphens. Node oracle proves locale order.
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(JValue::Obj(pairs))
}
fn signature_wire(row: &JValue) -> Result<String, ()> {
    keys(
        row,
        &[
            "id",
            "long",
            "system",
            "missing",
            "profile",
            "args",
            "configured",
            "allowQ5Kv",
            "latest",
            "identity",
        ],
    )?;
    identity(row.get("identity"))?;
    let mut payload = vec![
        ("version".into(), JValue::Num(VERSION)),
        ("identity".into(), row.get("identity").clone()),
        ("options".into(), options(row.get("profile"))?),
    ];
    if flag(row.get("allowQ5Kv"))? {
        payload.push(("kv".into(), JValue::Str("q5".into())));
    }
    wtf16::stringify(&JValue::Obj(payload)).ok_or(())
}
fn signature(row: &JValue) -> Result<String, ()> {
    let wire = signature_wire(row)?;
    Ok(format!("{:x}", Sha256::digest(wire.as_bytes())))
}

fn reason(row: &JValue) -> Result<Option<&'static str>, ()> {
    keys(
        row,
        &[
            "id",
            "long",
            "system",
            "missing",
            "profile",
            "args",
            "configured",
            "allowQ5Kv",
            "latest",
            "identity",
        ],
    )?;
    let id = text(row.get("id"))?;
    if id.is_empty() || id.len() > 2048 {
        return Err(());
    }
    if flag(row.get("long"))? {
        return Ok(Some(
            "Long-context profile — tuned with Long mode from its model",
        ));
    }
    if flag(row.get("system"))? {
        return Ok(Some("System routing model — not tuned"));
    }
    if flag(row.get("missing"))? {
        return Ok(Some("Model file missing from the models folder"));
    }
    let profile = row.get("profile");
    let JValue::Arr(args) = row.get("args") else {
        return Err(());
    };
    if args.len() > 512 {
        return Err(());
    }
    let embedding = args.iter().any(|a| {
        matches!(
            a.as_str(),
            Some("--embedding" | "--embeddings" | "--rerank" | "--reranking")
        )
    }) || ["embedding", "embeddings", "rerank", "reranking"]
        .iter()
        .any(|k| {
            matches!(
                profile.get("options").get(k).as_str(),
                Some("true" | "1" | "on")
            )
        });
    if !flag(row.get("configured"))? || embedding {
        return Ok(Some("Not a configured chat model"));
    }
    Ok(None)
}
fn rows(input: &JValue) -> Result<&[JValue], ()> {
    let JValue::Arr(rows) = input.get("rows") else {
        return Err(());
    };
    if rows.len() > 512 {
        return Err(());
    }
    Ok(rows)
}
fn run(input: &JValue) -> Result<JValue, ()> {
    keys(input, &["op", "version", "rows", "row"])?;
    if obj(input)?.len() != 3 {
        return Err(());
    }
    if input.get("version") != &JValue::Num(1.0) {
        return Err(());
    }
    match text(input.get("op"))? {
        "signature" => Ok(JValue::obj([(
            "signature",
            JValue::Str(signature(input.get("row"))?),
        )])),
        "classify" => {
            let mut eligible = Vec::new();
            for (i, row) in rows(input)?.iter().enumerate() {
                if reason(row)?.is_none() {
                    eligible.push(JValue::Num(i as f64));
                }
            }
            Ok(JValue::obj([("eligible", JValue::Arr(eligible))]))
        }
        "scan" => {
            let mut models = Vec::new();
            let mut skipped = Vec::new();
            for row in rows(input)? {
                let mut why = reason(row)?;
                if why.is_none() {
                    let current = signature(row)?;
                    let latest = row.get("latest");
                    if !matches!(latest, JValue::Null) {
                        keys(latest, &["version", "signature"])?;
                        let JValue::Num(v) = latest.get("version") else {
                            return Err(());
                        };
                        if !v.is_finite() || *v < 1.0 || *v > VERSION || v.fract() != 0.0 {
                            return Err(());
                        }
                        let prior = text(latest.get("signature"))?;
                        if !hex(prior) {
                            return Err(());
                        }
                        if *v == VERSION && prior == current {
                            why = Some("Current tune already applied");
                        }
                    }
                }
                if let Some(why) = why {
                    skipped.push(JValue::obj([
                        ("model", row.get("id").clone()),
                        ("reason", JValue::Str(why.into())),
                    ]));
                } else {
                    models.push(row.get("id").clone());
                }
            }
            Ok(JValue::obj([
                ("models", JValue::Arr(models)),
                ("skipped", JValue::Arr(skipped)),
            ]))
        }
        _ => Err(()),
    }
}
/// Existing planner ABI is unchanged; this separate entry has a versioned closed operation set.
pub fn run_json(text: &str) -> (u32, String) {
    if text.len() > MAX_BYTES {
        return (1, "{\"error\":\"too_large\"}".into());
    }
    let result = wtf16::parse(text)
        .map_err(|_| ())
        .and_then(|v| run(&v))
        .and_then(|v| wtf16::stringify(&v).ok_or(()));
    match result {
        Ok(v) => (0, v),
        Err(()) => (1, "{\"error\":\"autotune_scan_unavailable\"}".into()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn cases() -> Vec<JValue> {
        match wtf16::parse(include_str!("../tests/fixtures/untuned-node.json"))
            .expect("immutable Node fixture")
        {
            JValue::Arr(v) => v,
            _ => panic!("cases"),
        }
    }
    fn set(v: &mut JValue, k: &str, value: JValue) {
        let JValue::Obj(fields) = v else {
            panic!("object")
        };
        wtf16::set(fields, k, value);
    }
    #[test]
    fn exact_original_node_json_bytes_hashes_and_ordered_candidate_replies() {
        for case in cases() {
            let input = case.get("input");
            let text = wtf16::stringify(input).expect("input");
            let (status, reply) = run_json(&text);
            assert_eq!(status, 0, "{}", case.get("name").as_str().unwrap_or("case"));
            assert_eq!(wtf16::parse(&reply).expect("reply"), *case.get("expected"));
            if input.get("op").as_str() == Some("signature") {
                assert_eq!(
                    signature_wire(input.get("row")).expect("wire"),
                    case.get("expectedWire")
                        .as_str()
                        .expect("captured exact JSON bytes")
                );
            }
        }
    }
    #[test]
    fn finite_canonical_keys_match_original_nodes_locale_order() {
        let v = wtf16::parse(include_str!(
            "../tests/fixtures/untuned-key-order-node.json"
        ))
        .expect("key fixture");
        let mut keys = KEYS.to_vec();
        keys.sort();
        let expected = JValue::Arr(keys.into_iter().map(|s| JValue::Str(s.into())).collect());
        assert_eq!(*v.get("nodeLocaleOrder"), expected);
        assert_eq!(*v.get("byteOrder"), expected);
    }
    #[test]
    fn unknown_versions_shapes_identity_and_future_history_cannot_claim_current_tunes() {
        let baseline = cases()
            .into_iter()
            .find(|c| c.get("input").get("op").as_str() == Some("signature"))
            .expect("signature case")
            .get("input")
            .clone();
        let mut bad = baseline.clone();
        set(&mut bad, "version", JValue::Num(2.0));
        assert_eq!(run(&bad), Err(()));
        let mut bad = baseline.clone();
        set(&mut bad, "op", JValue::Str("future".into()));
        assert_eq!(run(&bad), Err(()));
        let mut bad = baseline.clone();
        set(&mut bad, "extra", JValue::Bool(true));
        assert_eq!(run(&bad), Err(()));
        for identity in [
            JValue::Null,
            JValue::Arr(vec![]),
            JValue::obj([("future", JValue::Str("secret".into()))]),
        ] {
            let mut bad = baseline.clone();
            let mut row = bad.get("row").clone();
            set(&mut row, "identity", identity);
            set(&mut bad, "row", row);
            assert_eq!(run(&bad), Err(()));
        }
        let scan = cases()
            .into_iter()
            .find(|c| c.get("input").get("op").as_str() == Some("scan"))
            .expect("scan case");
        let JValue::Arr(rows) = scan.get("input").get("rows") else {
            panic!("rows")
        };
        let mut row = rows
            .iter()
            .find(|r| r.get("id").as_str() == Some("new"))
            .expect("eligible")
            .clone();
        set(
            &mut row,
            "latest",
            JValue::obj([
                ("version", JValue::Num(4.0)),
                ("signature", JValue::Str("a".repeat(64))),
            ]),
        );
        assert_eq!(
            run(&JValue::obj([
                ("version", JValue::Num(1.0)),
                ("op", JValue::Str("scan".into())),
                ("rows", JValue::Arr(vec![row.clone()]))
            ])),
            Err(())
        );
        assert_eq!(
            run(&JValue::obj([
                ("version", JValue::Num(1.0)),
                ("op", JValue::Str("scan".into())),
                ("rows", JValue::Arr(vec![row; 513]))
            ])),
            Err(())
        );
    }
}
