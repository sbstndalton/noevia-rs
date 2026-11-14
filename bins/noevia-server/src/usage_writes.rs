//! Track R12: the usage writer, `users/<id>/usage.json` (core usage.cjs `recordUsage`,
//! `recordToolUse`; crates/server-usage `write`). With NOEVIA_RUST_USAGE_WRITES=1 core's usage.cjs
//! hands every record to the internal store's `POST /v1/usage/record` (project_store.rs), so this is
//! the file's only writer; unset, nothing calls it and Node writes the file as before.
//!
//! One record at a time in this process (a read-modify-write of one small file). The account's
//! directory is never made here: a removed account (Node: `workspace.revoked`) records nothing. The
//! file is read without following a link, at most [`server_usage::FILE_CAP`] bytes (a larger one is
//! left alone, as the native usage reads refuse it), and replaced by a 0600 temporary file renamed
//! over it. Accounting never fails a request: the caller only learns whether it was written.

use js_json::JValue;
use server_usage::write::{self, Outcome};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
static TEMP: AtomicU64 = AtomicU64::new(0);

/// What a record did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    Written,
    /// Node would not write either (no tokens, no name), or the account is gone.
    Nothing,
    /// Left alone: a file this port does not add to, or one it could not read or replace safely.
    Skipped,
}

/// One record to make.
pub enum Record<'a> {
    /// `recordUsage(workspace, model, usage)`.
    Reply { model: &'a JValue, usage: &'a JValue },
    /// `recordToolUse(workspace, name)`.
    Tool { name: &'a JValue },
}

fn valid_user(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// The file's bytes (`None`: no file), or `Err` for a link, a non-regular file or one over the cap.
fn read(dir: &rustix::fd::OwnedFd) -> Result<Option<Vec<u8>>, ()> {
    use rustix::fs::{openat, FileType, Mode, OFlags};
    let fd = match openat(
        dir,
        "usage.json",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(()),
    };
    let stat = rustix::fs::fstat(&fd).map_err(|_| ())?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink != 1
        || u64::try_from(stat.st_size).map_or(true, |n| n > server_usage::FILE_CAP)
    {
        return Err(());
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(server_usage::FILE_CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > server_usage::FILE_CAP {
        return Err(());
    }
    Ok(Some(bytes))
}

/// `atomicJson`: a new 0600 temporary file beside it, synced, renamed over it.
fn replace(dir: &rustix::fd::OwnedFd, text: &str) -> Result<(), ()> {
    use rustix::fs::{openat, renameat, unlinkat, AtFlags, Mode, OFlags};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = format!(
        "usage.json.{}.{nanos}.{}.tmp",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::Relaxed)
    );
    let fd = openat(
        dir,
        tmp.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|_| ())?;
    let done = (|| -> Result<(), ()> {
        let mut file = std::fs::File::from(fd);
        file.write_all(text.as_bytes()).map_err(|_| ())?;
        file.sync_all().map_err(|_| ())?;
        renameat(dir, tmp.as_str(), dir, "usage.json").map_err(|_| ())
    })();
    if done.is_err() {
        let _ = unlinkat(dir, tmp.as_str(), AtFlags::empty());
    }
    done
}

/// Makes one record for `user` (blocking).
pub fn record(data_dir: &Path, user: &str, what: Record<'_>) -> Recorded {
    use rustix::fs::{open, openat, Mode, OFlags};
    if !valid_user(user) {
        return Recorded::Skipped;
    }
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let dir = match open(data_dir, flags, Mode::empty())
        .and_then(|root| openat(&root, "users", flags, Mode::empty()))
        .and_then(|users| openat(&users, user, flags, Mode::empty()))
    {
        Ok(d) => d,
        Err(rustix::io::Errno::NOENT) => return Recorded::Nothing,
        Err(_) => return Recorded::Skipped,
    };
    let Ok(stored) = read(&dir) else {
        return Recorded::Skipped;
    };
    let Some(clock) = write::clock_now() else {
        return Recorded::Skipped;
    };
    let outcome = match what {
        Record::Reply { model, usage } => write::record_usage(stored.as_deref(), model, usage, &clock),
        Record::Tool { name } => write::record_tool(stored.as_deref(), name, &clock),
    };
    match outcome {
        Outcome::Nothing => Recorded::Nothing,
        Outcome::Unusual => Recorded::Skipped,
        Outcome::Write(text) => match replace(&dir, &text) {
            Ok(()) => Recorded::Written,
            Err(()) => Recorded::Skipped,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const USER: &str = "aaaaaaaa-8888-4aaa-8aaa-aaaaaaaaaaaa";

    fn usage() -> JValue {
        js_json::wtf16::parse(r#"{"promptTokens":3,"completionTokens":2}"#).unwrap()
    }

    #[test]
    fn records_only_into_an_existing_account_and_never_through_a_link() {
        let d = tempfile::tempdir().unwrap();
        let model = JValue::Str("qa-model".into());
        // No account directory: nothing, and nothing is made.
        let u = usage();
        assert_eq!(record(d.path(), USER, Record::Reply { model: &model, usage: &u }), Recorded::Nothing);
        assert!(!d.path().join("users").exists());
        assert_eq!(record(d.path(), "../x", Record::Tool { name: &model }), Recorded::Skipped);
        let dir = d.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        let u = usage();
        assert_eq!(record(d.path(), USER, Record::Reply { model: &model, usage: &u }), Recorded::Written);
        let file = dir.join("usage.json");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        let stored: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let day = stored["days"].as_object().unwrap().values().next().unwrap().clone();
        assert_eq!((day["input"].as_f64(), day["models"]["qa-model"]["replies"].as_f64()), (Some(3.0), Some(1.0)));
        assert_eq!(record(d.path(), USER, Record::Tool { name: &JValue::Str("qa_tool".into()) }), Recorded::Written);
        assert_eq!(record(d.path(), USER, Record::Tool { name: &JValue::Str(String::new()) }), Recorded::Nothing);
        // Only usage.json is left in the directory (no temporary file).
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("usage.json")]);
        // A linked usage.json is left alone, and so is what it points at.
        std::fs::remove_file(&file).unwrap();
        let outside = d.path().join("outside.json");
        std::fs::write(&outside, "{}").unwrap();
        std::os::unix::fs::symlink(&outside, &file).unwrap();
        assert_eq!(record(d.path(), USER, Record::Tool { name: &model }), Recorded::Skipped);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "{}");
        std::fs::remove_file(&file).unwrap();
        std::fs::hard_link(&outside, &file).unwrap();
        assert_eq!(record(d.path(), USER, Record::Tool { name: &model }), Recorded::Skipped);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "{}");
    }
}
