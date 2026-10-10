//! Properties of the project-edit-target port: no input panics; never a plan where a direct
//! transcription of project-edit-target.cjs planEdit refuses, and when both plan, the identical
//! plan; a refusal names the JS's first failing rule; large projects stay linear.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use project_edit_target::{call, classify};
use proptest::prelude::*;
use std::time::{Duration, Instant};

const NAMES: &[&str] = &[
    "a.md",
    "b.txt",
    "c.json",
    "d.pdf",
    "e.png",
    "noext",
    ".md",
    "x.MD",
    "caf\u{e9}.md",
    "y.yaml",
];
const FOLDERS: &[Option<&str>] = &[Some("P"), Some("noevia projects/A"), Some(""), None];
const GROUPS: &[&str] = &["Text", "Documents", "Images", "Other", "Text/sub"];
const STATES: &[Option<&str>] = &[Some("ready"), Some("stored"), Some("partial"), None];

#[derive(Clone, Debug)]
struct F {
    name: Option<String>,
    source: Option<String>,
    attachment: Option<(Option<String>, Option<String>)>,
    document: bool,
}

fn s(v: &Option<String>) -> String {
    v.as_ref().map_or("null".into(), |x| format!("{x:?}"))
}

/// project-edit-target.cjs planEditJs for the resolved file, transcribed literally (JS
/// truthiness: `""` and `null` falsy; `classify` from upload-sniff).
fn js(
    pf: &Option<String>,
    rf: &Option<String>,
    connected: bool,
    files: &[F],
    i: usize,
) -> Result<(String, String, bool), &'static str> {
    let truthy = |v: &Option<String>| v.as_deref().filter(|x| !x.is_empty()).map(str::to_string);
    let file = &files[i];
    let name = file.name.clone().unwrap();
    let pf = truthy(pf);
    let src = truthy(&file.source);
    let (mut write, mut target, mut adopt) = (name.clone(), name.clone(), false);
    let upload = pf
        .as_ref()
        .filter(|f| file.attachment.is_some() && file.source.as_ref() == Some(*f));
    if let Some(pfv) = upload {
        let base = name[name.rfind('/').map_or(0, |k| k + 1)..].to_string();
        let up = format!("{pfv}/{}/{base}", upload_sniff::classify(&base));
        if name != up {
            return Err("upload_path");
        }
        write = base;
    } else {
        if src.is_some() {
            return Err("synced");
        }
        if name.contains('/') {
            return Err("path");
        }
        if connected {
            let Some(folder) = pf.or_else(|| truthy(rf)) else {
                return Err("no_folder");
            };
            if upload_sniff::classify(&name) != "Text" {
                return Err("not_text");
            }
            target = format!("{folder}/Text/{name}");
            if files
                .iter()
                .enumerate()
                .any(|(j, f)| j != i && f.name.as_deref() == Some(target.as_str()))
            {
                return Err("taken");
            }
            adopt = true;
        }
    }
    if let Some((state, _)) = &file.attachment {
        if state.as_deref() == Some("stored") {
            return Err("original");
        }
    }
    if file.document {
        return Err("document");
    }
    if let Some((state, _)) = &file.attachment {
        if state.as_deref() != Some("ready") || upload_sniff::classify(&write) != "Text" {
            return Err("partial");
        }
    }
    Ok((write, target, adopt))
}

fn wire(
    pf: &Option<String>,
    rf: &Option<String>,
    connected: bool,
    files: &[F],
    i: usize,
) -> String {
    let names: Vec<String> = files.iter().map(|f| s(&f.name)).collect();
    let f = &files[i];
    let att = f
        .attachment
        .as_ref()
        .map_or("null".into(), |(a, b)| format!("[{},{}]", s(a), s(b)));
    format!(
        "[{},{},{connected},{i},[{}],[{},{att},{}]]",
        s(pf),
        s(rf),
        names.join(","),
        s(&f.source),
        f.document
    )
}

