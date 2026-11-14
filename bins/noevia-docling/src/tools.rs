//! The image's helper commands, so the docling image needs no Python outside the engine itself.
//! Only these exact first arguments divert from starting the server; any other start is unchanged.
//!
//! - `healthcheck [URL]`: the HEALTHCHECK probe (docling_service::health), default
//!   `http://127.0.0.1:$DOCLING_PORT/health` (8031 unset), 5 s per socket operation. Exit 0 healthy,
//!   1 not (one line on stderr), 2 usage.
//! - `synthetic-pdfs [DIR]`: the #700 fixtures services `docling/synthetic_pdfs.py` wrote when run
//!   directly (synthetic-picture-text.pdf, synthetic-text.pdf, synthetic-blank.pdf) into DIR
//!   (default: the current directory), for pointing selftest.py at a known page.
//! - `synthetic-pdf OUT SPEC...`: one document, a page per SPEC (`KIND`, `KIND:START`,
//!   `KIND:START:COUNT` or `KIND=TEXT`; docling_service::synthetic::parse_page).
use docling_service::{health, synthetic};
use std::{ffi::OsString, io::Write, path::Path};

pub const USAGE: &str =
    "usage: noevia-docling [healthcheck [URL] | synthetic-pdfs [DIR] | synthetic-pdf OUT SPEC...]";

/// The exit code when `args` (without the program name) name a helper command; None = start the
/// server as before.
pub fn dispatch(args: &[OsString], port: Option<OsString>, stderr: &mut dyn Write) -> Option<i32> {
    let (first, rest) = args.split_first()?;
    let rest: Vec<String> = match rest.iter().map(|a| a.clone().into_string()).collect() {
        Ok(rest) => rest,
        Err(_) => {
            if matches!(
                first.to_str(),
                Some("healthcheck" | "synthetic-pdfs" | "synthetic-pdf")
            ) {
                let _ = writeln!(stderr, "{USAGE}");
                return Some(2);
            }
            return None;
        }
    };
    match first.to_str()? {
        "healthcheck" => Some(healthcheck(&rest, port, stderr)),
        "synthetic-pdfs" => Some(default_set(&rest, stderr)),
        "synthetic-pdf" => Some(one_document(&rest, stderr)),
        _ => None,
    }
}

fn healthcheck(rest: &[String], port: Option<OsString>, stderr: &mut dyn Write) -> i32 {
    let url = match rest {
        [] => {
            let port = port.map(|p| p.to_string_lossy().into_owned());
            match health::default_url(port.as_deref()) {
                Ok(url) => url,
                Err(error) => {
                    let _ = writeln!(stderr, "noevia-docling healthcheck: {error}");
                    return 1;
                }
            }
        }
        [url] => url.clone(),
        _ => {
            let _ = writeln!(stderr, "{USAGE}");
            return 2;
        }
    };
    match health::probe(&url, health::TIMEOUT) {
        Ok(_) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "noevia-docling healthcheck: {error}");
            1
        }
    }
}

fn default_set(rest: &[String], stderr: &mut dyn Write) -> i32 {
    let directory = match rest {
        [] => ".",
        [directory] => directory.as_str(),
        _ => {
            let _ = writeln!(stderr, "{USAGE}");
            return 2;
        }
    };
    match synthetic::write_default_set(Path::new(directory)) {
        Ok(_) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "noevia-docling synthetic-pdfs: {error}");
            1
        }
    }
}

fn one_document(rest: &[String], stderr: &mut dyn Write) -> i32 {
    let Some((out, specs)) = rest.split_first().filter(|(_, specs)| !specs.is_empty()) else {
        let _ = writeln!(stderr, "{USAGE}");
        return 2;
    };
    let mut pages = Vec::with_capacity(specs.len());
    for spec in specs {
        match synthetic::parse_page(spec) {
            Some(page) => pages.push(page),
            None => {
                let _ = writeln!(
                    stderr,
                    "noevia-docling synthetic-pdf: bad page spec {spec:?}"
                );
                return 2;
            }
        }
    }
    match synthetic::write(Path::new(out), &pages) {
        Ok(_) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "noevia-docling synthetic-pdf: {error}");
            1
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn run(args: &[&str], port: Option<&str>) -> (Option<i32>, String) {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut err = Vec::new();
        let code = dispatch(&args, port.map(OsString::from), &mut err);
        (code, String::from_utf8(err).unwrap())
    }

    #[test]
    fn other_starts_are_the_server() {
        for args in [
            &[][..],
            &["serve"],
            &["--help"],
            &["Healthcheck"],
            &["healthcheck-x"],
        ] {
            assert_eq!(run(args, None).0, None, "{args:?}");
        }
    }

    #[test]
    fn usage_errors_exit_2() {
        for args in [
            &["healthcheck", "a", "b"][..],
            &["synthetic-pdfs", "a", "b"],
            &["synthetic-pdf"],
            &["synthetic-pdf", "out.pdf"],
            &["synthetic-pdf", "out.pdf", "text:x"],
        ] {
            let (code, err) = run(args, None);
            assert_eq!(code, Some(2), "{args:?}");
            assert!(!err.is_empty());
        }
    }

    #[test]
    fn non_unicode_helper_arguments_are_usage_errors() {
        use std::os::unix::ffi::OsStringExt;
        let args = vec![
            OsString::from("synthetic-pdfs"),
            OsString::from_vec(vec![0xff]),
        ];
        assert_eq!(dispatch(&args, None, &mut Vec::new()), Some(2));
        let args = vec![OsString::from("serve"), OsString::from_vec(vec![0xff])];
        assert_eq!(dispatch(&args, None, &mut Vec::new()), None);
    }

    #[test]
    fn bad_port_is_unhealthy_not_a_probe_elsewhere() {
        let (code, err) = run(&["healthcheck"], Some("not-a-port"));
        assert_eq!(code, Some(1));
        assert!(err.contains("DOCLING_PORT"), "{err}");
    }

    #[test]
    fn writes_the_default_set_and_one_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        assert_eq!(run(&["synthetic-pdfs", path], None).0, Some(0));
        for (name, kind) in synthetic::DEFAULT_SET {
            let lines = if kind == "blank" {
                Vec::new()
            } else {
                synthetic::default_lines()
            };
            let expected = synthetic::build(&[synthetic::Page::new(kind, lines)]).unwrap();
            assert_eq!(
                std::fs::read(dir.path().join(name)).unwrap(),
                expected,
                "{name}"
            );
        }
        let out = dir.path().join("mixed.pdf");
        let code = run(
            &[
                "synthetic-pdf",
                out.to_str().unwrap(),
                "text",
                "picture-text:20",
                "blank",
            ],
            None,
        )
        .0;
        assert_eq!(code, Some(0));
        let expected = synthetic::build(&[
            synthetic::Page::new("text", synthetic::default_lines()),
            synthetic::Page::new("picture-text", synthetic::page_lines(12, 20)),
            synthetic::Page::new("blank", Vec::new()),
        ])
        .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), expected);
        let (code, err) = run(
            &["synthetic-pdf", out.to_str().unwrap(), "text=\u{2014}"],
            None,
        );
        assert_eq!(code, Some(1));
        assert!(err.contains("Latin-1"), "{err}");
        let missing = dir.path().join("no/such/dir");
        assert_eq!(
            run(&["synthetic-pdfs", missing.to_str().unwrap()], None).0,
            Some(1)
        );
    }
}
