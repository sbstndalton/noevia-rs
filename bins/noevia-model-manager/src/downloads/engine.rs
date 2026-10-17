//! `DownloadManager._run`, `_stream`, `_stream_parallel`, `_stream_single`, `_verify_size` and
//! the validator files, with httpx's redirect handling and `_authorize_request` (the token goes
//! only to the token origin, re-decided for every hop).
use super::{pyfmt, Chunk, Downloads, Job};
use futures_util::StreamExt;
use std::{
    io::{Seek, SeekFrom, Write},
    sync::Arc,
    time::Duration,
};

/// httpx `Timeout(30.0)` for the HEAD, `Timeout(30.0, read=120.0)` for the transfers.
const CONNECT: Duration = Duration::from_secs(30);
const HEAD_READ: Duration = Duration::from_secs(30);
const GET_READ: Duration = Duration::from_secs(120);
/// httpx's `max_redirects`.
const MAX_REDIRECTS: usize = 20;
/// The error body Python quotes is its first 200 characters; this much is read for them.
const ERROR_BODY_CAP: usize = 4096;
const TOO_MANY_REDIRECTS: &str = "TooManyRedirects: Exceeded maximum allowed redirects.";

pub(super) struct Http {
    head: reqwest::Client,
    get: reqwest::Client,
}

impl Http {
    pub(super) fn new() -> Result<Self, &'static str> {
        let build = |read| {
            reqwest::Client::builder()
                .connect_timeout(CONNECT)
                .read_timeout(read)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| "download client could not be built")
        };
        Ok(Self {
            head: build(HEAD_READ)?,
            get: build(GET_READ)?,
        })
    }
}

/// How a transfer ended early: the job's cancel, or Python's `"{type}: {message}"`. `Fatal`
/// is an exception that is not an `httpx.HTTPError` (the HEAD does not swallow it).
enum Stop {
    Cancelled,
    Failed(String),
    Fatal(String),
}

fn failed(kind: &str, message: impl std::fmt::Display) -> Stop {
    Stop::Failed(format!("{kind}: {message}"))
}

/// `repr(str)` for the characters a header can decode to.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..=0xa0).contains(&(c as u32)) || c == '\u{ad}' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// An `OSError` as Python prints it: `"{Class}: [Errno n] {strerror}: '{path}'"`.
fn os_error(e: &std::io::Error, path: &str, other: Option<&str>) -> Stop {
    let Some(errno) = e.raw_os_error() else {
        return failed("OSError", e);
    };
    let kind = match rustix::io::Errno::from_raw_os_error(errno) {
        rustix::io::Errno::NOENT => "FileNotFoundError",
        rustix::io::Errno::ACCESS | rustix::io::Errno::PERM => "PermissionError",
        rustix::io::Errno::ISDIR => "IsADirectoryError",
        rustix::io::Errno::NOTDIR => "NotADirectoryError",
        rustix::io::Errno::EXIST => "FileExistsError",
        _ => "OSError",
    };
    let text = e.to_string();
    let strerror = text
        .rsplit_once(" (os error ")
        .map_or(text.as_str(), |(head, _)| head);
    let mut message = format!("[Errno {errno}] {strerror}: {}", py_repr(path));
    if let Some(other) = other {
        message.push_str(&format!(" -> {}", py_repr(other)));
    }
    failed(kind, message)
}

/// What an httpx error is named as. Connection refusals read as httpx's own text; other
/// transport failures keep the class name with a native description.
fn http_error(e: &reqwest::Error) -> Stop {
    if e.is_connect() {
        return failed("ConnectError", "All connection attempts failed");
    }
    if e.is_timeout() {
        return failed("ReadTimeout", "timed out");
    }
    if e.is_builder() {
        return failed("UnsupportedProtocol", "Request URL is not supported");
    }
    failed("RemoteProtocolError", e)
}

/// httpx `Headers.get`: values of one name joined with `", "`, decoded as httpx decodes the
/// whole header set (ASCII, else UTF-8, else Latin-1).
fn header_get(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    let all: Vec<&[u8]> = headers
        .iter()
        .flat_map(|(k, v)| [k.as_str().as_bytes(), v.as_bytes()])
        .collect();
    let utf8 = all.iter().all(|b| std::str::from_utf8(b).is_ok());
    let decode = |b: &[u8]| -> String {
        if utf8 {
            String::from_utf8_lossy(b).into_owned()
        } else {
            b.iter().map(|&c| char::from(c)).collect()
        }
    };
    let values: Vec<String> = headers
        .get_all(name)
        .iter()
        .map(|v| decode(v.as_bytes()))
        .collect();
    if values.is_empty() {
        None
    } else {
        Some(values.join(", "))
    }
}

