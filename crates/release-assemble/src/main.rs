//! `release-assemble`: the Rust slices of assemble-release.sh as subcommands (inert: nothing calls it).
//!
//! ```text
//! release-assemble lock-value LOCK KEY            value of the last KEY= line (lock_value)
//! release-assemble plan LOCK SHA                  web/core/services/rs resolution, or the die text
//! release-assemble check-sha256 FILE REPO REF SUM the fetch_remote checksum check
//! release-assemble extract TGZ DEST [STRIP]       tar -xzf TGZ --strip-components=STRIP -C DEST (fail closed)
//! release-assemble finish TREE SHA OUT_DIR WEB CORE SERVICES [RS]
//!                                                 required entries, release-refs, ui-data prune,
//!                                                 .dockerignore, deterministic tarball
//! release-assemble manifest TGZ                   content manifest (compare two release tarballs)
//! release-assemble routes-toml FILE               the pairing guards' routes.toml records (routes_toml);
//!                                                 a refused file prints `line N: <reason>` and exits 2
//! release-assemble move-capability FILE NAME VALUE KIND
//!                                                 exit 0 when the marker is declared (move_capability)
//! release-assemble project-routes-core FILE [NAME] a Core module's OWNED_ROUTES (project_routes_core)
//! release-assemble core-requires FILE MODULE     yes, no or invalid: does the Core module call
//!                                                 require(MODULE) (core_requires, without node)
//! release-assemble features-version READER FILE none, invalid or the version a strict FEATURES reader gives
//!                                                 for the front's lib.rs (features_reader::ALL by CLI or
//!                                                 shell function name, e.g. discovery, lease_rs_version)
//! release-assemble code-body FILE                 the code lines a pairing reads, as "$(code_body FILE)"
//!                                                 gives them (trailing blank lines dropped) plus one newline
//! release-assemble pairing NAME TREE              one ownership pairing over an assembled tree; NAME is
//!                                                 feature, model-reads, project-reads, project-lifecycle,
//!                                                 project-files, project-config, code-egress,
//!                                                 calibration-cancel, calibration-routes, research
//!                                                 task-reads, model-scan, document-pages,
//!                                                 conversation-import, project-documents,
//!                                                 public-pages, plugin-directory, model-writes,
//!                                                 model-manager, embedding-admin, or one of the facts
//!                                                 pairings (facts::ALL: autotune-settings,
//!                                                 idle-unload-read, model-assignments, preset-read,
//!                                                 autotune-clear, autotune-resume, autotune-cancel,
//!                                                 autotune-untuned), or one of the route pairings
//!                                                 (route_pairings::NAMES: project-storage,
//!                                                 tool-approvals, framing-suggest, toolbox-picker,
//!                                                 close-out,
//!                                                 decision-settings, storage-integrations,
//!                                                 vault-mirror, storage-connection, providers,
//!                                                 chatgpt-signin, mcp-credentials, mcp-views,
//!                                                 provider-models, google-drive, offsite-backup,
//!                                                 model-sizing, model-state-reads, model-evidence)
//! release-assemble pairing NAME TREE VERDICTS     a route pairing with earlier verdicts the script has by
//!                                                 then and this crate does not port yet: a comma list
//!                                                 of auto-roles-write (close-out) and discovery
//!                                                 (toolbox-picker, mcp-views), or empty
//! ```
//! Refusals print `assemble-release: <reason>` on stderr and exit 1; bad usage exits 2.

use release_assemble::guard::{self, MoveKind};
use release_assemble::lock::Lock;
use release_assemble::plan::{self, Component, Resolution, Resolved};
use release_assemble::{
    bridged, core_requires, digest, facts, features_reader, model_pairings, route_pairings, tar,
    tree,
};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: release-assemble lock-value LOCK KEY | plan LOCK SHA | check-sha256 FILE REPO REF SUM | extract TGZ DEST [STRIP] | finish TREE SHA OUT_DIR WEB CORE SERVICES [RS] | manifest TGZ | routes-toml FILE | move-capability FILE NAME VALUE KIND | project-routes-core FILE [NAME] | core-requires FILE MODULE | features-version READER FILE | code-body FILE | pairing NAME TREE [VERDICTS]";

fn write_out(bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}

