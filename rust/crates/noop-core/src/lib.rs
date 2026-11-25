//! Logic shared by the Noop server and the machine program. No I/O here.
pub mod authcode;
pub mod budget;
pub mod paths;
pub mod tools;
pub mod totp;

/// `encodeURIComponent`: everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )` is percent-encoded as UTF-8.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Constant time for equal lengths.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Fill a buffer from the OS random source.
pub fn random_bytes(buf: &mut [u8]) {
    getrandom::fill(buf).expect("OS random source");
}
