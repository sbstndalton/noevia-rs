//! The off-site backup's destinations (sbstndalton/noevia#2190 round 7), ported from core
//! `offsite-dir.cjs` and `offsite-s3.cjs` with the same contract: relative keys, `list` filtered by
//! prefix, `get` returning none for a missing key, `delete` idempotent.
//!
//! - [`DirStore`]: a folder. Writes are atomic (a hidden `.tmp-` file, fsync, rename), because the
//!   host may copy the folder on its own schedule and must never see half a chunk; hidden entries
//!   are never listed.
//! - [`S3Store`]: any S3-compatible service speaking SigV4 path-style (crates/s3-sign), https only
//!   (a loopback address may use http), the credentials only in signed headers, no redirect
//!   followed, 60 s per request, an object read refused past 64 MiB while streaming, a listing page
//!   past 4 MiB refused.
//!
//! File work runs on the blocking pool; nothing here logs a key, credential or object.
use crate::gdrive::{encode_uri_component, random, Fail};
use offsite_backup_format::{civil_from_days, valid_dir_key};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// A data object is one sealed chunk (4 MiB + 28 bytes); manifests stay far below this.
pub const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
const LIST_PAGE_CAP: usize = 4 * 1024 * 1024;
const LIST_PAGES: usize = 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const TEMP: &str = ".tmp-";

fn io_fail(_: std::io::Error) -> Fail {
    Fail::new("The backup destination could not be reached.", 502)
}

fn blocked(_: tokio::task::JoinError) -> Fail {
    Fail::new("The backup destination could not be reached.", 502)
}

#[derive(Clone)]
pub struct DirStore {
    base: Arc<PathBuf>,
}

impl DirStore {
    /// `createDirStore({ root })`: the root must be absolute.
    pub fn new(root: &Path) -> Result<DirStore, Fail> {
        if !root.is_absolute() {
            return Err(Fail::new(
                "OFFSITE_BACKUP_DIR must be an absolute path.",
                400,
            ));
        }
        Ok(DirStore {
            base: Arc::new(root.to_path_buf()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.base
    }

    fn file(&self, key: &str) -> Result<PathBuf, Fail> {
        if !valid_dir_key(key) {
            let shown: String = key.chars().take(80).collect();
            return Err(Fail::new(
                format!("Refusing an unsafe backup key: {shown}"),
                400,
            ));
        }
        let mut full = (*self.base).clone();
        for seg in key.split('/') {
            full.push(seg);
        }
        Ok(full)
    }

    pub async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), Fail> {
        let target = self.file(key)?;
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write as _;
            use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
            let dir = target.parent().map(Path::to_path_buf).unwrap_or_default();
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
            let temp = dir.join(format!("{TEMP}{}", crate::gdrive::hex_of(&random::<6>())));
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            let written = f.write_all(&bytes).and_then(|()| f.sync_all());
            drop(f);
            let renamed = written.and_then(|()| std::fs::rename(&temp, &target));
            if renamed.is_err() {
                let _ = std::fs::remove_file(&temp);
            }
            renamed
        })
        .await
        .map_err(blocked)?
        .map_err(io_fail)
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Fail> {
        let path = self.file(key)?;
        tokio::task::spawn_blocking(move || match std::fs::read(path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        })
        .await
        .map_err(blocked)?
        .map_err(io_fail)
    }

    pub async fn size_of(&self, key: &str) -> Result<Option<u64>, Fail> {
        let path = self.file(key)?;
        tokio::task::spawn_blocking(move || match std::fs::metadata(path) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        })
        .await
        .map_err(blocked)?
        .map_err(io_fail)
    }

