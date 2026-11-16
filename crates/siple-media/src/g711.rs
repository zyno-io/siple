//! ITU-T G.711 mu-law / A-law companding (own implementation).
//!
//! Decoding follows the Sun `g711.c` reference (mu-law bias 0x84, A-law with the 0x55 even-bit
//! inversion) and is table-driven (256 entries, built at compile time). Encoding picks the
//! *nearest* codeword for each sample (14-bit input resolution, midpoint rounding between
//! adjacent decoded levels, mu-law clip at 32635), via a 16 KiB table built at compile time from
//! the decode tables, so encode and decode are exact inverses on codewords.

const ULAW_BIAS: i32 = 0x84;

/// Encode one 16-bit linear sample to mu-law.
#[inline]
pub fn linear_to_ulaw(sample: i16) -> u8 {
    ULAW_ENCODE[((sample as u16).wrapping_add(0x8000) >> 2) as usize]
}

/// Decode one mu-law byte to a 16-bit linear sample.
#[inline]
pub fn ulaw_to_linear(byte: u8) -> i16 {
    ULAW_DECODE[byte as usize]
}

/// Encode one 16-bit linear sample to A-law (with the 0x55 even-bit inversion).
#[inline]
pub fn linear_to_alaw(sample: i16) -> u8 {
    ALAW_ENCODE[((sample as u16).wrapping_add(0x8000) >> 2) as usize]
}

/// Decode one A-law byte to a 16-bit linear sample.
#[inline]
pub fn alaw_to_linear(byte: u8) -> i16 {
    ALAW_DECODE[byte as usize]
}

const fn build_ulaw() -> [i16; 256] {
    let mut t = [0i16; 256];
    let mut i = 0;
    while i < 256 {
        let u = !(i as u8) as i32;
        let mut v = (((u & 0x0F) << 3) + ULAW_BIAS) << ((u & 0x70) >> 4);
        v = if u & 0x80 != 0 {
            ULAW_BIAS - v
        } else {
            v - ULAW_BIAS
        };
        t[i] = v as i16;
        i += 1;
    }
    t
}

const fn build_alaw() -> [i16; 256] {
    let mut t = [0i16; 256];
    let mut i = 0;
    while i < 256 {
        let a = (i as u8 ^ 0x55) as i32;
        let seg = (a & 0x70) >> 4;
        let mut v = (a & 0x0F) << 4;
        v = match seg {
            0 => v + 8,
            1 => v + 0x108,
            _ => (v + 0x108) << (seg - 1),
        };
        t[i] = (if a & 0x80 != 0 { v } else { -v }) as i16;
        i += 1;
    }
    t
}

/// Build the nearest-codeword encode table, indexed by `(sample + 32768) >> 2`.
/// `mask` is the code of zero (0xFF mu-law, 0xD5 A-law); `dec` is the matching decode table.
const fn build_encode(dec: &[i16; 256], mask: u8) -> [u8; 16384] {
    let mut t = [0u8; 16384];
    t[8192] = mask;
    let mut j = 1usize;
    let mut i = 0u8;
    while i < 127 {
        let v1 = dec[(i ^ mask) as usize] as i32;
        let v2 = dec[((i + 1) ^ mask) as usize] as i32;
        let v = ((v1 + v2 + 4) >> 3) as usize;
        while j < v {
            t[8192 - j] = i ^ (mask ^ 0x80);
            t[8192 + j] = i ^ mask;
            j += 1;
        }
        i += 1;
    }
    while j < 8192 {
        t[8192 - j] = 127 ^ (mask ^ 0x80);
        t[8192 + j] = 127 ^ mask;
        j += 1;
    }
    t[0] = t[1];
    t
}

