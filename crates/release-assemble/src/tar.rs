//! A fail-closed tar reader and a deterministic tar writer (no new dependencies).
//!
//! Reader: what `tar -xzf TGZ --strip-components=N -C DEST` does with the codeload and `git archive`
//! tarballs a release is built from (ustar names with prefix, pax `x`/`g` records, GNU `L`/`K` long
//! names; regular files, directories and symbolic links). Where GNU tar would warn, skip or follow, this
//! refuses the whole archive instead: an absolute or `..` member, a member that would be written through
//! a symlink extracted earlier, a hardlink, a device or fifo, a bad header checksum, a truncated archive.
//!
//! Writer: entries in the given order, directories recursed in byte order of their names, every header
//! with uid/gid 0, empty user and group names and one fixed mtime; modes are 0755 for directories and
//! executables, 0644 for other files, 0777 for symlinks. Names longer than the ustar fields get a pax
//! `x` record (`path`, `linkpath`, `size`), which GNU tar, bsdtar and Docker all read.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

const BLOCK: usize = 512;

/// One member as the reader saw it (after pax/GNU names are applied, before strip).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    File { executable: bool, data: Vec<u8> },
    Dir,
    Symlink { target: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The member name, bytes, without a trailing `/`.
    pub name: Vec<u8>,
    pub kind: Kind,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn field(h: &[u8; BLOCK], start: usize, len: usize) -> &[u8] {
    h.get(start..start + len).unwrap_or_default()
}

fn cstr(b: &[u8]) -> &[u8] {
    match b.iter().position(|c| *c == 0) {
        Some(i) => b.get(..i).unwrap_or_default(),
        None => b,
    }
}

/// An octal numeric field (spaces/NULs around it), or GNU base-256 for large sizes.
fn number(b: &[u8]) -> io::Result<u64> {
    if let Some(first) = b.first() {
        if first & 0x80 != 0 {
            let mut v: u64 = u64::from(first & 0x7f);
            for c in b.get(1..).unwrap_or_default() {
                v = v
                    .checked_mul(256)
                    .and_then(|v| v.checked_add(u64::from(*c)))
                    .ok_or_else(|| bad("tar: number overflow"))?;
            }
            return Ok(v);
        }
    }
    let s = cstr(b);
    let s: Vec<u8> = s.iter().copied().filter(|c| *c != b' ').collect();
    if s.is_empty() {
        return Ok(0);
    }
    let mut v: u64 = 0;
    for c in s {
        if !(b'0'..=b'7').contains(&c) {
            return Err(bad("tar: bad octal field"));
        }
        v = v
            .checked_mul(8)
            .and_then(|v| v.checked_add(u64::from(c - b'0')))
            .ok_or_else(|| bad("tar: number overflow"))?;
    }
    Ok(v)
}

fn checksum_ok(h: &[u8; BLOCK]) -> io::Result<bool> {
    let want = number(field(h, 148, 8))?;
    let mut unsigned: u64 = 0;
    let mut signed: i64 = 0;
    for (i, c) in h.iter().enumerate() {
        let c = if (148..156).contains(&i) { b' ' } else { *c };
        unsigned += u64::from(c);
        signed += i64::from(c as i8);
    }
    Ok(want == unsigned || i64::try_from(want).ok() == Some(signed))
}

fn read_block(r: &mut dyn Read, buf: &mut [u8; BLOCK]) -> io::Result<bool> {
    let mut got = 0;
    while got < BLOCK {
        let n = r.read(buf.get_mut(got..).unwrap_or_default())?;
        if n == 0 {
            if got == 0 {
                return Ok(false);
            }
            return Err(bad("tar: truncated header"));
        }
        got += n;
    }
    Ok(true)
}

/// Size caps of one read (noevia#2726 item 1): a header's size field (octal, base-256 or pax, up to 64
/// bits) is checked against them BEFORE anything is allocated, and the running total of member data
/// against `total`, so a hostile or broken archive is refused instead of aborting the process on memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// One regular file's data.
    pub member: u64,
    /// Every member's data together (the reader keeps the members in memory).
    pub total: u64,
    /// A pax `x`/`g` record block or a GNU `L`/`K` long name.
    pub meta: u64,
    /// Members (headers of any kind), so a stream of empty headers cannot grow the list without end.
    pub members: u64,
}