/// `int(x or 0)` for a Content-Length httpx has already validated.
fn length(headers: &reqwest::header::HeaderMap) -> i64 {
    header_get(headers, "content-length")
        .and_then(|v| pyfmt::py_strip(&v).parse().ok())
        .unwrap_or(0)
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

impl Downloads {
    /// Python's `_auth_headers(url)` as the request hook applies it on every hop.
    fn token_for(&self, url: &str) -> Result<Option<reqwest::header::HeaderValue>, Stop> {
        if !self.config.token_origin.allows(url) {
            return Ok(None);
        }
        match super::store::hub_auth(&self.config.database) {
            super::store::HubAuth::Absent => Ok(None),
            super::store::HubAuth::Value(v) => Ok(Some(v)),
            super::store::HubAuth::NotAscii => Err(Stop::Fatal(
                "UnicodeEncodeError: 'ascii' codec can't encode the Hugging Face token".into(),
            )),
            super::store::HubAuth::Refused => {
                Err(failed("LocalProtocolError", "Illegal header value"))
            }
            super::store::HubAuth::Failed => Err(Stop::Fatal(
                "OperationalError: the settings store could not be read".into(),
            )),
        }
    }

    /// One request with httpx's redirect following (`follow_redirects=True`): the final
    /// response and its URL.
    async fn send(
        &self,
        client: &reqwest::Client,
        method: reqwest::Method,
        url: &str,
        extra: &[(&str, String)],
    ) -> Result<(reqwest::Response, String), Stop> {
        let mut url = url.to_owned();
        let mut method = method;
        let mut redirects = 0;
        loop {
            let mut request = client.request(method.clone(), &url);
            for (name, value) in extra {
                request = request.header(*name, value);
            }
            if let Some(token) = self.token_for(&url)? {
                request = request.header("authorization", token);
            }
            let response = request.send().await.map_err(|e| http_error(&e))?;
            let status = response.status().as_u16();
            let location = response.headers().get("location").cloned();
            match location {
                Some(location) if is_redirect(status) => {
                    redirects += 1;
                    if redirects > MAX_REDIRECTS {
                        return Err(Stop::Failed(TOO_MANY_REDIRECTS.into()));
                    }
                    let base = reqwest::Url::parse(&url).map_err(|_| {
                        failed("UnsupportedProtocol", "Request URL is not supported")
                    })?;
                    let target = location
                        .to_str()
                        .ok()
                        .and_then(|l| base.join(l).ok())
                        .ok_or_else(|| {
                            failed("RemoteProtocolError", "Invalid URL in location header")
                        })?;
                    if status == 303 && method != reqwest::Method::HEAD {
                        method = reqwest::Method::GET;
                    }
                    url = target.to_string();
                }
                _ => return Ok((response, url)),
            }
        }
    }

    /// `_run(job)`: one permit of `max_concurrent_downloads`, then the transfer, the size check
    /// and the rename; any cancel ends it at once, as `task.cancel()` does.
    pub(super) async fn run(self: Arc<Self>, job: Arc<Job>) {
        let mut cancel = job.cancel.subscribe();
        let work = async {
            let _permit = self
                .permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| failed("RuntimeError", "download slots closed"))?;
            {
                let mut s = job.state();
                s.status = "downloading";
                s.started_at = Some(self.now());
            }
            self.stream(&job).await?;
            // From here on Python has no await: nothing can interrupt the size check or rename.
            self.verify_size(&job)?;
            std::fs::rename(&job.temp, &job.dest)
                .map_err(|e| os_error(&e, &job.temp, Some(&job.dest)))?;
            drop_validator(&job);
            Ok::<(), Stop>(())
        };
        let outcome = tokio::select! {
            biased;
            _ = cancel.wait_for(|c| *c) => Err(Stop::Cancelled),
            r = work => r,
        };
        let now = self.now();
        match outcome {
            Ok(()) => {
                let mut s = job.state();
                s.status = "done";
                s.completed_at = Some(now);
            }
            Err(Stop::Cancelled) => {
                {
                    let mut s = job.state();
                    s.status = "canceled";
                    s.completed_at = Some(now);
                }
                let _ = std::fs::remove_file(&job.temp);
                drop_validator(&job);
            }
            Err(Stop::Failed(e) | Stop::Fatal(e)) => {
                let mut s = job.state();
                s.status = "error";
                s.error = Some(e);
                s.completed_at = Some(now);
            }
        }
        let (status, error, total, started, completed) = {
            let s = job.state();
            (
                s.status,
                s.error.clone(),
                s.total,
                s.started_at,
                s.completed_at,
            )
        };
        let started = started.filter(|t| *t != 0.0).unwrap_or_else(|| self.now());
        let database = self.config.database.clone();
        let (repo, filename, dest) = (job.repo_id.clone(), job.filename.clone(), job.dest.clone());
        let _ = tokio::task::spawn_blocking(move || {
            super::store::record(
                &database,
                &repo,
                &filename,
                &dest,
                total,
                status,
                error.as_deref(),
                started,
                completed,
            )
        })
        .await;
    }

    /// `_stream(job)`.
    async fn stream(&self, job: &Job) -> Result<(), Stop> {
        if let Some(parent) = std::path::Path::new(&job.dest).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| os_error(&e, &parent.to_string_lossy(), None))?;
        }
        let mut total = 0;
        let mut ranges = false;
        let mut resolved = job.url.clone();
        let mut current = String::new();
        // `except httpx.HTTPError: pass`: only what is not an httpx error ends the job here.
        let head = match self
            .send(&self.http.head, reqwest::Method::HEAD, &job.url, &[])
            .await
        {
            Ok(head) => Some(head),
            Err(Stop::Fatal(e)) => return Err(Stop::Fatal(e)),
            Err(_) => None,
        };
        if let Some((head, url)) = head {
            if head.status().as_u16() < 400 {
                total = length(head.headers());
                ranges = header_get(head.headers(), "accept-ranges")
                    .is_some_and(|v| v.to_lowercase() == "bytes");
                resolved = url;
                let headers = head.headers().clone();
                current = pyfmt::validator(&|name| header_get(&headers, name), self.now());
            }
        }
        if total > 0 {
            job.state().total = total;
        }
        if ranges && total >= self.config.parallel_min && super::PARALLEL_CHUNKS > 1 {
            self.parallel(job, &resolved, total).await
        } else {
            self.single(job, &current, 0).await
        }
    }

    /// `_stream_parallel(job, url, headers, total)`.
    async fn parallel(&self, job: &Job, url: &str, total: i64) -> Result<(), Stop> {
        drop_validator(job);
        {
            let file =
                std::fs::File::create(&job.temp).map_err(|e| os_error(&e, &job.temp, None))?;
            file.set_len(u64::try_from(total).unwrap_or(0))
                .map_err(|e| os_error(&e, &job.temp, None))?;
        }
        let now = self.now();
        {
            let mut s = job.state();
            s.downloaded = 0;
            s.samples.push((now, 0));
            s.parallel = true;
            let n = super::PARALLEL_CHUNKS;
            let chunk = total.div_euclid(n);
            s.chunks = (0..n)
                .map(|i| {
                    let start = i * chunk;
                    let end = if i < n - 1 {
                        start + chunk - 1
                    } else {
                        total - 1
                    };
                    Chunk {
                        idx: i,
                        start,
                        end,
                        downloaded: 0,
                        samples: Vec::new(),
                    }
                })
                .collect();
        }
        let count = job.state().chunks.len();
        let mut workers: futures_util::stream::FuturesUnordered<_> = (0..count)
            .map(|i| self.worker(job, url, i, total))
            .collect();
        let mut outcome = Ok(());
        while let Some(r) = workers.next().await {
            if let Err(e) = r {
                outcome = Err(e);
                break;
            }
        }
        drop(workers);
        if outcome.is_err() {
            let _ = std::fs::remove_file(&job.temp);
        }
        outcome
    }

    /// One parallel range (`worker(cs)`).
    async fn worker(&self, job: &Job, url: &str, i: usize, total: i64) -> Result<(), Stop> {
        let (start, end, size) = {
            let s = job.state();
            let c = s
                .chunks
                .get(i)
                .ok_or_else(|| failed("IndexError", "list index out of range"))?;
            (c.start, c.end, c.size())
        };
        let range = format!("bytes={start}-{end}");
        let (response, _) = self
            .send(
                &self.http.get,
                reqwest::Method::GET,
                url,
                &[("range", range)],
            )
            .await?;
        let status = response.status().as_u16();
        if status != 206 {
            return Err(failed(
                "RuntimeError",
                format!("HTTP {status} for range {start}-{end}"),
            ));
        }
        let content_range = header_get(response.headers(), "content-range").unwrap_or_default();
        if parse_range(&content_range, false) != Some((start, end, Some(total))) {
            return Err(failed(
                "RuntimeError",
                format!("unexpected Content-Range for range {start}-{end}"),
            ));
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&job.temp)
            .map_err(|e| os_error(&e, &job.temp, None))?;
        file.seek(SeekFrom::Start(u64::try_from(start).unwrap_or(0)))
            .map_err(|e| os_error(&e, &job.temp, None))?;
        let declared = length(response.headers());
        let mut received: i64 = 0;
        let mut chunker = Chunker::default();
        let mut body = response.bytes_stream();
        let mut done = false;
        while !done {
            let pieces = match body.next().await {
                Some(piece) => {
                    let piece = piece.map_err(|e| body_error(&e, received, declared))?;
                    received += i64::try_from(piece.len()).unwrap_or(i64::MAX);
                    chunker.push(&piece)
                }
                None => {
                    done = true;
                    chunker.flush().into_iter().collect()
                }
            };
            for piece in pieces {
                self.write_range(job, i, &mut file, &piece, (start, end, size))?;
            }
        }
        let downloaded = job.state().chunks.get(i).map_or(0, |c| c.downloaded);
        if downloaded != size {
            return Err(failed(
                "RuntimeError",
                format!("incomplete range {start}-{end}: received {downloaded} of {size} bytes"),
            ));
        }
        Ok(())
    }

    /// One piece of a parallel range: the overflow check, the write and the progress.
    fn write_range(
        &self,
        job: &Job,
        i: usize,
        file: &mut std::fs::File,
        piece: &[u8],
        (start, end, size): (i64, i64, i64),
    ) -> Result<(), Stop> {
        let n = i64::try_from(piece.len()).unwrap_or(i64::MAX);
        let downloaded = job.state().chunks.get(i).map_or(0, |c| c.downloaded);
        if downloaded + n > size {
            return Err(failed(
                "RuntimeError",
                format!("too many bytes for range {start}-{end}"),
            ));
        }
        file.write_all(piece)
            .map_err(|e| os_error(&e, &job.temp, None))?;
        let now = self.now();
        let mut s = job.state();
        if let Some(c) = s.chunks.get_mut(i) {
            c.downloaded += n;
            if c.samples
                .last()
                .is_none_or(|(t, _)| now - t >= super::SPEED_SAMPLE_INTERVAL_S)
            {
                c.samples.push((now, c.downloaded));
                if c.samples.len() > 12 {
                    let cut = c.samples.len() - 12;
                    c.samples.drain(..cut);
                }
            }
        }
        s.progress(n, now);
        Ok(())
    }

    /// `_verify_size(job)`.
    fn verify_size(&self, job: &Job) -> Result<(), Stop> {
        let total = job.state().total;
        if total <= 0 {
            return Ok(());
        }
        let size = std::fs::metadata(&job.temp)
            .map_err(|e| os_error(&e, &job.temp, None))?
            .len();
        let size = i64::try_from(size).unwrap_or(i64::MAX);
        if size == total {
            return Ok(());
        }
        if size > total {
            let _ = std::fs::remove_file(&job.temp);
            drop_validator(job);
            return Err(failed(
                "RuntimeError",
                format!(
                    "downloaded file is {size} bytes, expected {total}; discarded it, nothing was installed"
                ),
            ));
        }
        Err(failed(
            "RuntimeError",
            format!("download incomplete: {size} of {total} bytes; nothing was installed (retry to resume)"),
        ))
    }

    /// `_stream_single(job, url, headers, current)`; `depth` bounds the one restart Python can
    /// make (a 416, or a 206 for another upload).
    async fn single(&self, job: &Job, current: &str, depth: usize) -> Result<(), Stop> {
        let mut start =
            std::fs::metadata(&job.temp).map_or(0, |m| i64::try_from(m.len()).unwrap_or(i64::MAX));
        let (stored, stored_total) = read_validator(job)?;
        let total = job.state().total;
        if start != 0
            && (stored.is_empty()
                || (!current.is_empty() && current != stored)
                || (stored_total != 0 && total != 0 && stored_total != total)
                || (total != 0 && start >= total))
        {
            start = 0;
        }
        let mut extra = Vec::new();
        if start > 0 {
            extra.push(("range", format!("bytes={start}-")));
            extra.push(("if-range", stored.clone()));
        }
        {
            let now = self.now();
            let mut s = job.state();
            s.downloaded = start;
            let d = s.downloaded;
            s.samples.push((now, d));
        }
        let (response, _) = self
            .send(&self.http.get, reqwest::Method::GET, &job.url, &extra)
            .await?;
        let status = response.status().as_u16();
        let restart = |job: &Job| {
            let _ = std::fs::remove_file(&job.temp);
            drop_validator(job);
            let now = self.now();
            job.state().restart(now);
        };
        if status == 416 && start > 0 && depth == 0 {
            restart(job);
            drop(response);
            return Box::pin(self.single(job, current, depth + 1)).await;
        }
        if status != 200 && status != 206 {
            let text = error_text(response).await;
            return Err(failed(
                "RuntimeError",
                format!("HTTP {status} from {}: {text}", job.url),
            ));
        }
        let headers = response.headers().clone();
        let append = if status == 206 {
            if start == 0 {
                return Err(failed(
                    "RuntimeError",
                    format!("HTTP 206 from {} for a request without a Range", job.url),
                ));
            }
            let raw = header_get(&headers, "content-range").unwrap_or_default();
            let parsed = parse_range(pyfmt::py_strip(&raw), true);
            let Some((first, _, remote)) = parsed.filter(|(first, _, _)| *first == start) else {
                return Err(failed(
                    "RuntimeError",
                    format!(
                        "unexpected Content-Range {} resuming at byte {start}",
                        py_repr(&raw)
                    ),
                ));
            };
            let _ = first;
            match remote {
                Some(remote) => {
                    let total = job.state().total;
                    let expected: Vec<i64> = [stored_total, total]
                        .into_iter()
                        .filter(|t| *t != 0)
                        .collect();
                    if !expected.is_empty() && expected.iter().any(|t| *t != remote) && depth == 0 {
                        restart(job);
                        drop(response);
                        return Box::pin(self.single(job, current, depth + 1)).await;
                    }
                    job.state().total = remote;
                }
                None => {
                    let mut s = job.state();
                    if s.total == 0
                        && header_get(&headers, "content-length").is_some_and(|v| !v.is_empty())
                    {
                        s.total = start + length(&headers);
                    }
                }
            }
            true
        } else {
            if start != 0 {
                let now = self.now();
                job.state().restart(now);
            }
            let declared = length(&headers);
            {
                let mut s = job.state();
                if s.total == 0 && declared != 0 {
                    s.total = declared;
                }
            }
            drop_validator(job);
            false
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(&job.temp)
            .map_err(|e| os_error(&e, &job.temp, None))?;
        if !append {
            let value = pyfmt::validator(&|name| header_get(&headers, name), self.now());
            let total = job.state().total;
            write_validator(job, &value, total)?;
        }
        let declared = length(&headers);
        let mut received: i64 = 0;
        let mut chunker = Chunker::default();
        let mut body = response.bytes_stream();
        let mut done = false;
        while !done {
            let pieces = match body.next().await {
                Some(piece) => {
                    let piece = piece.map_err(|e| body_error(&e, received, declared))?;
                    received += i64::try_from(piece.len()).unwrap_or(i64::MAX);
                    chunker.push(&piece)
                }
                None => {
                    done = true;
                    chunker.flush().into_iter().collect()
                }
            };
            for piece in pieces {
                file.write_all(&piece)
                    .map_err(|e| os_error(&e, &job.temp, None))?;
                let now = self.now();
                job.state()
                    .progress(i64::try_from(piece.len()).unwrap_or(i64::MAX), now);
            }
        }
        Ok(())
    }
}

