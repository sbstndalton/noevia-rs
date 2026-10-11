//! Exact libtest semantic failure classifier; no substring-only kill.
pub struct Policy<'a> {
    pub test: &'a str,
    pub id: &'a str,
    pub location: String,
    filtered: usize,
}
impl<'a> Policy<'a> {
    pub fn bind(source: &str, test: &'a str, id: &'a str) -> Result<Self, String> {
        Self::bind_at(source, test, id, "tests/owned_controls.rs", 3)
    }
    pub fn bind_at(
        source: &str,
        test: &'a str,
        id: &'a str,
        file: &str,
        filtered: usize,
    ) -> Result<Self, String> {
        if !matches!(
            file,
            "tests/owned_controls.rs" | "tests/network_policies.rs"
        ) {
            return Err("policy file binding refused".into());
        }
        let candidates: Vec<_> = source
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(id))
            .collect();
        let (line, text) = match candidates.as_slice() {
            [(line, text)] if text.trim_start().starts_with("assert!(") => (*line, *text),
            _ => return Err("policy assertion source binding failed".into()),
        };
        let column = text.len() - text.trim_start().len() + 1;
        Ok(Self {
            test,
            id,
            location: format!("{file}:{}:{column}", line + 1),
            filtered,
        })
    }
    pub fn accepts(&self, code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> bool {
        if code != Some(101) {
            return false;
        }
        let (Ok(out), Ok(err)) = (std::str::from_utf8(stdout), std::str::from_utf8(stderr)) else {
            return false;
        };
        let prefix = format!("thread '{}' (", self.test);
        let suffix = format!(") panicked at {}:", self.location);
        let Some(panic) = out.lines().find(|l| l.starts_with(&prefix)) else {
            return false;
        };
        let Some(number) = panic
            .strip_prefix(&prefix)
            .and_then(|s| s.strip_suffix(&suffix))
        else {
            return false;
        };
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let summary_prefix=format!("test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; {} filtered out; finished in ",self.filtered);
        let Some(summary) = out.lines().find(|l| l.starts_with(&summary_prefix)) else {
            return false;
        };
        let Some(duration) = summary
            .strip_prefix(&summary_prefix)
            .and_then(|s| s.strip_suffix('s'))
        else {
            return false;
        };
        if !decimal(duration) {
            return false;
        }
        let expected=format!("\nrunning 1 test\ntest {} ... FAILED\n\nfailures:\n\n---- {} stdout ----\n\n{panic}\n{}\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    {}\n\n{summary}\n\n",self.test,self.test,self.id,self.test);
        out == expected && self.stderr(err)
    }
    fn stderr(&self, err: &str) -> bool {
        let target = if self.filtered == 3 {
            "owned_controls"
        } else {
            "network_policies"
        };
        let footer = format!("error: test failed, to rerun pass `--test {target}`\n");
        let Some(progress) = err.strip_suffix(&footer) else {
            return false;
        };
        let mut lines = progress.lines().peekable();
        while let Some(line) = lines.peek().copied() {
            let Some(value) = line.strip_prefix("   Compiling ") else {
                break;
            };
            let mut words = value.splitn(3, ' ');
            let (Some(name), Some(version)) = (words.next(), words.next()) else {
                return false;
            };
            if !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || name.is_empty()
                || !version.starts_with('v')
                || !version.get(1..).is_some_and(|s| {
                    s.bytes().all(|b| b.is_ascii_digit() || b == b'.') && !s.is_empty()
                })
            {
                return false;
            }
            if let Some(path) = words.next() {
                if !path.starts_with("(/") || !path.ends_with(')') || path.contains('`') {
                    return false;
                }
            }
            lines.next();
        }
        if lines.peek() == Some(&"warning: unreachable pattern") {
            if self.id != "M4_NET_EXIT" {
                return false;
            }
            let warning=["warning: unreachable pattern","   --> src/isolated.rs:429:9","    |","428 |         Ok(bytes) => { let _ = success; Ok(bytes) },","    |         --------- matches all the relevant values","429 |         Ok(_) => Err(Error::Exit),","    |         ^^^^^ no value can reach this","    |","    = note: `#[warn(unreachable_patterns)]` (part of `#[warn(unused)]`) on by default","","warning: `server-storage-reads` (lib) generated 1 warning"];
            for expected in warning {
                if lines.next() != Some(expected) {
                    return false;
                }
            }
        }
        let Some(finished) = lines
            .next()
            .and_then(|s| {
                s.strip_prefix(
                    "    Finished `test` profile [unoptimized + debuginfo] target(s) in ",
                )
            })
            .and_then(|s| s.strip_suffix('s'))
        else {
            return false;
        };
        if !decimal(finished) {
            return false;
        }
        let Some(running) = lines
            .next()
            .and_then(|s| s.strip_prefix(&format!("     Running tests/{target}.rs (")))
            .and_then(|s| s.strip_suffix(')'))
        else {
            return false;
        };
        if !running.starts_with('/')
            || !running.contains(&format!("/target/debug/deps/{target}-"))
            || running.contains('`')
        {
            return false;
        }
        lines.next().is_none()
    }
}
fn decimal(s: &str) -> bool {
    let mut dot = false;
    let mut digits = 0;
    for b in s.bytes() {
        if b == b'.' && !dot {
            dot = true;
        } else if b.is_ascii_digit() {
            digits += 1;
        } else {
            return false;
        }
    }
    digits > 0 && !s.starts_with('.') && !s.ends_with('.')
}
