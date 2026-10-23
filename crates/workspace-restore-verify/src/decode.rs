//! Bounded, versioned private IPC for a fully verified archive. See ../DECODE-V1.md.
//! Output is not a success receipt until the consumer observes the end marker, EOF AND exit0.
use crate::VerifiedArchive;
use serde_json::json;
use std::io::{self, Write};

pub const DECODE_V1_MAGIC: &[u8] = b"noevia-workspace-decode-v1\0";
pub const DECODE_V1_END: &[u8] = b"\0noevia-workspace-decode-v1-end\0";
/// Includes canonical JSON expansion of an 8 MiB manifest and its checked listing.
/// Source bytes have their separate MAX_TOTAL bound and are not part of metadata.
pub const MAX_DECODE_METADATA: usize = 32 * 1024 * 1024;

struct Metadata(Vec<u8>);
impl Write for Metadata {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_DECODE_METADATA.saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decode metadata exceeds limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl VerifiedArchive {
    /// Serialize decode-v1, raw file bytes once, without touching disk/network. Metadata is
    /// bounded and prepared BEFORE any output. A write/flush failure propagates; callers must
    /// exit nonzero. Consumers MUST stage privately until exact framing, EOF and exit0.
    pub fn write_decode_v1(&self, mut output: impl Write) -> io::Result<()> {
        let listing = self.listing();
        let files: Vec<_> = listing
            .files
            .iter()
            .map(|f| {
                json!({
                    "path": f.path, "bytes": f.bytes, "sha256": f.sha256
                })
            })
            .collect();
        let metadata = json!({
            "version": 1,
            "fingerprint": self.fingerprint(),
            "listingSha256": listing.listing_sha256(),
            "files": files,
            "directories": listing.directories,
            "bytes": listing.bytes,
            "settings": self.settings()
        });
        let mut encoded = Metadata(Vec::new());
        serde_json::to_writer(&mut encoded, &metadata).map_err(io::Error::other)?;
        let length = u32::try_from(encoded.0.len()).map_err(io::Error::other)?;
        output.write_all(DECODE_V1_MAGIC)?;
        output.write_all(&length.to_le_bytes())?;
        output.write_all(&encoded.0)?;
        for (record, bytes) in self.files() {
            output.write_all(&record.bytes.to_le_bytes())?;
            output.write_all(bytes)?;
        }
        output.write_all(DECODE_V1_END)?;
        output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_limit_is_enforced_without_unbounded_growth() {
        let mut m = Metadata(vec![0; MAX_DECODE_METADATA - 1]);
        assert!(m.write_all(b"xx").is_err());
        assert_eq!(m.0.len(), MAX_DECODE_METADATA - 1);
        assert!(m.write_all(b"x").is_ok());
        assert!(m.write_all(b"x").is_err());
    }
}