static ULAW_DECODE: [i16; 256] = build_ulaw();
static ALAW_DECODE: [i16; 256] = build_alaw();
static ULAW_ENCODE: [u8; 16384] = build_encode(&ULAW_DECODE, 0xFF);
static ALAW_ENCODE: [u8; 16384] = build_encode(&ALAW_DECODE, 0xD5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_vectors() {
        // (sample, mu-law, A-law); extremes and silence match Sun g711.c / ITU-T G.711.
        let v: [(i16, u8, u8); 9] = [
            (0, 0xFF, 0xD5),
            (1, 0xFF, 0xD5),
            (4, 0xFE, 0xD5),
            (-1, 0x7E, 0x55),
            (-4, 0x7E, 0x55),
            (1000, 0xCE, 0xFA),
            (-1000, 0x4E, 0x7A),
            (32767, 0x80, 0xAA),
            (-32768, 0x00, 0x2A),
        ];
        for (s, u, a) in v {
            assert_eq!(linear_to_ulaw(s), u, "ulaw {s}");
            assert_eq!(linear_to_alaw(s), a, "alaw {s}");
        }
        assert_eq!(ulaw_to_linear(0xFF), 0);
        assert_eq!(ulaw_to_linear(0x7F), 0);
        assert_eq!(ulaw_to_linear(0x80), 32124);
        assert_eq!(ulaw_to_linear(0x00), -32124);
        assert_eq!(alaw_to_linear(0xD5), 8);
        assert_eq!(alaw_to_linear(0x55), -8);
        assert_eq!(alaw_to_linear(0xAA), 32256);
        assert_eq!(alaw_to_linear(0x2A), -32256);
    }

    #[test]
    fn decode_encode_is_identity_on_codewords() {
        for b in 0..=255u8 {
            let u = ulaw_to_linear(b);
            // 0x7F and 0xFF are both "zero"; the encoder emits 0xFF / 0x7F by sign.
            assert_eq!(ulaw_to_linear(linear_to_ulaw(u)), u);
            let a = alaw_to_linear(b);
            assert_eq!(linear_to_alaw(a), b);
        }
    }

    #[test]
    fn roundtrip_error_is_bounded() {
        for s in i16::MIN..=i16::MAX {
            let m = (s as i32).abs();
            let eu = (ulaw_to_linear(linear_to_ulaw(s)) as i32 - s as i32).abs();
            let ea = (alaw_to_linear(linear_to_alaw(s)) as i32 - s as i32).abs();
            // Step size is at most 1/16 of the magnitude (+ bias slack) in both laws.
            assert!(eu <= m / 16 + 64 || m > 32635, "ulaw {s}: {eu}");
            assert!(ea <= m / 16 + 32, "alaw {s}: {ea}");
        }
    }

    #[test]
    fn encode_is_monotonic_in_decoded_value() {
        let (mut pu, mut pa) = (i32::MIN, i32::MIN);
        for s in i16::MIN..=i16::MAX {
            let u = ulaw_to_linear(linear_to_ulaw(s)) as i32;
            let a = alaw_to_linear(linear_to_alaw(s)) as i32;
            assert!(u >= pu && a >= pa, "not monotonic at {s}");
            (pu, pa) = (u, a);
        }
    }

    fn fnv(h: u64, b: u8) -> u64 {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    }

    /// Fixed digests of the full encode/decode maps, captured when this implementation was
    /// verified bit-exact against the `xlaw` crate for every i16 input and every codeword.
    #[test]
    fn full_map_digests_are_stable() {
        let (mut eu, mut ea, mut du, mut da) = (
            0xcbf29ce484222325u64,
            0xcbf29ce484222325u64,
            0xcbf29ce484222325u64,
            0xcbf29ce484222325u64,
        );
        for s in i16::MIN..=i16::MAX {
            eu = fnv(eu, linear_to_ulaw(s));
            ea = fnv(ea, linear_to_alaw(s));
        }
        for b in 0..=255u8 {
            for x in ulaw_to_linear(b).to_le_bytes() {
                du = fnv(du, x);
            }
            for x in alaw_to_linear(b).to_le_bytes() {
                da = fnv(da, x);
            }
        }
        assert_eq!(
            (eu, ea, du, da),
            (
                0xa73378df9212a731,
                0xb692762fb973cb11,
                0xb91e65d68481a1a8,
                0x693a40ca62a3d2a5
            )
        );
    }
}
