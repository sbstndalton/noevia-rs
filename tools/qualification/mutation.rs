//! A failed build or unrelated panic is infrastructure, never mutation evidence.
use super::process::Output;
#[derive(Debug)]
pub enum Failure {
    Infrastructure(String),
    Assertion { test: String, id: String },
}
pub fn classify(result: Result<Output, String>) -> Failure {
    let output = match result {
        Ok(output) => output,
        Err(error) => return Failure::Infrastructure(error),
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.code() != Some(101)
        || !text.contains("test result: FAILED. 0 passed; 1 failed;")
        || text.contains("could not compile")
    {
        return Failure::Infrastructure("not one completed failing test".into());
    }
    let panics: Vec<_> = text
        .lines()
        .filter_map(|line| {
            line.strip_prefix("thread '")
                .and_then(|line| line.split_once('\''))
                .filter(|(_, tail)| tail.contains("panicked at"))
                .map(|(test, _)| test)
        })
        .collect();
    let ids: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let message = line
                .split_once("failed: ")
                .map_or(line, |(_, message)| message);
            message.strip_prefix("ROUTING_ASSERT/").map(|_| message)
        })
        .collect();
    match (panics.as_slice(), ids.as_slice()) {
        ([test], [id]) => Failure::Assertion {
            test: (*test).into(),
            id: (*id).into(),
        },
        _ => Failure::Infrastructure("missing unique named semantic assertion".into()),
    }
}
pub fn reject_mutant(test: &str, id: &str, failure: Failure) -> Result<(), String> {
    match failure {
        Failure::Assertion {
            test: actual_test,
            id: actual_id,
        } if actual_test == test && actual_id == id => Ok(()),
        Failure::Assertion { test, id } => Err(format!("wrong assertion {test}: {id}")),
        Failure::Infrastructure(error) => Err(format!("infrastructure: {error}")),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    fn output(text: &str, code: i32) -> Result<Output, String> {
        Ok(Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: text.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }
    #[test]
    fn exact_assertion_required_and_infrastructure_rejected() {
        let good="thread 'expected' (1) panicked at test.rs:1:1:\nassertion `left == right` failed: ROUTING_ASSERT/expected\ntest result: FAILED. 0 passed; 1 failed;";
        assert!(reject_mutant(
            "expected",
            "ROUTING_ASSERT/expected",
            classify(output(good, 101))
        )
        .is_ok());
        for text in [
            good.replace("ROUTING_ASSERT/expected", "ROUTING_ASSERT/wrong"),
            good.replace("thread 'expected'", "thread 'other'"),
            good.replace(
                "assertion `left == right` failed: ROUTING_ASSERT/expected",
                "called Result::unwrap on Err: synthetic IO",
            ),
            format!("{good}\ncould not compile package"),
        ] {
            assert!(reject_mutant(
                "expected",
                "ROUTING_ASSERT/expected",
                classify(output(&text, 101))
            )
            .is_err());
        }
        for result in [
            Err("child spawn failed".into()),
            Err("child deadline exceeded".into()),
            Err("child output limit exceeded".into()),
            output(good, 1),
            output(good, 0),
        ] {
            assert!(
                reject_mutant("expected", "ROUTING_ASSERT/expected", classify(result)).is_err()
            );
        }
    }
}