/// httpx's `aiter_bytes(1024 * 1024)` (`ByteChunker`): the body is handed on in whole MiB
/// pieces, and the remainder only when the body ends cleanly. A body that fails part way loses
/// what was buffered, exactly as Python's partial file does.
const PIECE: usize = 1024 * 1024;

#[derive(Default)]
struct Chunker {
    buffer: Vec<u8>,
}

impl Chunker {
    fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(data);
        let mut out = Vec::new();
        while self.buffer.len() >= PIECE {
            let rest = self.buffer.split_off(PIECE);
            out.push(std::mem::replace(&mut self.buffer, rest));
        }
        out
    }

    fn flush(&mut self) -> Option<Vec<u8>> {
        (!self.buffer.is_empty()).then(|| std::mem::take(&mut self.buffer))
    }
}

/// A body that ended early reads as httpx's text for a closed connection.
fn body_error(e: &reqwest::Error, received: i64, declared: i64) -> Stop {
    if e.is_timeout() {
        return failed("ReadTimeout", "timed out");
    }
    if declared > 0 && received < declared {
        return failed(
            "RemoteProtocolError",
            format!(
                "peer closed connection without sending complete message body (received {received} bytes, expected {declared})"
            ),
        );
    }
    failed("RemoteProtocolError", e)
}