fn file(pf: Option<&'static str>) -> impl Strategy<Value = F> {
    (
        prop::sample::select(NAMES),
        prop::sample::select(GROUPS),
        prop::sample::select(FOLDERS),
        0u8..6,
        prop::option::of((
            prop::sample::select(STATES),
            prop::sample::select(&["Text", "Documents", "Other"][..]),
        )),
        prop::bool::weighted(0.15),
    )
        .prop_map(move |(n, g, f, shape, att, document)| {
            let folder = f.or(pf).unwrap_or("P").to_string();
            let (name, source) = match shape {
                0 | 1 => (format!("{folder}/{g}/{n}"), Some(folder.clone())),
                2 => (format!("{folder}/Text/{n}"), None),
                3 => (n.to_string(), Some("Attached".to_string())),
                _ => (n.to_string(), None),
            };
            F {
                name: Some(name),
                source,
                attachment: att.map(|(s, g)| (s.map(str::to_string), Some(g.to_string()))),
                document,
            }
        })
}

fn project() -> impl Strategy<Value = (Option<String>, Option<String>, bool, Vec<F>, usize)> {
    (
        prop::sample::select(FOLDERS),
        prop::sample::select(FOLDERS),
        any::<bool>(),
    )
        .prop_flat_map(|(pf, rf, c)| {
            prop::collection::vec(file(pf), 1..6).prop_flat_map(move |files| {
                let n = files.len();
                (
                    Just(pf.map(str::to_string)),
                    Just(rf.map(str::to_string)),
                    Just(c),
                    Just(files),
                    0..n,
                )
            })
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn random_bytes_never_panic(body in prop::collection::vec(any::<u8>(), 0..256)) {
        let mut input = vec![1u8];
        input.extend(body);
        let (status, reply) = call(&input);
        prop_assert!(status <= 1 && !reply.is_empty());
    }

    #[test]
    fn never_more_permissive_than_js((pf, rf, connected, files, i) in project()) {
        let w = wire(&pf, &rf, connected, &files, i);
        let mut input = vec![1u8];
        input.extend(w.as_bytes());
        let (status, reply) = call(&input);
        prop_assert_eq!(status, 0, "{}", String::from_utf8_lossy(&reply));
        let reply = String::from_utf8(reply).unwrap();
        match js(&pf, &rf, connected, &files, i) {
            Ok((write, target, adopt)) => prop_assert_eq!(
                reply,
                format!("{{\"plan\":{{\"write\":{write:?},\"target\":{target:?},\"adopt\":{adopt}}}}}")
            ),
            Err(code) => prop_assert_eq!(reply, format!("{{\"refused\":\"{code}\"}}")),
        }
    }
}

#[test]
fn classify_reads_lone_surrogates_as_replacement() {
    assert_eq!(classify(&[0x61, 0x2e, 0x6d, 0x64]), "Text");
    assert_eq!(classify(&[0xd800, 0x2e, 0x6d, 0x64]), "Text");
    assert_eq!(classify(&[0x61, 0x2e, 0xd800]), "Other");
}

#[test]
fn large_projects_stay_linear() {
    // 200k names, each as long as the target: one comparison per name.
    let target = "noevia projects/Alpha/Text/target.md";
    let mut names: Vec<String> = (0..200_000)
        .map(|k| format!("{:?}", format!("noevia projects/Alpha/Text/t{k:07}.md")))
        .collect();
    names.push("\"target.md\"".into());
    let i = names.len() - 1;
    let body = format!(
        "[\"noevia projects/Alpha\",null,true,{i},[{}],[null,null,false]]",
        names.join(",")
    );
    assert!(body.len() < project_edit_target::MAX_INPUT_BYTES);
    let mut input = vec![1u8];
    input.extend(body.as_bytes());
    let start = Instant::now();
    let (status, reply) = call(&input);
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(status, 0);
    assert_eq!(
        String::from_utf8(reply).unwrap(),
        format!("{{\"plan\":{{\"write\":\"target.md\",\"target\":\"{target}\",\"adopt\":true}}}}")
    );
}