fn write_lines(lines: &[Vec<u8>]) -> Result<(), String> {
    let mut all = Vec::new();
    for l in lines {
        all.extend_from_slice(l);
        all.push(b'\n');
    }
    write_out(&all)
}

fn die(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("assemble-release: {msg}");
    ExitCode::from(1)
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn read_lock(path: &str) -> Result<Lock, String> {
    std::fs::read(path)
        .map(|b| Lock::from_bytes(&b))
        .map_err(|_| format!("--lock file not found: {path}"))
}

fn open_tgz(path: &str) -> Result<Vec<tar::Entry>, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    tar::read_gz(&mut f).map_err(|e| format!("{path}: {e}"))
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["lock-value", lock, key] => {
            let v = read_lock(lock)?.value(key);
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            out.write_all(&v)
                .and_then(|()| out.write_all(b"\n"))
                .map_err(|e| e.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        ["plan", lock, sha] => {
            if !release_assemble::lock::release_sha(sha) {
                return Ok(usage());
            }
            let lock = read_lock(lock)?;
            for c in Component::ALL {
                match plan::resolve(&lock, c, sha)? {
                    Resolution::SelfExport => println!("{}\tself", c.dir()),
                    Resolution::Pinned { reference, sha256 } => {
                        println!("{}\t{reference}\t{sha256}", c.dir())
                    }
                }
            }
            match plan::resolve_rs(&lock, sha)? {
                None => println!("rs\tnone"),
                Some((reference, sum)) => println!("rs\t{reference}\t{sum}"),
            }
            Ok(ExitCode::SUCCESS)
        }
        ["check-sha256", file, repo, reference, sum] => {
            digest::check_tarball(Path::new(file), repo, reference, sum)?;
            Ok(ExitCode::SUCCESS)
        }
        ["extract", tgz, dest, rest @ ..] => {
            let strip = match rest {
                [] => 0,
                [s] => match s.parse() {
                    Ok(n) => n,
                    Err(_) => return Ok(usage()),
                },
                _ => return Ok(usage()),
            };
            let entries = open_tgz(tgz)?;
            tar::extract(&entries, strip, Path::new(dest)).map_err(|e| e.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        ["finish", tree_dir, sha, out_dir, web, core, services, rest @ ..] => {
            let rs = match rest {
                [] => String::new(),
                [r] => (*r).to_string(),
                _ => return Ok(usage()),
            };
            let resolved = Resolved {
                web: (*web).into(),
                core: (*core).into(),
                services: (*services).into(),
                rs,
            };
            let mtime = std::env::var("SOURCE_DATE_EPOCH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let out = tree::finish(
                Path::new(tree_dir),
                sha,
                &resolved,
                Path::new(out_dir),
                mtime,
            )?;
            println!("ok: {}", out.display());
            Ok(ExitCode::SUCCESS)
        }
        ["manifest", tgz] => {
            for line in tree::manifest(&open_tgz(tgz)?) {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        ["routes-toml", file] => {
            let rt = guard::routes_toml_file(Path::new(file));
            write_out(&rt.stdout())?;
            match rt.error {
                None => Ok(ExitCode::SUCCESS),
                Some(e) => {
                    eprintln!("{e}");
                    Ok(ExitCode::from(2))
                }
            }
        }
        ["move-capability", file, name, value, kind] => {
            let Some(kind) = MoveKind::parse(kind) else {
                return Ok(usage());
            };
            let src = guard::read_file(Path::new(file));
            Ok(
                if guard::move_capability(src.as_deref(), name, value, kind) {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                },
            )
        }
        ["project-routes-core", file, rest @ ..] => {
            let name = match rest {
                [] | [""] => "OWNED_ROUTES",
                [n] => *n,
                _ => return Ok(usage()),
            };
            let src = guard::read_file(Path::new(file));
            write_lines(&guard::project_routes_core(src.as_deref(), name))?;
            Ok(ExitCode::SUCCESS)
        }
        ["core-requires", file, module] => {
            let verdict = core_requires::core_requires_file(Path::new(file), module);
            write_out(verdict.word().as_bytes())?;
            Ok(ExitCode::SUCCESS)
        }
        ["features-version", reader, file] => {
            let Some(reader) = features_reader::Reader::by_name(reader) else {
                return Ok(usage());
            };
            write_out(features_reader::read(reader, Path::new(file)).as_bytes())?;
            Ok(ExitCode::SUCCESS)
        }
        ["code-body", file] => {
            let mut body = model_pairings::code_body(guard::read_file(Path::new(file)).as_deref());
            if !body.is_empty() {
                body.push(b'\n');
            }
            write_out(&body)?;
            Ok(ExitCode::SUCCESS)
        }
        ["pairing", which, tree_dir] => {
            let t = Path::new(tree_dir);
            match *which {
                "feature" => guard::namespace_pairing(t, &guard::FEATURE_OWNERSHIP)?,
                "model-reads" => guard::namespace_pairing(t, &guard::MODEL_READ_OWNERSHIP)?,
                "project-reads" => guard::namespace_pairing(t, &guard::PROJECT_READ_OWNERSHIP)?,
                "project-lifecycle" => {
                    guard::namespace_pairing(t, &guard::PROJECT_LIFECYCLE_OWNERSHIP)?
                }
                // The storage pair runs first in the script (Core and the front agree by then), so its
                // front half is its verdict.
                "project-files" => {
                    guard::project_files_pairing(t, guard::project_storage_rs_owner(t))?
                }
                "project-config" => guard::project_config_pairing(t)?,
                "document-pages" => {
                    guard::namespace_pairing(t, &guard::PROJECT_DOCUMENT_PAGES_OWNERSHIP)?
                }
                // The create/delete pair runs first in the script, so its front half is its verdict.
                "conversation-import" => {
                    guard::conversation_import_pairing(t, guard::project_lifecycle_rs_owner(t))?
                }
                "project-documents" => guard::project_documents_pairing(t)?,
                "public-pages" => guard::namespace_pairing(t, &guard::PUBLIC_PAGES_OWNERSHIP)?,
                "plugin-directory" => {
                    guard::namespace_pairing(t, &guard::PLUGIN_DIRECTORY_OWNERSHIP)?
                }
                "code-egress" => bridged::code_egress_pairing(t)?,
                "research" => bridged::research_pairing(t)?,
                "task-reads" => bridged::task_reads_pairing(t)?,
                "model-scan" => bridged::model_scan_pairing(t)?,
                "calibration-cancel" => {
                    bridged::calibration_pairing(t, &bridged::CALIBRATION_CANCEL)?
                }
                "calibration-routes" => {
                    bridged::calibration_pairing(t, &bridged::CALIBRATION_ROUTES)?
                }
                "model-writes" => model_pairings::model_writes_pairing(t)?,
                "model-manager" => model_pairings::model_manager_pairing(t)?,
                "embedding-admin" => model_pairings::embedding_admin_pairing(t)?,
                other => match facts::by_name(other) {
                    Some(p) => facts::facts_pairing(t, p)?,
                    None => match route_pairings::by_name(t, other, earlier(t, "")) {
                        Some(verdict) => verdict?,
                        None => return Ok(usage()),
                    },
                },
            }
            Ok(ExitCode::SUCCESS)
        }
        // The route pairings that take verdicts of pairings not ported yet: VERDICTS is a comma list of
        // auto-roles-write (auto_roles_write_owner = rust) and discovery (md_rs_owner = rust), or empty.
        ["pairing", which, tree_dir, verdicts] => {
            let t = Path::new(tree_dir);
            if verdicts
                .split(',')
                .any(|v| !v.is_empty() && v != "auto-roles-write" && v != "discovery")
            {
                return Ok(usage());
            }
            match route_pairings::by_name(t, which, earlier(t, verdicts)) {
                Some(verdict) => verdict?,
                None => return Ok(usage()),
            }
            Ok(ExitCode::SUCCESS)
        }
        _ => Ok(usage()),
    }
}

/// The earlier verdicts a route pairing takes: the project create/delete pair's front half (that pairing runs
/// first in the script, so its front half is its verdict), and the two named in `verdicts`.
fn earlier(t: &Path, verdicts: &str) -> route_pairings::Earlier {
    let named = |n: &str| verdicts.split(',').any(|v| v == n);
    route_pairings::Earlier {
        lifecycle_rs_owner: guard::project_lifecycle_rs_owner(t),
        auto_roles_write: named("auto-roles-write"),
        discovery_rs_owner: named("discovery"),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(msg) => die(msg),
    }
}