/// `(await resp.aread()).decode(errors="replace")[:200]`, or `""` when reading fails.
async fn error_text(response: reqwest::Response) -> String {
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(piece) = body.next().await {
        let Ok(piece) = piece else {
            return String::new();
        };
        bytes.extend_from_slice(&piece);
        if bytes.len() >= ERROR_BODY_CAP {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).chars().take(200).collect()
}

/// `bytes (\d+)-(\d+)/(\d+)` (or `\*` for the total when `star`), matched in full.
fn parse_range(text: &str, star: bool) -> Option<(i64, i64, Option<i64>)> {
    let rest = text.strip_prefix("bytes ")?;
    let (first, rest) = rest.split_once('-')?;
    let (last, total) = rest.split_once('/')?;
    let number = |s: &str| -> Option<i64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let total = if star && total == "*" {
        None
    } else {
        Some(number(total)?)
    };
    Some((number(first)?, number(last)?, total))
}

/// `_read_validator(job)`: `("", 0)` when there is none.
fn read_validator(job: &Job) -> Result<(String, i64), Stop> {
    let path = pyfmt::validator_path(&job.temp);
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok((String::new(), 0));
    };
    let text = String::from_utf8(bytes).map_err(|_| {
        failed(
            "UnicodeDecodeError",
            "'utf-8' codec can't decode the validator file",
        )
    })?;
    let text = text.trim_matches(|c| c == '\r' || c == '\n');
    let (value, total) = text.split_once('\t').unwrap_or((text, ""));
    let total = pyfmt::py_strip(total);
    let total = if !total.is_empty() && total.bytes().all(|b| b.is_ascii_digit()) {
        total.parse().unwrap_or(0)
    } else {
        0
    };
    Ok((pyfmt::py_strip(value).to_owned(), total))
}

