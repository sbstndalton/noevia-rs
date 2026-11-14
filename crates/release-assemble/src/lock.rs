//! `release/versions.lock` as the assembler reads it.
//!
//! The shell keeps the whole file in `$lock` (a command substitution: NUL bytes dropped, trailing
//! newlines stripped) and reads a key with `printf '%s\n' "$lock" | sed -n "s/^$1=//p" | tail -n 1`:
//! the rest of the LAST line that starts with `KEY=`, byte for byte (a `#` after the value, spaces and a
//! carriage return all stay part of it). There is no comment syntax beyond "a line that does not start
//! with `KEY=`". Values are bytes: the lock is not required to be UTF-8.

/// The lock text as the shell holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lock {
    text: Vec<u8>,
}

impl Lock {
    /// `lock="$(cat -- file)"` or `lock="$(git show sha:release/versions.lock)"`.
    pub fn from_bytes(raw: &[u8]) -> Self {
        let mut text: Vec<u8> = raw.iter().copied().filter(|b| *b != 0).collect();
        while text.last() == Some(&b'\n') {
            text.pop();
        }
        Lock { text }
    }

    fn lines(&self) -> impl Iterator<Item = &[u8]> {
        self.text.split(|b| *b == b'\n')
    }

    /// `lock_value KEY`: the value of the last `KEY=` line, or empty when there is none.
    ///
    /// `KEY` is spliced into a sed basic regex; the keys the assembler uses are `[A-Z0-9_]+`, which
    /// match themselves. Other keys are refused rather than interpreted as a regex.
    pub fn value(&self, key: &str) -> Vec<u8> {
        if !plain_key(key) {
            return Vec::new();
        }
        let mut prefix = key.as_bytes().to_vec();
        prefix.push(b'=');
        self.lines()
            .filter_map(|line| line.strip_prefix(prefix.as_slice()))
            .last()
            .map(<[u8]>::to_vec)
            .unwrap_or_default()
    }

    /// `printf '%s\n' "$lock" | grep -q '^KEY='`: a `KEY=` line exists (even with an empty value).
    pub fn has_key(&self, key: &str) -> bool {
        if !plain_key(key) {
            return false;
        }
        let mut prefix = key.as_bytes().to_vec();
        prefix.push(b'=');
        self.lines().any(|line| line.starts_with(&prefix))
    }
}

fn plain_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// `[[ "$v" =~ ^[0-9a-f]{N}$ ]]`.
pub fn lower_hex(v: &[u8], n: usize) -> bool {
    v.len() == n
        && v.iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// `[[ "$sha" =~ ^[0-9a-f]{7,40}$ ]]` (the usage check of the noevia sha).
pub fn release_sha(v: &str) -> bool {
    (7..=40).contains(&v.len()) && lower_hex(v.as_bytes(), v.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_line_wins_and_bytes_are_kept() {
        let l =
            Lock::from_bytes(b"# NOEVIA_RS_REF=x\nNOEVIA_RS_REF=a\nNOEVIA_RS_REF=b # c\r\n\n\n");
        assert_eq!(l.value("NOEVIA_RS_REF"), b"b # c\r".to_vec());
        assert_eq!(l.value("NOEVIA_RS"), Vec::<u8>::new());
        assert!(l.has_key("NOEVIA_RS_REF"));
        assert!(!l.has_key("NOEVIA_WEB_REF"));
    }

    #[test]
    fn empty_last_line_wins() {
        let l = Lock::from_bytes(b"K=a\nK=\n");
        assert_eq!(l.value("K"), Vec::<u8>::new());
        assert!(l.has_key("K"));
    }

    #[test]
    fn hex_checks() {
        assert!(release_sha("abcdef0"));
        assert!(!release_sha("abcdef"));
        assert!(!release_sha("ABCDEF0"));
        assert!(lower_hex(&[b'a'; 40], 40));
        assert!(!lower_hex(b"a\r", 1));
    }
}