impl Default for Limits {
    /// Well above today's release inputs (the largest component tarball inflates to a few hundred MiB).
    fn default() -> Self {
        Limits {
            member: 1 << 30,
            total: 4 << 30,
            meta: 1 << 20,
            members: 2_000_000,
        }
    }
}

struct Budget {
    limits: Limits,
    used: u64,
    headers: u64,
}

impl Budget {
    fn header(&mut self) -> io::Result<()> {
        self.headers += 1;
        if self.headers > self.limits.members {
            return Err(bad(format!(
                "tar: more than {} members",
                self.limits.members
            )));
        }
        Ok(())
    }

    fn take(&mut self, size: u64, cap: u64, what: &str) -> io::Result<()> {
        if size > cap {
            return Err(bad(format!(
                "tar: {what} of {size} bytes is over the {cap}-byte cap"
            )));
        }
        let used = self.used.saturating_add(size);
        if used > self.limits.total {
            return Err(bad(format!(
                "tar: the archive's data is over the {}-byte cap",
                self.limits.total
            )));
        }
        self.used = used;
        Ok(())
    }
}

fn read_data(r: &mut dyn Read, size: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(size).map_err(|_| bad("tar: member too large"))?;
    let mut data = Vec::new();
    // `take` + `read_to_end` grows with the bytes that really arrive: a size field larger than the stream
    // is a truncation, never a large up-front allocation.
    Read::take(&mut *r, size).read_to_end(&mut data)?;
    if data.len() != len {
        return Err(bad("tar: truncated member"));
    }
    let pad = (BLOCK - len % BLOCK) % BLOCK;
    let mut skip = vec![0u8; pad];
    r.read_exact(&mut skip)
        .map_err(|_| bad("tar: truncated member"))?;
    Ok(data)
}

/// pax records `LEN key=value\n`.
fn pax(data: &[u8]) -> io::Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut rest = data;
    while !rest.is_empty() {
        let sp = rest
            .iter()
            .position(|c| *c == b' ')
            .ok_or_else(|| bad("tar: bad pax record"))?;
        let len: usize = std::str::from_utf8(rest.get(..sp).unwrap_or_default())
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad("tar: bad pax length"))?;
        let rec = rest.get(..len).ok_or_else(|| bad("tar: bad pax length"))?;
        if len <= sp + 1 || rec.last() != Some(&b'\n') {
            return Err(bad("tar: bad pax record"));
        }
        let body = rec.get(sp + 1..len - 1).unwrap_or_default();
        let eq = body
            .iter()
            .position(|c| *c == b'=')
            .ok_or_else(|| bad("tar: bad pax record"))?;
        let key = String::from_utf8_lossy(body.get(..eq).unwrap_or_default()).into_owned();
        out.insert(key, body.get(eq + 1..).unwrap_or_default().to_vec());
        rest = rest.get(len..).unwrap_or_default();
    }
    Ok(out)
}

/// Read every member of an uncompressed tar stream, under the default [`Limits`].
pub fn read_all(r: &mut dyn Read) -> io::Result<Vec<Entry>> {
    read_all_limited(r, Limits::default())
}