/// `_write_validator(job, value, total)`.
fn write_validator(job: &Job, value: &str, total: i64) -> Result<(), Stop> {
    if !value.is_empty() && !value.contains(['\t', '\r', '\n']) {
        let path = pyfmt::validator_path(&job.temp);
        std::fs::write(&path, format!("{value}\t{}", total.max(0)))
            .map_err(|e| os_error(&e, &path, None))
    } else {
        drop_validator(job);
        Ok(())
    }
}

/// `_drop_validator(job)`.
fn drop_validator(job: &Job) {
    let _ = std::fs::remove_file(pyfmt::validator_path(&job.temp));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chunker_hands_on_whole_mib_pieces() {
        let mut c = Chunker::default();
        assert!(c.push(&[1; 10]).is_empty());
        let out = c.push(&vec![2; PIECE * 2]);
        assert_eq!(out.iter().map(Vec::len).collect::<Vec<_>>(), [PIECE, PIECE]);
        assert_eq!(c.flush().map(|b| b.len()), Some(10));
        assert_eq!(c.flush(), None);
    }

    #[test]
    fn ranges_are_matched_in_full() {
        assert_eq!(parse_range("bytes 0-9/10", false), Some((0, 9, Some(10))));
        assert_eq!(parse_range("bytes 5-9/*", true), Some((5, 9, None)));
        assert_eq!(parse_range("bytes 5-9/*", false), None);
        assert_eq!(parse_range("bytes 0-9/10 ", false), None);
        assert_eq!(parse_range("bytes -1-9/10", false), None);
    }

    #[test]
    fn repr_follows_python() {
        assert_eq!(py_repr("bytes 0-30/31"), "'bytes 0-30/31'");
        assert_eq!(py_repr("a'b"), "\"a'b\"");
        assert_eq!(py_repr("a\tb\u{a0}é"), "'a\\tb\\xa0é'");
    }
}
