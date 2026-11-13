// SPDX-License-Identifier: GPL-3.0-only
//! CRC-32 (IEEE, as zlib), used to verify uploads end to end. It runs over every uploaded byte, on the
//! console's 1 GHz core too, so it must be fast: there with the CPU's CRC32 instructions (the daemon's target
//! enables them), elsewhere eight bytes at a time from tables (slice-by-8).

pub const INIT: u32 = 0xFFFF_FFFF;

/// Feeds `data` into a running CRC. Start from [`INIT`] and pass the result to [`finish`].
pub fn update(crc: u32, data: &[u8]) -> u32 {
    #[cfg(all(target_arch = "aarch64", target_feature = "crc"))]
    {
        hardware(crc, data)
    }
    #[cfg(not(all(target_arch = "aarch64", target_feature = "crc")))]
    {
        sliced(crc, data)
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "crc"))]
#[allow(unused_unsafe)]
fn hardware(mut crc: u32, data: &[u8]) -> u32 {
    use core::arch::aarch64::{__crc32b, __crc32d};
    let mut words = data.chunks_exact(8);
    for word in &mut words {
        let word = u64::from_le_bytes(word.try_into().unwrap_or_default());
        // SAFETY: the target has the CRC instructions (checked by the cfg above).
        crc = unsafe { __crc32d(crc, word) };
    }
    for &byte in words.remainder() {
        // SAFETY: as above.
        crc = unsafe { __crc32b(crc, byte) };
    }
    crc
}

/// `TABLES[k][b]`: the CRC of byte `b` followed by `k` zero bytes.
#[cfg_attr(all(target_arch = "aarch64", target_feature = "crc"), allow(dead_code))]
const TABLES: [[u32; 256]; 8] = {
    let mut tables = [[0u32; 256]; 8];
    let mut b = 0;
    while b < 256 {
        let mut crc = b as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
            bit += 1;
        }
        tables[0][b] = crc;
        b += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut b = 0;
        while b < 256 {
            let prev = tables[k - 1][b];
            tables[k][b] = (prev >> 8) ^ tables[0][(prev & 0xFF) as usize];
            b += 1;
        }
        k += 1;
    }
    tables
};

#[cfg_attr(all(target_arch = "aarch64", target_feature = "crc"), allow(dead_code))]
fn sliced(mut crc: u32, data: &[u8]) -> u32 {
    let t = &TABLES;
    let mut words = data.chunks_exact(8);
    for w in &mut words {
        let lo = crc ^ u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        crc = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][w[4] as usize]
            ^ t[2][w[5] as usize]
            ^ t[1][w[6] as usize]
            ^ t[0][w[7] as usize];
    }
    for &byte in words.remainder() {
        crc = (crc >> 8) ^ t[0][((crc ^ byte as u32) & 0xFF) as usize];
    }
    crc
}

pub fn finish(crc: u32) -> u32 {
    !crc
}

pub fn checksum(data: &[u8]) -> u32 {
    finish(update(INIT, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert_eq!(checksum(b""), 0);
        assert_eq!(checksum(b"123456789"), 0xCBF4_3926);
        let split = finish(update(update(INIT, b"1234"), b"56789"));
        assert_eq!(split, 0xCBF4_3926);
    }

    /// The one-bit-at-a-time definition, to check the fast ones against.
    fn bitwise(mut crc: u32, data: &[u8]) -> u32 {
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
            }
        }
        crc
    }

    #[test]
    fn matches_the_bitwise_definition_at_every_length_and_split() {
        let data: alloc::vec::Vec<u8> = (0..1000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        for len in [0, 1, 7, 8, 9, 15, 16, 17, 63, 64, 65, 999, 1000] {
            assert_eq!(update(INIT, &data[..len]), bitwise(INIT, &data[..len]), "length {len}");
            assert_eq!(sliced(INIT, &data[..len]), bitwise(INIT, &data[..len]), "length {len}");
        }
        for split in [1, 3, 8, 500] {
            assert_eq!(update(update(INIT, &data[..split]), &data[split..]), bitwise(INIT, &data));
        }
    }
}
