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
        let Ok(out) = std::str::from_utf8(stdout) else {
            return false;
        };
        let Ok(err) = std::str::from_utf8(stderr) else {
            return false;
        };
        if err.contains("panicked at")
            || err.contains("error[E")
            || err.contains("could not compile")
        {
            return false;
        }
        let lines: Vec<_> = out.lines().collect();
        if lines.iter().filter(|l| l.contains("panicked at")).count() != 1
            || lines.iter().filter(|l| l.contains(self.id)).count() != 1
        {
            return false;
        }
        let Some(index) = lines.iter().position(|l| l.contains("panicked at")) else {
            return false;
        };
        let Some(panic) = lines.get(index) else {
            return false;
        };
        let prefix = format!("thread '{}' (", self.test);
        let suffix = format!(") panicked at {}:", self.location);
        let Some(number) = panic
            .strip_prefix(&prefix)
            .and_then(|v| v.strip_suffix(&suffix))
        else {
            return false;
        };
        if number.is_empty()
            || !number.bytes().all(|b| b.is_ascii_digit())
            || lines.get(index + 1) != Some(&self.id)
        {
            return false;
        }
        // Exact single selected test result and sole failure inventory.
        let expected = format!("test {} ... FAILED", self.test);
        let test_lines: Vec<_> = lines
            .iter()
            .filter(|line| line.starts_with("test "))
            .copied()
            .collect();
        if test_lines.len() != 2 || test_lines.first().copied() != Some(expected.as_str()) {
            return false;
        }
        let Some(summary) = test_lines.get(1) else {
            return false;
        };
        if !summary.starts_with(&format!("test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; {} filtered out; finished in ",self.filtered)) || !summary.ends_with('s') {return false;}
        if lines.iter().filter(|l| **l == "running 1 test").count() != 1 {
            return false;
        }
        let marker = format!("---- {} stdout ----", self.test);
        if lines.iter().filter(|l| l.starts_with("---- ")).count() != 1
            || !lines.contains(&marker.as_str())
        {
            return false;
        }
        let inventories: Vec<_> = lines
            .windows(2)
            .filter(|pair| pair.first() == Some(&"failures:"))
            .collect();
        if inventories.len() != 2 {
            return false;
        }
        // The final inventory must name exactly this test, followed by a blank line.
        let Some(final_index) = lines.iter().rposition(|l| *l == "failures:") else {
            return false;
        };
        lines.get(final_index + 1) == Some(&format!("    {}", self.test).as_str())
            && lines.get(final_index + 2) == Some(&"")
    }
}
