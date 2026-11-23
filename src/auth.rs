// SPDX-License-Identifier: AGPL-3.0-or-later
//! Challenge/response cipher (port of Gadgetbridge `protocol/Authentication.java`
//! and `protocol/AuthData.java`).
//!
//! It is the Bluetooth flavour of SAFER+ (128-bit key, 8 rounds, plaintext
//! re-added before round 3), keyed with the earbuds' challenge (last byte
//! XOR 6) and applied to a fixed plaintext.

const BLOCK: usize = 16;
/// Bit i set => byte i uses XOR / exp; cleared => ADD / log.
const PATTERN: u32 = 0x9999;

const SEQ: [u8; 16] = [
    0x11, 0x22, 0x33, 0x33, 0x22, 0x11, 0x11, 0x22, 0x33, 0x33, 0x22, 0x11, 0x11, 0x22, 0x33, 0x33,
];

const COEFFICIENTS: [[u8; 16]; 16] = [
    [2, 1, 1, 1, 4, 2, 1, 1, 2, 2, 4, 2, 4, 4, 16, 8],
    [2, 1, 1, 1, 4, 2, 1, 1, 1, 1, 2, 1, 2, 2, 8, 4],
    [1, 1, 4, 2, 2, 2, 4, 2, 16, 8, 4, 4, 2, 1, 1, 1],
    [1, 1, 4, 2, 1, 1, 2, 1, 8, 4, 2, 2, 2, 1, 1, 1],
    [16, 8, 2, 2, 4, 2, 4, 4, 1, 1, 4, 2, 1, 1, 2, 1],
    [8, 4, 1, 1, 2, 1, 2, 2, 1, 1, 4, 2, 1, 1, 2, 1],
    [2, 2, 4, 2, 4, 4, 16, 8, 2, 1, 1, 1, 4, 2, 1, 1],
    [1, 1, 2, 1, 2, 2, 8, 4, 2, 1, 1, 1, 4, 2, 1, 1],
    [4, 2, 4, 4, 16, 8, 2, 2, 1, 1, 2, 1, 1, 1, 4, 2],
    [2, 1, 2, 2, 8, 4, 1, 1, 1, 1, 2, 1, 1, 1, 4, 2],
    [4, 4, 16, 8, 1, 1, 2, 1, 4, 2, 1, 1, 4, 2, 2, 2],
    [2, 2, 8, 4, 1, 1, 2, 1, 4, 2, 1, 1, 2, 1, 1, 1],
    [1, 1, 2, 1, 1, 1, 4, 2, 4, 4, 16, 8, 2, 2, 4, 2],
    [1, 1, 2, 1, 1, 1, 4, 2, 2, 2, 8, 4, 1, 1, 2, 1],
    [4, 2, 1, 1, 2, 1, 1, 1, 4, 2, 2, 2, 16, 8, 4, 4],
    [4, 2, 1, 1, 2, 1, 1, 1, 2, 1, 1, 1, 8, 4, 2, 2],
];

const fn pow45_mod257(mut e: u32) -> u32 {
    let mut base = 45u32;
    let mut acc = 1u32;
    while e > 0 {
        if e & 1 == 1 {
            acc = acc * base % 257;
        }
        base = base * base % 257;
        e >>= 1;
    }
    acc
}

const fn gen_exp() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        // 45^128 mod 257 == 256, which is represented as 0.
        t[i] = (pow45_mod257(i as u32) % 256) as u8;
        i += 1;
    }
    t
}

const fn gen_log() -> [u8; 256] {
    let mut t = [0u8; 256];
    t[0] = 128;
    let mut i = 1;
    while i < 256 {
        let v = pow45_mod257(i as u32);
        if v != 256 {
            t[v as usize] = i as u8;
        }
        i += 1;
    }
    t
}

const fn gen_bias() -> [[u8; 16]; 16] {
    let mut b = [[0u8; 16]; 16];
    let mut i = 0;
    while i < 16 {
        let mut j = 0;
        while j < 16 {
            let e = 17 * (i as u32 + 2) + (j as u32 + 1);
            let v = pow45_mod257(pow45_mod257(e));
            b[i][j] = (v % 256) as u8;
            j += 1;
        }
        i += 1;
    }
    b
}

pub(crate) const EXP: [u8; 256] = gen_exp();
pub(crate) const LOG: [u8; 256] = gen_log();
pub(crate) const BIAS: [[u8; 16]; 16] = gen_bias();

#[inline]
fn xor_slot(i: usize) -> bool {
    (1u32 << i) & PATTERN != 0
}

fn key_schedule(challenge: &[u8; 16]) -> [[u8; 16]; 17] {
    let mut key0 = *challenge;
    key0[15] ^= 6;

    let mut reg = [0u8; 17];
    reg[..16].copy_from_slice(&key0);
    reg[16] = key0.iter().fold(0, |a, b| a ^ b);

    let mut keys = [[0u8; 16]; 17];
    keys[0] = key0;
    for k in 1..17 {
        for r in reg.iter_mut() {
            *r = r.rotate_left(3);
        }
        for i in 0..16 {
            keys[k][i] = reg[(k + i) % 17].wrapping_add(BIAS[k - 1][i]);
        }
    }
    keys
}

