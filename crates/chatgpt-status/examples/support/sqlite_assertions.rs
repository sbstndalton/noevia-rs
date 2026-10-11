//! Private exact-source assertion certificate; no production policy and no shared classifier.
use serde::Serialize;
use sha2::{Digest, Sha256};
const STDOUT_CAP: usize = 8 * 1024 * 1024;
const STDERR_CAP: usize = 64 * 1024;
#[derive(Clone, Copy)]
pub enum Kind {
    Equal,
    Boolean,
}
#[derive(Clone, Copy)]
pub struct Anchor {
    pub statement: &'static str,
    pub payload: &'static str,
    pub kind: Kind,
}
#[derive(Clone, Serialize)]
pub struct Site {
    path: String,
    line: usize,
    column: usize,
    message: String,
}
#[derive(Serialize)]
pub struct Binding {
    test: String,
    source_sha256: String,
    sites: Vec<Site>,
    filtered: usize,
}
#[derive(Serialize)]
pub struct Witness {
    pub test: String,
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub message: String,
    pub exit: i32,
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn bind(source: &str, path: &str, test: &str, anchors: &[Anchor]) -> Result<Binding, String> {
    if source.is_empty()
        || source.len() > 1048576
        || !matches!(path, "src/sqlite.rs" | "tests/sqlite.rs")
        || test.is_empty()
        || anchors.is_empty()
        || anchors.len() > 2
    {
        return Err("missing/invalid source bindings".into());
    }
    let mut sites = Vec::new();
    for anchor in anchors {
        if source.matches(anchor.statement).count() != 1
            || !anchor.payload.starts_with("ROUTING_ASSERT/sqlite/")
        {
            return Err("missing/nonunique explicit assertion anchor".into());
        }
        let offset = source.find(anchor.statement).ok_or("missing anchor")?;
        let before = source.get(..offset).ok_or("anchor offset")?;
        let owner = before
            .rfind("fn ")
            .and_then(|n| before.get(n..))
            .ok_or("missing assertion owner")?;
        let function = test
            .rsplit("::")
            .next()
            .ok_or("missing selected function")?;
        if !owner.starts_with(&format!("fn {function}(")) {
            return Err("assertion belongs to another test".into());
        }
        let direct = format!("\"{}\"", anchor.payload);
        if !anchor.statement.contains(&direct)
            && !(anchor.payload
                == "ROUTING_ASSERT/sqlite/authorizer UPDATE settings SET value='false'"
                && anchor
                    .statement
                    .contains("\"ROUTING_ASSERT/sqlite/authorizer {statement}\"")
                && owner.contains("\"UPDATE settings SET value='false'\""))
        {
            return Err("source assertion payload mismatch".into());
        }
        let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
        let column = before
            .rsplit('\n')
            .next()
            .ok_or("anchor column")?
            .chars()
            .count()
            + 1;
        if !anchor.statement.starts_with(match anchor.kind {
            Kind::Equal => "assert_eq!(",
            Kind::Boolean => "assert!(",
        }) || !anchor.statement.contains("ROUTING_ASSERT/sqlite/")
        {
            return Err("invalid assertion kind/payload anchor".into());
        }
        let message = match anchor.kind {
            Kind::Equal => format!("assertion `left == right` failed: {}", anchor.payload),
            Kind::Boolean => anchor.payload.to_owned(),
        };
        sites.push(Site {
            path: path.into(),
            line,
            column,
            message,
        });
    }
    if sites.iter().enumerate().any(|(i, s)| {
        sites
            .iter()
            .skip(i + 1)
            .any(|t| s.path == t.path && s.line == t.line && s.column == t.column)
    }) {
        return Err("duplicate assertion sites".into());
    }
    let count = source.matches("#[test]").count();
    if count == 0 || count > 512 {
        return Err("missing/invalid current test inventory".into());
    }
    Ok(Binding {
        test: test.into(),
        source_sha256: hash(source.as_bytes()),
        sites,
        filtered: count - 1,
    })
}
pub fn certify(
    exit: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    binding: &Binding,
    source: &str,
) -> Result<Witness, String> {
    if exit != Some(101) || stdout.len() > STDOUT_CAP || stderr.len() > STDERR_CAP {
        return Err("exit/capture bound mismatch".into());
    }
    if binding.sites.is_empty() || hash(source.as_bytes()) != binding.source_sha256 {
        return Err("current source binding mismatch".into());
    }
    let out = std::str::from_utf8(stdout).map_err(|_| "stdout UTF8")?;
    let err = std::str::from_utf8(stderr).map_err(|_| "stderr UTF8")?;
    if out.contains("panicked at")
        || out.contains("ROUTING_ASSERT/")
        || err.contains("could not compile")
        || err.contains("error[")
        || err.contains("called `Result::unwrap")
        || err.contains("called `Option::unwrap")
    {
        return Err("setup/compiler/unwrap or wrong-stream failure".into());
    }
    let lines: Vec<_> = out.lines().collect();
    let summaries: Vec<_> = lines
        .iter()
        .filter(|l| l.starts_with("test result:"))
        .collect();
    if summaries.len() != 1
        || !summaries.first().is_some_and(|l| {
            l.starts_with("test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; ")
                && l.contains(" filtered out; finished in ")
        })
        || lines.iter().filter(|l| **l == "running 1 test").count() != 1
    {
        return Err("selected test count/footer mismatch".into());
    }
    let footer = summaries
        .first()
        .ok_or("missing footer")?
        .strip_prefix("test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; ")
        .ok_or("footer prefix")?;
    let (filtered, duration) = footer
        .split_once(" filtered out; finished in ")
        .ok_or("footer grammar")?;
    if filtered.parse::<usize>().map_err(|_| "filtered count")? != binding.filtered
        || !duration.strip_suffix('s').is_some_and(|s| {
            s.split_once('.').is_some_and(|(a, b)| {
                !a.is_empty()
                    && !b.is_empty()
                    && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit())
            })
        })
    {
        return Err("current final inventory/duration mismatch".into());
    }
    let tests: Vec<_> = lines
        .iter()
        .filter(|l| l.starts_with("test ") && !l.starts_with("test result:"))
        .collect();
    let selected = format!("test {} ... FAILED", binding.test);
    if tests.as_slice() != [&selected.as_str()] {
        return Err("selected test status inventory mismatch".into());
    }
    let inventories: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| **l == "failures:")
        .collect();
    if !matches!(inventories.len(), 1 | 2) {
        return Err("failed test inventory count mismatch".into());
    }
    if inventories.len() == 2 {
        let first = inventories
            .first()
            .ok_or("missing empty failure section")?
            .0;
        let second = inventories.get(1).ok_or("missing final failure section")?.0;
        if lines
            .iter()
            .skip(first + 1)
            .take(second.saturating_sub(first + 1))
            .any(|l| !l.is_empty())
        {
            return Err("nonempty extra failure section".into());
        }
    }
    let at = inventories.last().ok_or("missing inventory")?.0;
    let failed: Vec<_> = lines
        .iter()
        .skip(at + 1)
        .take_while(|l| !l.starts_with("test result:"))
        .filter(|l| !l.is_empty())
        .copied()
        .collect();
    if failed.as_slice() != [format!("    {}", binding.test)] {
        return Err("failed test inventory mismatch".into());
    }
    if lines.iter().any(|l| {
        !l.is_empty()
            && *l != "running 1 test"
            && *l != selected
            && *l != "failures:"
            && *l != format!("    {}", binding.test)
            && !l.starts_with("test result:")
    }) {
        return Err("unexpected stdout/setup record".into());
    }
    let errors: Vec<_> = err.lines().filter(|l| l.starts_with("error:")).collect();
    if errors.len() != 1
        || !errors
            .first()
            .is_some_and(|l| l.starts_with("error: test failed, to rerun pass "))
    {
        return Err("cargo failure footer mismatch".into());
    }
    let target = if binding
        .sites
        .first()
        .is_some_and(|s| s.path == "src/sqlite.rs")
    {
        "--lib"
    } else {
        "--test sqlite"
    };
    if !errors
        .first()
        .and_then(|s| s.strip_prefix("error: test failed, to rerun pass "))
        .is_some_and(|s| s.trim_matches('`') == target)
    {
        return Err("cargo selected target mismatch".into());
    }
    let panic_lines: Vec<_> = err
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with("thread '"))
        .collect();
    if panic_lines.len() != 1 || err.matches("panicked at").count() != 1 {
        return Err("sole selected panic mismatch".into());
    }
    let (panic_at, header) = *panic_lines.first().ok_or("missing panic")?;
    if err.lines().take(panic_at).any(|l| {
        !l.is_empty()
            && !l.starts_with("   Compiling ")
            && !l.starts_with("    Finished ")
            && !l.starts_with("     Running ")
    }) {
        return Err("unexpected pre-panic setup/compiler/process record".into());
    }
    let rest = header.strip_prefix("thread '").ok_or("panic header")?;
    let (test, tail) = rest.split_once('\'').ok_or("panic test")?;
    if test != binding.test {
        return Err("wrong panic test".into());
    }
    let (thread, location) = tail.split_once(" panicked at ").ok_or("panic location")?;
    if !thread.starts_with(" (")
        || !thread.ends_with(')')
        || !thread
            .get(2..thread.len().saturating_sub(1))
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("panic thread identity".into());
    }
    let location = location
        .strip_suffix(':')
        .ok_or("panic location terminator")?;
    let (prefix, column) = location.rsplit_once(':').ok_or("panic column")?;
    let (path, line) = prefix.rsplit_once(':').ok_or("panic line")?;
    let line = line.parse::<usize>().map_err(|_| "panic line number")?;
    let column = column.parse::<usize>().map_err(|_| "panic column number")?;
    let err_lines: Vec<_> = err.lines().collect();
    let message = *err_lines
        .get(panic_at + 1)
        .ok_or("missing assertion payload")?;
    if !binding
        .sites
        .iter()
        .any(|s| s.path == path && s.line == line && s.column == column && s.message == message)
    {
        return Err("concrete assertion identity mismatch".into());
    }
    if err
        .lines()
        .filter(|l| l.contains("ROUTING_ASSERT/"))
        .count()
        != 1
    {
        return Err("extra/missing assertion payload".into());
    }
    // Rust assertion bodies may carry only comparison values followed by the standard note.
    let body = err_lines
        .iter()
        .skip(panic_at + 2)
        .take_while(|l| !l.starts_with("error:"));
    let mut comparison = Vec::new();
    for l in body {
        if l.is_empty() || l.starts_with("note: run with `RUST_BACKTRACE=1`") {
            continue;
        }
        if l.starts_with("  left: ") || l.starts_with(" right: ") {
            comparison.push(*l);
        } else {
            return Err("unexpected assertion/setup body".into());
        }
    }
    let expected_equal = message.starts_with("assertion `left == right` failed: ");
    if (expected_equal
        && (comparison.len() != 2
            || !comparison
                .first()
                .is_some_and(|l| l.starts_with("  left: "))
            || !comparison.get(1).is_some_and(|l| l.starts_with(" right: "))))
        || (!expected_equal && !comparison.is_empty())
    {
        return Err("assertion body kind mismatch".into());
    }
    Ok(Witness {
        test: test.into(),
        path: path.into(),
        line,
        column,
        message: message.into(),
        exit: 101,
    })
}
