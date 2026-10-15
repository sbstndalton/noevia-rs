//! Native conversation export (#1315): the bounded classic stored-ZIP foundation (this file),
//! the Markdown policy ([`markdown`]), buildExport ([`assemble`]) and the route's tenant reads
//! ([`route`]). The HTTP handler, the audit row and the switch live in bins/noevia-server.
//! Bytes follow core9655984 chat-export.cjs zipStore; UTF-8 names, UTC DOS timestamps,
//! no compression, descriptors, comments or extra fields. Payload <=64MiB, <=20,000 entries.
//! Additional fail-closed work bound: all entry names combined <=1MiB (individual <=u16::MAX).
use chrono::{Datelike, Timelike, Utc};

pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 20_000;
pub const MAX_NAME_BYTES: usize = 1024 * 1024;
/// Maximum output allocation, including both name copies and all classic headers.
pub const MAX_OUTPUT_BYTES: usize = MAX_PAYLOAD_BYTES + 2 * MAX_NAME_BYTES + 76 * MAX_ENTRIES + 22;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Limit,
    Timestamp,
    Allocation,
}

pub struct Entry<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
}

struct Record {
    crc: u32,
    size: u32,
    name_size: u16,
    offset: u32,
}

fn u16le(out: &mut Vec<u8>, n: u16) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn u32le(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn add(a: usize, b: usize) -> Result<usize, Error> {
    a.checked_add(b).ok_or(Error::Limit)
}

/// Construct stored ZIP bytes at an explicit timestamp supplied by the caller.
/// No production clock override exists; the eventual handler must supply its current clock.
/// Names are already export-derived paths: this layer deliberately does not invent path policy.
/// It permits duplicate names like Node's low-level zipStore, which delegates collision handling.
pub fn zip_store(entries: &[Entry<'_>], when_ms: i64) -> Result<Vec<u8>, Error> {
    if entries.len() > MAX_ENTRIES {
        return Err(Error::Limit);
    }
    let when = chrono::DateTime::<Utc>::from_timestamp_millis(when_ms).ok_or(Error::Timestamp)?;
    let date = (when.year().max(1980) - 1980)
        .checked_mul(512)
        .and_then(|v| v.checked_add((when.month() * 32 + when.day()) as i32))
        .and_then(|v| u16::try_from(v).ok())
        .ok_or(Error::Timestamp)?;
    let time = u16::try_from(when.hour() * 2048 + when.minute() * 32 + when.second() / 2)
        .map_err(|_| Error::Timestamp)?;
    let (mut payload, mut names, mut local_size, mut central_size) = (0, 0, 0, 0);
    // Refuse all input/work bounds before CRC traversal or allocating output.
    for entry in entries {
        payload = add(payload, entry.data.len())?;
        names = add(names, entry.name.len())?;
        if payload > MAX_PAYLOAD_BYTES
            || names > MAX_NAME_BYTES
            || entry.name.len() > u16::MAX.into()
        {
            return Err(Error::Limit);
        }
        local_size = add(
            local_size,
            add(30, add(entry.name.len(), entry.data.len())?)?,
        )?;
        central_size = add(central_size, add(46, entry.name.len())?)?;
    }
    let output_size = add(add(local_size, central_size)?, 22)?;
    if output_size > MAX_OUTPUT_BYTES {
        return Err(Error::Limit);
    }
    let local_u32 = u32::try_from(local_size).map_err(|_| Error::Limit)?;
    let central_u32 = u32::try_from(central_size).map_err(|_| Error::Limit)?;
    let count = u16::try_from(entries.len()).map_err(|_| Error::Limit)?;
    let mut records = Vec::new();
    records
        .try_reserve_exact(entries.len())
        .map_err(|_| Error::Allocation)?;
    let mut out = Vec::new();
    out.try_reserve_exact(output_size)
        .map_err(|_| Error::Allocation)?;
    for entry in entries {
        let record = Record {
            crc: crc32fast::hash(entry.data),
            size: u32::try_from(entry.data.len()).map_err(|_| Error::Limit)?,
            name_size: u16::try_from(entry.name.len()).map_err(|_| Error::Limit)?,
            offset: u32::try_from(out.len()).map_err(|_| Error::Limit)?,
        };
        u32le(&mut out, 0x04034b50);
        for n in [20, 0x0800, 0, time, date] {
            u16le(&mut out, n);
        }
        u32le(&mut out, record.crc);
        u32le(&mut out, record.size);
        u32le(&mut out, record.size);
        u16le(&mut out, record.name_size);
        u16le(&mut out, 0);
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(entry.data);
        records.push(record);
    }
    for (entry, record) in entries.iter().zip(records.iter()) {
        u32le(&mut out, 0x02014b50);
        for n in [20, 20, 0x0800, 0, time, date] {
            u16le(&mut out, n);
        }
        u32le(&mut out, record.crc);
        u32le(&mut out, record.size);
        u32le(&mut out, record.size);
        for n in [record.name_size, 0, 0, 0, 0] {
            u16le(&mut out, n);
        }
        u32le(&mut out, 0);
        u32le(&mut out, record.offset);
        out.extend_from_slice(entry.name.as_bytes());
    }
    u32le(&mut out, 0x06054b50);
    for n in [0, 0, count, count] {
        u16le(&mut out, n);
    }
    u32le(&mut out, central_u32);
    u32le(&mut out, local_u32);
    u16le(&mut out, 0);
    Ok(out)
}

/// The DOS time and date words of `when_ms` (UTC), as [`zip_store`] writes them.
fn dos(when_ms: i64) -> Result<(u16, u16), Error> {
    let when = chrono::DateTime::<Utc>::from_timestamp_millis(when_ms).ok_or(Error::Timestamp)?;
    let date = (when.year().max(1980) - 1980)
        .checked_mul(512)
        .and_then(|v| v.checked_add((when.month() * 32 + when.day()) as i32))
        .and_then(|v| u16::try_from(v).ok())
        .ok_or(Error::Timestamp)?;
    let time = u16::try_from(when.hour() * 2048 + when.minute() * 32 + when.second() / 2)
        .map_err(|_| Error::Timestamp)?;
    Ok((time, date))
}

/// [`zip_store`]'s bytes, built entry by entry as a list of chunks (#1824): an entry's payload
/// parts are moved in, never copied or concatenated, so the export holds its payload once. The
/// same limits apply, checked as each entry is added; the timestamp is checked (and written into
/// the local headers) by [`ZipWriter::finish`], where `zip_store` checks it.
pub struct ZipWriter {
    chunks: Vec<Vec<u8>>,
    /// Per entry: the index of its local-header chunk, and its central record.
    entries: Vec<(usize, Record, String)>,
    offset: usize,
    payload: usize,
    names: usize,
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ZipWriter {
    pub fn new() -> Self {
        ZipWriter {
            chunks: Vec::new(),
            entries: Vec::new(),
            offset: 0,
            payload: 0,
            names: 0,
        }
    }

    /// One stored entry whose data is `parts` in order.
    pub fn add(&mut self, name: String, parts: Vec<Vec<u8>>) -> Result<(), Error> {
        let size = parts
            .iter()
            .try_fold(0usize, |n, p| n.checked_add(p.len()))
            .ok_or(Error::Limit)?;
        self.payload = add(self.payload, size)?;
        self.names = add(self.names, name.len())?;
        if self.entries.len() >= MAX_ENTRIES
            || self.payload > MAX_PAYLOAD_BYTES
            || self.names > MAX_NAME_BYTES
            || name.len() > u16::MAX.into()
        {
            return Err(Error::Limit);
        }
        let mut crc = crc32fast::Hasher::new();
        for p in &parts {
            crc.update(p);
        }
        let record = Record {
            crc: crc.finalize(),
            size: u32::try_from(size).map_err(|_| Error::Limit)?,
            name_size: u16::try_from(name.len()).map_err(|_| Error::Limit)?,
            offset: u32::try_from(self.offset).map_err(|_| Error::Limit)?,
        };
        let mut local = Vec::new();
        local
            .try_reserve_exact(30 + name.len())
            .map_err(|_| Error::Allocation)?;
        u32le(&mut local, 0x04034b50);
        // Time and date (0 here) are written by finish().
        for n in [20, 0x0800, 0, 0, 0] {
            u16le(&mut local, n);
        }
        u32le(&mut local, record.crc);
        u32le(&mut local, record.size);
        u32le(&mut local, record.size);
        u16le(&mut local, record.name_size);
        u16le(&mut local, 0);
        local.extend_from_slice(name.as_bytes());
        self.offset = add(self.offset, add(local.len(), size)?)?;
        u32::try_from(self.offset).map_err(|_| Error::Limit)?;
        self.entries.push((self.chunks.len(), record, name));
        self.chunks.push(local);
        self.chunks
            .extend(parts.into_iter().filter(|p| !p.is_empty()));
        Ok(())
    }

    /// The ZIP's chunks (concatenated: `zip_store`'s bytes) and its total length.
    pub fn finish(mut self, when_ms: i64) -> Result<(Vec<Vec<u8>>, usize), Error> {
        let (time, date) = dos(when_ms)?;
        let count = u16::try_from(self.entries.len()).map_err(|_| Error::Limit)?;
        let mut central = Vec::new();
        for (at, record, name) in &self.entries {
            if let Some(local) = self.chunks.get_mut(*at) {
                if let Some(slot) = local.get_mut(10..14) {
                    slot.copy_from_slice(&[time.to_le_bytes(), date.to_le_bytes()].concat());
                }
            }
            u32le(&mut central, 0x02014b50);
            for n in [20, 20, 0x0800, 0, time, date] {
                u16le(&mut central, n);
            }
            u32le(&mut central, record.crc);
            u32le(&mut central, record.size);
            u32le(&mut central, record.size);
            for n in [record.name_size, 0, 0, 0, 0] {
                u16le(&mut central, n);
            }
            u32le(&mut central, 0);
            u32le(&mut central, record.offset);
            central.extend_from_slice(name.as_bytes());
        }
        let central_size = u32::try_from(central.len()).map_err(|_| Error::Limit)?;
        let local_size = u32::try_from(self.offset).map_err(|_| Error::Limit)?;
        u32le(&mut central, 0x06054b50);
        for n in [0, 0, count, count] {
            u16le(&mut central, n);
        }
        u32le(&mut central, central_size);
        u32le(&mut central, local_size);
        u16le(&mut central, 0);
        let total = add(self.offset, central.len())?;
        self.chunks.push(central);
        Ok((self.chunks, total))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn arithmetic_overflow_refuses() {
        assert_eq!(super::add(usize::MAX, 1), Err(super::Error::Limit));
    }
}

/// Bounded native conversation Markdown policy (#1317); no HTTP ownership.
pub mod assemble;
pub mod markdown;
/// `GET /api/export/conversations`: what the route reads and answers (#1315).
pub mod route;
