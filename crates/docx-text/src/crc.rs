//! CRC-32 (IEEE 802.3, the ZIP checksum), table-driven.

// Const evaluation: i < 256 always, and an out-of-range index would fail the build, not run.
#[allow(clippy::indexing_slicing)]
const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

pub(crate) fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, &b| {
        TABLE
            .get(usize::from((c as u8) ^ b))
            .map_or(0, |t| t ^ (c >> 8))
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_value() {
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(super::crc32(b""), 0);
    }
}