    pub async fn delete(&self, key: &str) -> Result<(), Fail> {
        let path = self.file(key)?;
        tokio::task::spawn_blocking(move || match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .map_err(blocked)?
        .map_err(io_fail)
    }

    pub async fn list(&self, prefix: &str) -> Result<Vec<String>, Fail> {
        let (base, prefix) = (Arc::clone(&self.base), prefix.to_owned());
        tokio::task::spawn_blocking(move || -> std::io::Result<Vec<String>> {
            fn walk(
                dir: &Path,
                rel: &str,
                prefix: &str,
                out: &mut Vec<String>,
            ) -> std::io::Result<()> {
                let entries = match std::fs::read_dir(dir) {
                    Ok(e) => e,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(e),
                };
                for entry in entries {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // Temp files are half-written by definition; dot entries are never ours.
                    if name.starts_with('.') {
                        continue;
                    }
                    let next = if rel.is_empty() {
                        name.clone()
                    } else {
                        format!("{rel}/{name}")
                    };
                    let kind = entry.file_type()?;
                    if kind.is_dir() {
                        walk(&entry.path(), &next, prefix, out)?;
                    } else if kind.is_file() && next.starts_with(prefix) {
                        out.push(next);
                    }
                }
                Ok(())
            }
            let mut out = Vec::new();
            walk(&base, "", &prefix, &mut out)?;
            out.sort();
            Ok(out)
        })
        .await
        .map_err(blocked)?
        .map_err(io_fail)
    }
}

/// `YYYYMMDDTHHMMSSZ` for a wall-clock time in ms.
pub fn amz_date(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[derive(Clone)]
pub struct S3Store {
    client: reqwest::Client,
    base: url::Url,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: Arc<zeroize::Zeroizing<String>>,
    root: String,
}

fn s3_refused(what: &str, status: u16) -> Fail {
    Fail::new(
        format!("The backup destination refused {what} ({status})."),
        502,
    )
}

impl S3Store {
    /// `createS3Store(...)`: the endpoint must be https (loopback may be http), carry no credentials
    /// or query, and every value must be set.
    pub fn new(
        endpoint: &str,
        bucket: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
        prefix: &str,
    ) -> Result<S3Store, Fail> {
        if endpoint.is_empty()
            || bucket.is_empty()
            || access_key.is_empty()
            || secret_key.is_empty()
        {
            return Err(Fail::new(
                "Offsite backup needs endpoint, bucket and credentials.",
                409,
            ));
        }
        let base = url::Url::parse(endpoint).map_err(|_| {
            Fail::new(
                "Offsite backup needs endpoint, bucket and credentials.",
                409,
            )
        })?;
        let loopback = matches!(base.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        if base.scheme() != "https" && !loopback {
            return Err(Fail::new("Offsite backup endpoints must use HTTPS.", 409));
        }
        if !base.username().is_empty() || base.password().is_some() || base.query().is_some() {
            return Err(Fail::new(
                "Put credentials in their own settings, not the endpoint URL.",
                409,
            ));
        }
        // Collapse doubled slashes and trim, so object URLs and listing prefixes agree.
        let mut root = String::new();
        let mut last_slash = false;
        for c in prefix.chars() {
            if c == '/' && last_slash {
                continue;
            }
            last_slash = c == '/';
            root.push(c);
        }
        let root = root.trim_matches('/').to_owned();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Fail::new("The backup destination could not be reached.", 502))?;
        Ok(S3Store {
            client,
            base,
            bucket: bucket.to_owned(),
            region: s3_sign::normalize_region(region),
            access_key: access_key.to_owned(),
            secret_key: Arc::new(zeroize::Zeroizing::new(secret_key.to_owned())),
            root,
        })
    }

    fn origin_and_path(&self) -> (String, String) {
        let origin = self.base.origin().ascii_serialization();
        (origin, self.base.path().trim_end_matches('/').to_owned())
    }

    fn object_url(&self, key: &str) -> Result<url::Url, Fail> {
        let (origin, path) = self.origin_and_path();
        let segments: Vec<String> = std::iter::once(self.bucket.clone())
            .chain(
                format!("{}/{key}", self.root)
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            )
            .map(|s| encode_uri_component(&s))
            .collect();
        url::Url::parse(&format!("{origin}{path}/{}", segments.join("/")))
            .map_err(|_| Fail::new("The backup destination could not be reached.", 502))
    }

    async fn request(
        &self,
        method: &str,
        target: &url::Url,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, Fail> {
        let host = match target.port() {
            Some(p) => format!("{}:{p}", target.host_str().unwrap_or_default()),
            None => target.host_str().unwrap_or_default().to_owned(),
        };
        let query: Vec<(String, String)> = target
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let query_ref: Vec<(&str, &str)> = query
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let date = amz_date(crate::gdrive::now_ms());
        let signed = s3_sign::sign(&s3_sign::Request {
            method,
            host: &host,
            pathname: target.path(),
            query: &query_ref,
            payload: &body,
            access_key: &self.access_key,
            secret_key: self.secret_key.as_bytes(),
            region: &self.region,
            session_token: "",
            amz_date: &date,
        })
        .map_err(|_| Fail::new("storage request could not be signed", 502))?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| Fail::new("The backup destination could not be reached.", 502))?;
        let mut builder = self
            .client
            .request(method.clone(), target.clone())
            .timeout(REQUEST_TIMEOUT);
        for (name, value) in &signed.headers {
            if name.eq_ignore_ascii_case("host") {
                continue;
            }
            builder = builder.header(*name, value);
        }
        if method == reqwest::Method::PUT {
            builder = builder.body(body);
        }
        let response = builder
            .send()
            .await
            .map_err(|_| Fail::new("The backup destination could not be reached.", 502))?;
        if response.status().is_redirection() {
            return Err(Fail::new(
                "The backup destination could not be reached.",
                502,
            ));
        }
        Ok(response)
    }

    async fn read_capped(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, Fail> {
        let too_large = || {
            Fail::new(
                "A backup object from the destination is larger than any object this backup writes.",
                502,
            )
        };
        if response.content_length().is_some_and(|n| n > limit as u64) {
            return Err(too_large());
        }
        let mut out = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Fail::new("The backup destination could not be reached.", 502))?
        {
            if out.len() + chunk.len() > limit {
                return Err(too_large());
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    pub async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), Fail> {
        let r = self.request("PUT", &self.object_url(key)?, bytes).await?;
        if !r.status().is_success() {
            return Err(s3_refused("a write", r.status().as_u16()));
        }
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Fail> {
        let r = self
            .request("GET", &self.object_url(key)?, Vec::new())
            .await?;
        if r.status().as_u16() == 404 {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(s3_refused("a read", r.status().as_u16()));
        }
        Self::read_capped(r, MAX_OBJECT_BYTES).await.map(Some)
    }

    pub async fn delete(&self, key: &str) -> Result<(), Fail> {
        let r = self
            .request("DELETE", &self.object_url(key)?, Vec::new())
            .await?;
        if !r.status().is_success() && r.status().as_u16() != 404 {
            return Err(s3_refused("a delete", r.status().as_u16()));
        }
        Ok(())
    }

    pub async fn list(&self, key_prefix: &str) -> Result<Vec<String>, Fail> {
        let full = if self.root.is_empty() {
            key_prefix.to_owned()
        } else {
            format!("{}/{key_prefix}", self.root)
        };
        let skip = if self.root.is_empty() {
            0
        } else {
            self.root.len() + 1
        };
        let (origin, path) = self.origin_and_path();
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..LIST_PAGES {
            let mut target = url::Url::parse(&format!(
                "{origin}{path}/{}",
                encode_uri_component(&self.bucket)
            ))
            .map_err(|_| Fail::new("The backup destination could not be reached.", 502))?;
            {
                let mut q = target.query_pairs_mut();
                q.append_pair("list-type", "2").append_pair("prefix", &full);
                if let Some(t) = &token {
                    q.append_pair("continuation-token", t);
                }
            }
            let r = self.request("GET", &target, Vec::new()).await?;
            if !r.status().is_success() {
                return Err(s3_refused("a listing", r.status().as_u16()));
            }
            let bytes = Self::read_capped(r, LIST_PAGE_CAP).await.map_err(|_| {
                Fail::new(
                    "The backup destination sent a listing that was too large.",
                    502,
                )
            })?;
            let xml = String::from_utf8_lossy(&bytes);
            for k in dav_parse::element_texts(&xml, "Key", 10_000) {
                let key = dav_parse::decode_xml_entities(k);
                keys.push(key.get(skip..).unwrap_or_default().to_owned());
            }
            let next = dav_parse::element_texts(&xml, "NextContinuationToken", 1)
                .first()
                .map(|t| dav_parse::decode_xml_entities(t));
            let truncated =
                dav_parse::element_texts(&xml, "IsTruncated", 1).first() == Some(&"true");
            match next {
                Some(t) if truncated => token = Some(t),
                _ => break,
            }
        }
        Ok(keys)
    }
}

/// The destination of one run.
#[derive(Clone)]
pub enum Store {
    Dir(DirStore),
    S3(S3Store),
}

impl Store {
    pub async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), Fail> {
        match self {
            Store::Dir(s) => s.put(key, bytes).await,
            Store::S3(s) => s.put(key, bytes).await,
        }
    }
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Fail> {
        match self {
            Store::Dir(s) => s.get(key).await,
            Store::S3(s) => s.get(key).await,
        }
    }
    pub async fn delete(&self, key: &str) -> Result<(), Fail> {
        match self {
            Store::Dir(s) => s.delete(key).await,
            Store::S3(s) => s.delete(key).await,
        }
    }
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>, Fail> {
        match self {
            Store::Dir(s) => s.list(prefix).await,
            Store::S3(s) => s.list(prefix).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn amz_dates_are_iso_without_separators() {
        assert_eq!(amz_date(1_700_000_000_123), "20231114T221320Z");
        assert_eq!(amz_date(0), "19700101T000000Z");
    }

    #[test]
    fn s3_endpoints_are_checked_like_core() {
        let new = |e: &str| S3Store::new(e, "b", "us-east-1", "ak", "sk", "p");
        assert!(new("https://s3.example.test").is_ok());
        assert!(new("http://127.0.0.1:9000").is_ok());
        assert!(
            new("http://s3.example.test").is_err(),
            "plain http to a remote host"
        );
        assert!(new("https://u:p@s3.example.test").is_err());
        assert!(new("https://s3.example.test/?x=1").is_err());
        assert!(S3Store::new("https://s3.example.test", "", "r", "a", "s", "p").is_err());
    }

    #[test]
    fn s3_object_urls_collapse_the_prefix() {
        let s = S3Store::new(
            "https://s3.example.test/base/",
            "my bucket",
            "us-east-1",
            "ak",
            "sk",
            "//a//b/",
        )
        .unwrap();
        assert_eq!(
            s.object_url("data/ab/cd").unwrap().as_str(),
            "https://s3.example.test/base/my%20bucket/a/b/data/ab/cd"
        );
    }

    #[tokio::test]
    async fn the_folder_store_is_core_dir_store() {
        let d = tempfile::tempdir().unwrap();
        let s = DirStore::new(d.path()).unwrap();
        s.put("config", b"c".to_vec()).await.unwrap();
        s.put("data/ab/abcd", b"chunk".to_vec()).await.unwrap();
        std::fs::write(d.path().join(".tmp-half"), b"x").unwrap();
        std::fs::create_dir_all(d.path().join(".hidden")).unwrap();
        std::fs::write(d.path().join(".hidden").join("f"), b"x").unwrap();
        assert_eq!(s.get("config").await.unwrap().as_deref(), Some(&b"c"[..]));
        assert_eq!(s.get("missing").await.unwrap(), None);
        assert_eq!(s.list("").await.unwrap(), ["config", "data/ab/abcd"]);
        assert_eq!(s.list("data/").await.unwrap(), ["data/ab/abcd"]);
        assert_eq!(s.size_of("data/ab/abcd").await.unwrap(), Some(5));
        s.delete("config").await.unwrap();
        s.delete("config").await.unwrap();
        assert_eq!(s.get("config").await.unwrap(), None);
        for bad in ["../x", ".tmp-a", "a//b", "/abs", ""] {
            assert!(s.put(bad, vec![]).await.is_err(), "{bad}");
            assert!(s.get(bad).await.is_err(), "{bad}");
        }
        let no_temp = std::fs::read_dir(d.path().join("data/ab")).unwrap().count();
        assert_eq!(no_temp, 1, "no temp file is left behind");
        assert!(DirStore::new(Path::new("relative")).is_err());
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(d.path().join("data/ab/abcd"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