/// [`read_all`] under explicit caps.
pub fn read_all_limited(r: &mut dyn Read, limits: Limits) -> io::Result<Vec<Entry>> {
    let mut budget = Budget {
        limits,
        used: 0,
        headers: 0,
    };
    let mut out = Vec::new();
    let mut h = [0u8; BLOCK];
    let mut long_name: Option<Vec<u8>> = None;
    let mut long_link: Option<Vec<u8>> = None;
    let mut local: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut global: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut ended = false;
    while read_block(r, &mut h)? {
        if h.iter().all(|c| *c == 0) {
            ended = true;
            break;
        }
        if !checksum_ok(&h)? {
            return Err(bad("tar: header checksum mismatch"));
        }
        budget.header()?;
        let typeflag = h.get(156).copied().unwrap_or(0);
        let pax_size = local.get("size").or_else(|| global.get("size")).cloned();
        let size = match pax_size {
            Some(v) => String::from_utf8_lossy(&v)
                .parse::<u64>()
                .map_err(|_| bad("tar: bad pax size"))?,
            None => number(field(&h, 124, 12))?,
        };
        match typeflag {
            b'x' => {
                let n = number(field(&h, 124, 12))?;
                budget.take(n, limits.meta, "a pax record block")?;
                local = pax(&read_data(r, n)?)?;
                continue;
            }
            b'g' => {
                let n = number(field(&h, 124, 12))?;
                budget.take(n, limits.meta, "a pax record block")?;
                for (k, v) in pax(&read_data(r, n)?)? {
                    global.insert(k, v);
                }
                continue;
            }
            b'L' => {
                budget.take(size, limits.meta, "a long name")?;
                long_name = Some(cstr(&read_data(r, size)?).to_vec());
                continue;
            }
            b'K' => {
                budget.take(size, limits.meta, "a long link name")?;
                long_link = Some(cstr(&read_data(r, size)?).to_vec());
                continue;
            }
            _ => {}
        }
        let ustar = field(&h, 257, 5) == b"ustar";
        let mut name = cstr(field(&h, 0, 100)).to_vec();
        if ustar {
            let prefix = cstr(field(&h, 345, 155));
            if !prefix.is_empty() && field(&h, 263, 2) == b"00" {
                let mut full = prefix.to_vec();
                full.push(b'/');
                full.extend_from_slice(&name);
                name = full;
            }
        }
        if let Some(n) = long_name.take() {
            name = n;
        }
        if let Some(n) = local.get("path").or_else(|| global.get("path")) {
            name = n.clone();
        }
        let mut link = cstr(field(&h, 157, 100)).to_vec();
        if let Some(l) = long_link.take() {
            link = l;
        }
        if let Some(l) = local.get("linkpath").or_else(|| global.get("linkpath")) {
            link = l.clone();
        }
        local.clear();
        let mode = number(field(&h, 100, 8))?;
        // A directory or a link carries no data GNU tar uses, but its size field is still read past.
        let cap = if matches!(typeflag, b'0' | 0 | b'7') {
            limits.member
        } else {
            limits.meta
        };
        budget.take(size, cap, "a member")?;
        let kind = match typeflag {
            b'0' | 0 | b'7' => Kind::File {
                executable: mode & 0o111 != 0,
                data: read_data(r, size)?,
            },
            b'5' => {
                read_data(r, size)?;
                Kind::Dir
            }
            b'2' => {
                read_data(r, size)?;
                Kind::Symlink { target: link }
            }
            b'1' => return Err(bad("tar: hardlink members are refused")),
            other => {
                return Err(bad(format!(
                    "tar: member type {:?} is refused",
                    char::from(other)
                )))
            }
        };
        while name.last() == Some(&b'/') {
            name.pop();
        }
        out.push(Entry { name, kind });
    }
    if !ended && !out.is_empty() {
        // GNU tar accepts a missing end-of-archive marker with a warning; a release tarball always has one.
        return Err(bad("tar: missing end-of-archive marker"));
    }
    Ok(out)
}

/// Read a gzip-compressed tar stream (`tar -xz`), under the default [`Limits`].
pub fn read_gz(r: &mut dyn Read) -> io::Result<Vec<Entry>> {
    read_gz_limited(r, Limits::default())
}

/// [`read_gz`] under explicit caps. The inflated stream is read through the caps above (headers are
/// 512-byte blocks and data is counted), so a gzip bomb ends at the total cap, not at the memory limit.
pub fn read_gz_limited(r: &mut dyn Read, limits: Limits) -> io::Result<Vec<Entry>> {
    let mut gz = flate2::read::MultiGzDecoder::new(r);
    read_all_limited(&mut gz, limits)
}

