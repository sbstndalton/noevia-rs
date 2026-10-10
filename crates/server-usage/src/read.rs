//! Bounded capability-relative reads. Never follow user/file symlinks; no usage writes.
use crate::{summary::Usage, Fault, ACCOUNT_CAP, FILE_CAP, TOTAL_CAP};
use rustix::fs::{open, openat, Mode, OFlags};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

fn valid_user(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}
enum ReadResult {
    Missing,
    Invalid(u64),
    Value(crate::input::Input, u64),
}
fn read(data: &Path, uid: &str) -> Result<ReadResult, Fault> {
    if !valid_user(uid) {
        return Err(Fault::InvalidUser);
    }
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let read_inner = || -> Result<ReadResult, rustix::io::Errno> {
        let root = open(data, flags | OFlags::DIRECTORY, Mode::empty())?;
        let users = openat(&root, "users", flags | OFlags::DIRECTORY, Mode::empty())?;
        let user = openat(&users, uid, flags | OFlags::DIRECTORY, Mode::empty())?;
        let fd = openat(&user, "usage.json", flags | OFlags::NONBLOCK, Mode::empty())?;
        let file = File::from(fd);
        let Ok(stat) = file.metadata() else {
            return Ok(ReadResult::Invalid(0));
        };
        if !stat.is_file() || stat.len() > FILE_CAP {
            return Ok(ReadResult::Invalid(0));
        }
        let mut bytes = Vec::with_capacity((stat.len() as usize).min(FILE_CAP as usize));
        if file.take(FILE_CAP + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > FILE_CAP
        {
            return Ok(ReadResult::Invalid(bytes.len() as u64));
        }
        let length = bytes.len() as u64;
        // Node readFile('utf8') replaces invalid UTF-8; use the same decoding before JSON.
        let text = String::from_utf8_lossy(&bytes);
        let value = serde_json::from_str::<crate::input::Input>(&text).ok();
        Ok(match value {
            Some(v) => ReadResult::Value(v, length),
            None => ReadResult::Invalid(length),
        })
    };
    Ok(match read_inner() {
        Ok(r) => r,
        Err(rustix::io::Errno::NOENT) => ReadResult::Missing,
        Err(_) => ReadResult::Invalid(0),
    })
}
pub fn own(data: &Path, uid: &str) -> Result<Usage, Fault> {
    let mut out = Usage::default();
    if let ReadResult::Value(v, _) = read(data, uid)? {
        if matches!(
            v.get("days"),
            Some(crate::input::Input::Object(_) | crate::input::Input::Array(_))
        ) {
            out.merge_input(&v, false)?;
        }
    }
    Ok(out)
}
#[derive(Clone)]
pub struct Aggregate {
    pub store: Usage,
    pub accounts: usize,
    pub unreadable: usize,
    pub checked_at: i64,
}
struct Cached {
    paths: Vec<String>,
    at: i64,
    value: Aggregate,
}
/// One cache and one aggregate read at a time. Handler admission separately bounds requests.
#[derive(Default)]
pub struct AggregateReader {
    cache: Mutex<Option<Cached>>,
}
impl AggregateReader {
    pub fn get(&self, data: &Path, users: Vec<String>, now: i64) -> Result<Aggregate, Fault> {
        if users.len() > ACCOUNT_CAP {
            return Err(Fault::Limit);
        }
        if !users.iter().all(|u| valid_user(u)) {
            return Err(Fault::InvalidUser);
        }
        let paths: Vec<_> = users
            .iter()
            .map(|u| {
                data.join("users")
                    .join(u)
                    .join("usage.json")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let mut cache = self.cache.lock().map_err(|_| Fault::Io)?;
        if let Some(c) = cache.as_ref() {
            if c.paths == paths && now.saturating_sub(c.at) < 30000 {
                return Ok(c.value.clone());
            }
        }
        let mut value = Aggregate {
            store: Usage::default(),
            accounts: users.len(),
            unreadable: 0,
            checked_at: now,
        };
        let mut total = 0;
        for u in users {
            match read(data, &u)? {
                ReadResult::Missing => {}
                ReadResult::Invalid(n) => {
                    total += n;
                    if total > TOTAL_CAP {
                        return Err(Fault::Limit);
                    }
                    value.unreadable += 1;
                }
                ReadResult::Value(v, n) => {
                    total += n;
                    if total > TOTAL_CAP {
                        return Err(Fault::Limit);
                    }
                    if !matches!(v.get("days"), Some(crate::input::Input::Object(_))) {
                        value.unreadable += 1;
                        continue;
                    }
                    value.store.merge_input(&v, true)?;
                }
            }
        }
        *cache = Some(Cached {
            paths,
            at: now,
            value: value.clone(),
        });
        Ok(value)
    }
}
