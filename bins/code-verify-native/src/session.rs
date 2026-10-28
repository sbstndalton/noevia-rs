//! Same safe session boundary called by the trusted Exec helper and owned-child test.
pub(super) fn enter_untrusted_session() -> Result<(), Box<dyn std::error::Error>> {
    let session = nix::unistd::setsid()?;
    let own = nix::unistd::getpid();
    if session != own || nix::unistd::getsid(None)? != own || nix::unistd::getpgrp() != own {
        return Err("fresh session refused".into());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    #[test]
    fn exec_session_is_new_and_owned_by_the_exact_child() -> Result<(), Box<dyn std::error::Error>>
    {
        const WORKER: &str = "NOEVIA_PRIVATE_VERIFIER_SESSION_TEST";
        if std::env::var_os(WORKER).is_some() {
            enter_untrusted_session()?;
            println!(
                "VERIFIER_SESSION {} {}",
                nix::unistd::getsid(None)?.as_raw(),
                nix::unistd::getpgrp().as_raw()
            );
            return Ok(());
        }
        let parent = nix::unistd::getsid(None)?.as_raw();
        let child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "session::tests::exec_session_is_new_and_owned_by_the_exact_child",
                "--nocapture",
            ])
            .env_clear()
            .env(WORKER, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let id = i32::try_from(child.id())?;
        let output = child.wait_with_output()?;
        assert!(output.status.success(), "owned session probe failed");
        let text = String::from_utf8(output.stdout)?;
        let line = text
            .lines()
            .find(|l| l.starts_with("VERIFIER_SESSION "))
            .ok_or("session evidence")?;
        let fields: Vec<i32> = line
            .split_whitespace()
            .skip(1)
            .map(str::parse)
            .collect::<Result<_, _>>()?;
        assert_eq!(fields, vec![id, id]);
        assert_ne!(id, parent);
        Ok(())
    }
}