/// Member name components, refused when absolute, empty or with `..` (GNU tar strips or skips those).
fn components(name: &[u8]) -> io::Result<Vec<&[u8]>> {
    if name.first() == Some(&b'/') {
        return Err(bad(format!(
            "tar: absolute member {}",
            String::from_utf8_lossy(name)
        )));
    }
    let parts: Vec<&[u8]> = name
        .split(|c| *c == b'/')
        .filter(|p| !p.is_empty() && *p != b".")
        .collect();
    if parts.iter().any(|p| *p == b"..") {
        return Err(bad(format!(
            "tar: member with '..': {}",
            String::from_utf8_lossy(name)
        )));
    }
    Ok(parts)
}

/// noevia#2726 item 3: GNU tar writes a symlink's target as it is, absolute or pointing out of the tree.
/// `pack` and the image build read the extracted tree back, so a target that is empty, absolute or that
/// climbs above the extraction root (read lexically from the link's own directory, `depth` = the link's
/// component count after the strip), or that has a `..` after a named component, is refused. No release input has a symlink today (all five repos
/// checked at the time of writing), so this only closes a door.
fn link_stays_inside(depth: usize, target: &[u8]) -> io::Result<()> {
    let show = || String::from_utf8_lossy(target).into_owned();
    if target.is_empty() || target.first() == Some(&b'/') {
        return Err(bad(format!(
            "tar: symlink target {:?} is empty or absolute",
            show()
        )));
    }
    // Directories above the link inside the root. A `..` after a named component is refused too: the
    // named component may itself be a link (`d/s -> ../x`, then `l -> d/s/../..`), so only leading `..`
    // steps are counted, and those climb real directories (extraction never writes through a link).
    let mut at = depth.saturating_sub(1);
    let mut named = false;
    for part in target.split(|c| *c == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                if named {
                    return Err(bad(format!(
                        "tar: symlink target {:?} climbs after a named component",
                        show()
                    )));
                }
                at = at.checked_sub(1).ok_or_else(|| {
                    bad(format!("tar: symlink target {:?} leaves the tree", show()))
                })?;
            }
            _ => named = true,
        }
    }
    Ok(())
}

#[cfg(unix)]
fn os_path(parts: &[&[u8]]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let mut p = PathBuf::new();
    for part in parts {
        p.push(std::ffi::OsStr::from_bytes(part));
    }
    p
}

