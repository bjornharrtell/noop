//! Login codes: 8 characters from 31 letters and digits that can't be misread (no 0/O, 1/I/L): 31^8 ≈ 8.5 × 10^11.
//! Port of cloud/src/authcode.ts.

pub const CODE_ALPHABET: &str = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";
pub const CODE_LEN: usize = 8;

/// Unbiased: bytes ≥ 248 (= 8 × 31) are redrawn.
pub fn new_code() -> String {
    let a = CODE_ALPHABET.as_bytes();
    let mut s = String::with_capacity(CODE_LEN);
    let mut b = [0u8; 1];
    while s.len() < CODE_LEN {
        crate::random_bytes(&mut b);
        if b[0] < 248 {
            s.push(a[(b[0] % 31) as usize] as char);
        }
    }
    s
}

/// What the owner typed → the canonical code: case-insensitive, spaces and dashes ignored.
pub fn norm_code(c: &str) -> String {
    c.to_uppercase().chars().filter(|ch| !ch.is_whitespace() && *ch != '-').collect()
}

pub fn code_ok(c: &str) -> bool {
    c.len() == CODE_LEN && c.bytes().all(|b| CODE_ALPHABET.as_bytes().contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codes() {
        for _ in 0..100 {
            assert!(code_ok(&new_code()));
        }
        assert_eq!(norm_code("abcd-efgh "), "ABCDEFGH");
        assert!(!code_ok("ABCDEFG0"));
    }
}
