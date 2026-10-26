//! CRC-32 (IEEE, as zlib), used to verify uploads end to end. Bitwise to keep the daemon small; it
//! runs once per uploaded byte, which is fine next to SD write speed.

pub const INIT: u32 = 0xFFFF_FFFF;

/// Feeds `data` into a running CRC. Start from [`INIT`] and pass the result to [`finish`].
pub fn update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
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
}