/// Make every parent of `rel` under `dest` a real directory (never through a symlink).
fn real_parents(dest: &Path, rel: &Path) -> io::Result<()> {
    let mut at = dest.to_path_buf();
    let parents: Vec<Component> = rel
        .parent()
        .map(|p| p.components().collect())
        .unwrap_or_default();
    for c in parents {
        at.push(c);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => return Err(bad(format!("tar: {} is not a directory", at.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&at)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// `tar -x --strip-components=STRIP -C DEST` over decoded members. Members with no more than STRIP
/// components are skipped (GNU tar does the same). Returns the number of members written.
#[cfg(unix)]
pub fn extract(entries: &[Entry], strip: usize, dest: &Path) -> io::Result<usize> {
    use std::os::unix::ffi::OsStrExt;
    let mut written = 0;
    for e in entries {
        let parts = components(&e.name)?;
        let Some(kept) = parts.get(strip..) else {
            continue;
        };
        if kept.is_empty() {
            continue;
        }
        let rel = os_path(kept);
        real_parents(dest, &rel)?;
        let path = dest.join(&rel);
        let existing = fs::symlink_metadata(&path).ok();
        match &e.kind {
            Kind::Dir => match existing {
                Some(m) if m.file_type().is_dir() => {}
                Some(_) => {
                    return Err(bad(format!(
                        "tar: {} exists and is not a directory",
                        path.display()
                    )))
                }
                None => fs::create_dir(&path)?,
            },
            Kind::File { executable, data } => {
                if let Some(m) = existing {
                    if m.file_type().is_dir() {
                        return Err(bad(format!("tar: {} is a directory", path.display())));
                    }
                    fs::remove_file(&path)?;
                }
                fs::write(&path, data)?;
                set_mode(&path, if *executable { 0o755 } else { 0o644 })?;
            }
            Kind::Symlink { target } => {
                // noevia#2738 item 3: refuse before anything on disk changes.
                link_stays_inside(kept.len(), target)?;
                if let Some(m) = existing {
                    if m.file_type().is_dir() {
                        return Err(bad(format!("tar: {} is a directory", path.display())));
                    }
                    fs::remove_file(&path)?;
                }
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), &path)?;
            }
        }
        written += 1;
    }
    Ok(written)
}

/// The deterministic writer.
pub struct Writer<W: Write> {
    out: W,
    mtime: u64,
}

fn octal(buf: &mut [u8], v: u64) {
    // width-1 octal digits, zero padded, NUL terminated
    let width = buf.len().saturating_sub(1);
    let s = format!("{v:0width$o}");
    let bytes = s.as_bytes();
    let n = bytes.len().min(width);
    if let (Some(dst), Some(src)) = (buf.get_mut(..n), bytes.get(bytes.len() - n..)) {
        dst.copy_from_slice(src);
    }
    if let Some(last) = buf.get_mut(width) {
        *last = 0;
    }
}

fn put(h: &mut [u8; BLOCK], start: usize, bytes: &[u8]) {
    if let Some(dst) = h.get_mut(start..start + bytes.len()) {
        dst.copy_from_slice(bytes);
    }
}

fn fits_octal(v: u64, width: usize) -> bool {
    u32::try_from(3 * width)
        .ok()
        .and_then(|bits| 1u64.checked_shl(bits))
        .is_none_or(|limit| v < limit)
}

impl<W: Write> Writer<W> {
    pub fn new(out: W, mtime: u64) -> Self {
        Writer { out, mtime }
    }

    fn header(
        &mut self,
        name: &[u8],
        typeflag: u8,
        mode: u64,
        size: u64,
        link: &[u8],
    ) -> io::Result<()> {
        let mut h = [0u8; BLOCK];
        put(
            &mut h,
            0,
            name.get(..name.len().min(100)).unwrap_or_default(),
        );
        octal(h.get_mut(100..108).unwrap_or_default(), mode);
        octal(h.get_mut(108..116).unwrap_or_default(), 0);
        octal(h.get_mut(116..124).unwrap_or_default(), 0);
        octal(
            h.get_mut(124..136).unwrap_or_default(),
            if fits_octal(size, 11) { size } else { 0 },
        );
        octal(
            h.get_mut(136..148).unwrap_or_default(),
            if fits_octal(self.mtime, 11) {
                self.mtime
            } else {
                0
            },
        );
        put(&mut h, 148, b"        ");
        if let Some(t) = h.get_mut(156) {
            *t = typeflag;
        }
        put(
            &mut h,
            157,
            link.get(..link.len().min(100)).unwrap_or_default(),
        );
        put(&mut h, 257, b"ustar\x0000");
        octal(h.get_mut(329..337).unwrap_or_default(), 0);
        octal(h.get_mut(337..345).unwrap_or_default(), 0);
        let sum: u64 = h.iter().map(|c| u64::from(*c)).sum();
        let s = format!("{sum:06o}\0 ");
        put(&mut h, 148, s.as_bytes());
        self.out.write_all(&h)
    }

    fn pad(&mut self, len: u64) -> io::Result<()> {
        let rem = usize::try_from(len % BLOCK as u64).unwrap_or(0);
        if rem != 0 {
            self.out.write_all(&vec![0u8; BLOCK - rem])?;
        }
        Ok(())
    }

    fn pax_if_needed(&mut self, name: &[u8], link: &[u8], size: u64) -> io::Result<()> {
        let mut records: Vec<u8> = Vec::new();
        let mut add = |key: &str, value: &[u8]| {
            // LEN counts itself: find the fixed point.
            let base = key.len() + value.len() + 3;
            let mut len = base + 1;
            while len.to_string().len() + base != len {
                len = len.to_string().len() + base;
            }
            records.extend_from_slice(format!("{len} {key}=").as_bytes());
            records.extend_from_slice(value);
            records.push(b'\n');
        };
        if name.len() > 100 {
            add("path", name);
        }
        if link.len() > 100 {
            add("linkpath", link);
        }
        if !fits_octal(size, 11) {
            add("size", size.to_string().as_bytes());
        }
        if records.is_empty() {
            return Ok(());
        }
        let len = records.len() as u64;
        self.header(b"././@PaxHeader", b'x', 0o644, len, b"")?;
        self.out.write_all(&records)?;
        self.pad(len)
    }

    /// One regular file member.
    pub fn file(&mut self, name: &[u8], executable: bool, data: &[u8]) -> io::Result<()> {
        let size = data.len() as u64;
        self.pax_if_needed(name, b"", size)?;
        self.header(
            name,
            b'0',
            if executable { 0o755 } else { 0o644 },
            size,
            b"",
        )?;
        self.out.write_all(data)?;
        self.pad(size)
    }

    /// One directory member (`name/`).
    pub fn dir(&mut self, name: &[u8]) -> io::Result<()> {
        let mut n = name.to_vec();
        n.push(b'/');
        self.pax_if_needed(&n, b"", 0)?;
        self.header(&n, b'5', 0o755, 0, b"")
    }

    /// One symlink member.
    pub fn symlink(&mut self, name: &[u8], target: &[u8]) -> io::Result<()> {
        self.pax_if_needed(name, target, 0)?;
        self.header(name, b'2', 0o777, 0, target)
    }

    /// `tar -c -C ROOT REL`: the member and, for a directory, everything under it in byte order.
    #[cfg(unix)]
    pub fn add_path(&mut self, root: &Path, rel: &[u8]) -> io::Result<()> {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(std::ffi::OsStr::from_bytes(rel));
        let meta = fs::symlink_metadata(&path)?;
        let ft = meta.file_type();
        if ft.is_symlink() {
            let target = fs::read_link(&path)?.into_os_string().into_vec();
            self.symlink(rel, &target)
        } else if ft.is_dir() {
            self.dir(rel)?;
            let mut names: Vec<Vec<u8>> = fs::read_dir(&path)?
                .map(|d| d.map(|d| d.file_name().into_vec()))
                .collect::<io::Result<_>>()?;
            names.sort();
            for n in names {
                let mut child = rel.to_vec();
                child.push(b'/');
                child.extend_from_slice(&n);
                self.add_path(root, &child)?;
            }
            Ok(())
        } else if ft.is_file() {
            let data = fs::read(&path)?;
            self.file(rel, meta.permissions().mode() & 0o111 != 0, &data)
        } else {
            Err(bad(format!(
                "tar: {} is not a file, directory or symlink",
                path.display()
            )))
        }
    }

    /// The end-of-archive marker; returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.out.write_all(&[0u8; BLOCK * 2])?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_long_names() {
        let long: Vec<u8> = std::iter::repeat_n(b'a', 150)
            .chain(b"/b".iter().copied())
            .collect();
        let mut w = Writer::new(Vec::new(), 0);
        w.dir(b"top").ok();
        w.file(&long, true, b"x").ok();
        w.symlink(b"top/l", &[b'c'; 120]).ok();
        let bytes = w.finish().unwrap_or_default();
        let entries = read_all(&mut bytes.as_slice()).unwrap_or_default();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries.get(1).map(|e| e.name.clone()), Some(long));
        assert_eq!(
            entries.get(2).map(|e| e.kind.clone()),
            Some(Kind::Symlink {
                target: vec![b'c'; 120]
            })
        );
    }

    #[test]
    fn refuses_dotdot_and_absolute() {
        assert!(components(b"a/../b").is_err());
        assert!(components(b"/etc/x").is_err());
        assert_eq!(
            components(b"./a//b/").ok(),
            Some(vec![&b"a"[..], &b"b"[..]])
        );
    }
}
