//! `sha256_of` and the checksum step of `fetch_remote`.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Lowercase hex sha256 of a byte slice.
pub fn sha256_hex(bytes: &[u8]) -> String {
    to_hex(&Sha256::digest(bytes))
}

/// `sha256_of FILE`, streamed.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or_default());
    }
    Ok(to_hex(&hasher.finalize()))
}

/// `[ "$(sha256_of "$tgz")" = "$want_sum" ] || die "<repo> tarball checksum mismatch at <ref>"`.
/// An unreadable file is a mismatch too (the shell's `sha256sum` then prints nothing).
pub fn check_tarball(path: &Path, repo: &str, reference: &str, want: &str) -> Result<(), String> {
    match sha256_file(path) {
        Ok(got) if got == want => Ok(()),
        _ => Err(format!("{repo} tarball checksum mismatch at {reference}")),
    }
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
