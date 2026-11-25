//! TOTP (RFC 6238: HMAC-SHA1, 30 s steps, 6 digits), the second factor of the owner's sign-in.
//! Port of cloud/src/totp.ts.
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
pub const STEP_S: i64 = 30;
pub const DIGITS: usize = 6;
/// ±1 step of clock drift.
pub const WINDOW: i64 = 1;

pub fn b32encode(b: &[u8]) -> String {
    let (mut bits, mut v, mut out) = (0u32, 0u32, String::new());
    for &x in b {
        v = ((v << 8) | x as u32) & 0xffff;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((v >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((v << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn b32decode(s: &str) -> Result<Vec<u8>, &'static str> {
    let (mut bits, mut v, mut out) = (0u32, 0u32, Vec::new());
    for c in s.to_uppercase().chars().filter(|c| !c.is_whitespace() && *c != '=' && *c != '-') {
        let i = B32.iter().position(|&b| b as char == c).ok_or("bad base32")? as u32;
        v = ((v << 5) | i) & 0xffff;
        bits += 5;
        if bits >= 8 {
            out.push(((v >> (bits - 8)) & 255) as u8);
            bits -= 8;
        }
    }
    Ok(out)
}

/// A new 160-bit secret, base32 (what authenticator apps take).
pub fn new_secret() -> String {
    let mut b = [0u8; 20];
    crate::random_bytes(&mut b);
    b32encode(&b)
}

pub fn hotp(secret: &[u8], counter: u64) -> String {
    let mut m = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC takes any key length");
    m.update(&counter.to_be_bytes());
    let h = m.finalize().into_bytes();
    let o = (h[h.len() - 1] & 15) as usize;
    let n = ((h[o] as u32 & 127) << 24) | ((h[o + 1] as u32) << 16) | ((h[o + 2] as u32) << 8) | h[o + 3] as u32;
    format!("{:0width$}", n % 10u32.pow(DIGITS as u32), width = DIGITS)
}

pub fn step_at(ms: i64) -> i64 {
    ms.div_euclid(1000 * STEP_S)
}

/// The step the code matches (within ±WINDOW), or None. Steps ≤ `after` were already used: a code works once.
pub fn totp_match(secret_b32: &str, code: &str, after: i64, ms: i64) -> Option<i64> {
    let c: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if c.len() != DIGITS || !c.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let key = b32decode(secret_b32).ok()?;
    let now = step_at(ms);
    (now - WINDOW..=now + WINDOW)
        .filter(|&s| s > after && s >= 0)
        .find(|&s| crate::ct_eq(hotp(&key, s as u64).as_bytes(), c.as_bytes()))
}

/// otpauth:// URI for the QR code / manual entry.
pub fn otpauth_uri(secret: &str, account: &str, issuer: &str) -> String {
    use crate::encode_uri_component as e;
    format!(
        "otpauth://totp/{}?secret={secret}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_S}",
        e(&format!("{issuer}:{account}")),
        e(issuer)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rfc6238_vector() {
        // RFC 6238 appendix B, SHA-1, T = 59 s → 94287082 (8 digits); the last 6 digits are 287082
        let key = b"12345678901234567890";
        assert_eq!(hotp(key, 1), "287082");
        let secret = b32encode(key);
        assert_eq!(b32decode(&secret).unwrap(), key);
        assert_eq!(totp_match(&secret, "287082", -1, 59_000), Some(1));
        assert_eq!(totp_match(&secret, "287082", 1, 59_000), None, "a code works once");
        assert_eq!(totp_match(&secret, "28708", -1, 59_000), None);
    }
}