fn encrypt(plain: &[u8; 16], keys: &[[u8; 16]; 17]) -> [u8; 16] {
    let mut c = *plain;
    for round in 0..8 {
        if round == 2 {
            for i in 0..BLOCK {
                c[i] = if xor_slot(i) { c[i] ^ plain[i] } else { c[i].wrapping_add(plain[i]) };
            }
        }
        let k1 = &keys[round * 2];
        let k2 = &keys[round * 2 + 1];
        for i in 0..BLOCK {
            c[i] = if xor_slot(i) { c[i] ^ k1[i] } else { c[i].wrapping_add(k1[i]) };
        }
        for i in 0..BLOCK {
            c[i] = if xor_slot(i) { EXP[c[i] as usize] } else { LOG[c[i] as usize] };
        }
        for i in 0..BLOCK {
            c[i] = if xor_slot(i) { k2[i].wrapping_add(c[i]) } else { k2[i] ^ c[i] };
        }
        let copy = c;
        for i in 0..BLOCK {
            let mut sum = 0u8;
            for j in 0..BLOCK {
                sum = sum.wrapping_add(COEFFICIENTS[i][j].wrapping_mul(copy[j]));
            }
            c[i] = sum;
        }
    }
    let k = &keys[16];
    for i in 0..BLOCK {
        c[i] = if xor_slot(i) { k[i] ^ c[i] } else { k[i].wrapping_add(c[i]) };
    }
    c
}

/// Computes the 16-byte answer to the earbuds' 16-byte challenge.
pub fn challenge_response(challenge: &[u8; 16]) -> [u8; 16] {
    encrypt(&SEQ, &key_schedule(challenge))
}

/// Random 16-byte challenge for the phone-initiated auth step. The earbuds do
/// not seem to verify our side (Gadgetbridge ignores their answer too), so a
/// non-cryptographic source is enough.
pub fn random_challenge() -> [u8; 16] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = [0u8; 16];
    for chunk in out.chunks_mut(8) {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_log_tables_are_inverse() {
        assert_eq!(EXP[0], 1);
        assert_eq!(EXP[1], 45);
        assert_eq!(EXP[128], 0); // 45^128 = 256 = -1 mod 257
        assert_eq!(LOG[0], 128);
        assert_eq!(LOG[1], 0);
        assert_eq!(LOG[45], 1);
        for x in 0..256usize {
            assert_eq!(EXP[LOG[x] as usize] as usize, x, "x={x}");
            assert_eq!(LOG[EXP[x] as usize] as usize, x, "x={x}");
        }
    }

    #[test]
    fn bias_matches_safer_plus_spec() {
        // SAFER+ bias vector B2 (first row here) from the Bluetooth Core spec,
        // Vol 2 Part H, 6.1: 46 97 b1 ba a3 b7 10 0a c5 37 b3 c9 5a 28 ac 64.
        assert_eq!(
            BIAS[0],
            [0x46, 0x97, 0xb1, 0xba, 0xa3, 0xb7, 0x10, 0x0a, 0xc5, 0x37, 0xb3, 0xc9, 0x5a, 0x28, 0xac, 0x64]
        );
    }

    #[test]
    fn key_schedule_first_key_is_modified_challenge() {
        let ch = [0u8; 16];
        let k = key_schedule(&ch);
        let mut expect = [0u8; 16];
        expect[15] = 6;
        assert_eq!(k[0], expect);
        // With an all-zero key except byte 15 = 6, register = [0..0, 6, 6].
        // K2[i] = rotl3(reg[(1+i)%17]) + B2[i].
        let reg = |n: usize| if n >= 15 { 6u8.rotate_left(3) } else { 0 };
        for i in 0..16 {
            assert_eq!(k[1][i], reg((1 + i) % 17).wrapping_add(BIAS[0][i]));
        }
    }

    #[test]
    fn response_is_deterministic_and_challenge_dependent() {
        let a = challenge_response(&[0u8; 16]);
        let b = challenge_response(&[0u8; 16]);
        let mut ch = [0u8; 16];
        ch[0] = 1;
        let c = challenge_response(&ch);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, SEQ);
    }

    #[test]
    fn response_regression_vectors() {
        // Regression vectors produced by this port (no independent vectors
        // available); real-device auth success is the end-to-end check.
        let v0 = challenge_response(&[0u8; 16]);
        let v1 = challenge_response(&core::array::from_fn(|i| i as u8));
        println!("zero -> {}", crate::hex(&v0));
        println!("0..15 -> {}", crate::hex(&v1));
        assert_eq!(crate::hex(&v0), VEC_ZERO);
        assert_eq!(crate::hex(&v1), VEC_SEQ);
    }

    const VEC_ZERO: &str = "BC A5 90 5B C8 49 39 2E 7B F9 FD CD C5 70 EF 77";
    const VEC_SEQ: &str = "87 13 91 3C 41 43 4E 09 1C AD 79 4A 1B 5D 95 C4";
}
