//! Optional source-IP allow-list (HUB_ALLOW_IPS: comma/space-separated IPs or CIDRs, v4 and v6). Unset: every IP may
//! connect (auth still applies). Set: anything else gets 403 before reaching any route. Port of cloud/src/ipallow.ts.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Net {
    v6: bool,
    base: u128,
    bits: u32,
}

fn v4(s: &str) -> Option<u128> {
    let p: Vec<&str> = s.split('.').collect();
    if p.len() != 4 {
        return None;
    }
    let mut n: u128 = 0;
    for x in p {
        if x.is_empty() || x.len() > 3 || !x.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u128 = x.parse().ok()?;
        if v > 255 {
            return None;
        }
        n = (n << 8) | v;
    }
    Some(n)
}

fn v6(s: &str) -> Option<u128> {
    if !s.contains(':') {
        return None;
    }
    let s = s.trim_start_matches('[').trim_end_matches(']');
    let mut s = s.split('%').next().unwrap_or("").to_string();
    // an embedded IPv4 tail (::ffff:1.2.3.4) becomes two hextets
    if let Some(i) = s.rfind(':') {
        let tail = &s[i + 1..];
        if tail.contains('.') {
            let n = v4(tail)?;
            s = format!("{}{:x}:{:x}", &s[..=i], n >> 16, n & 0xffff);
        }
    }
    let halves: Vec<&str> = s.split("::").collect();
    if halves.len() > 2 {
        return None;
    }
    let head: Vec<&str> = if halves[0].is_empty() { vec![] } else { halves[0].split(':').collect() };
    let rest: Vec<&str> = if halves.len() == 2 && !halves[1].is_empty() { halves[1].split(':').collect() } else { vec![] };
    let fill = 8i32 - head.len() as i32 - rest.len() as i32;
    if (halves.len() == 1 && fill != 0) || (halves.len() == 2 && fill < 1) {
        return None;
    }
    let zeros = if halves.len() == 2 { fill as usize } else { 0 };
    let mut n: u128 = 0;
    for h in head.iter().copied().chain(std::iter::repeat_n("0", zeros)).chain(rest.iter().copied()) {
        if h.is_empty() || h.len() > 4 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        n = (n << 16) | u128::from_str_radix(h, 16).ok()?;
    }
    Some(n)
}

/// (is v6, value). IPv4-mapped IPv6 (::ffff:a.b.c.d) is the IPv4 address.
fn parse_ip(s: &str) -> Option<(bool, u128)> {
    if let Some(a) = v4(s) {
        return Some((false, a));
    }
    let b = v6(s)?;
    if b >> 32 == 0xffff {
        return Some((false, b & 0xffff_ffff));
    }
    Some((true, b))
}

fn mask(v: u128, max: u32, bits: u32) -> u128 {
    let shift = max - bits;
    if shift >= 128 { 0 } else { (v >> shift) << shift }
}

/// Parses the allow-list; entries that don't parse are returned in the second list (and match nothing).
pub fn parse_allow_list(raw: &str) -> (Vec<Net>, Vec<String>) {
    let (mut nets, mut bad) = (Vec::new(), Vec::new());
    for e in raw.split(|c: char| c.is_whitespace() || c == ',').filter(|s| !s.is_empty()) {
        let (ip, len) = match e.split_once('/') {
            Some((a, b)) => (a, Some(b)),
            None => (e, None),
        };
        let Some((is6, n)) = parse_ip(ip) else {
            bad.push(e.to_string());
            continue;
        };
        let max = if is6 { 128 } else { 32 };
        let bits = match len {
            None => Some(max),
            Some(l) if !l.is_empty() && l.len() <= 3 && l.bytes().all(|b| b.is_ascii_digit()) => l.parse::<u32>().ok().filter(|&b| b <= max),
            _ => None,
        };
        let Some(bits) = bits else {
            bad.push(e.to_string());
            continue;
        };
        nets.push(Net { v6: is6, base: mask(n, max, bits), bits });
    }
    (nets, bad)
}

pub fn ip_allowed(ip: &str, nets: &[Net]) -> bool {
    let Some((is6, n)) = parse_ip(ip.trim()) else { return false };
    nets.iter().any(|net| net.v6 == is6 && mask(n, if is6 { 128 } else { 32 }, net.bits) == net.base)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allow_list() {
        let (nets, bad) = parse_allow_list("1.2.3.0/24, 10.0.0.1 2001:db8::/32,bogus 1.2.3.4/33");
        assert_eq!(nets.len(), 3);
        assert_eq!(bad, ["bogus", "1.2.3.4/33"]);
        assert!(ip_allowed("1.2.3.200", &nets));
        assert!(!ip_allowed("1.2.4.1", &nets));
        assert!(ip_allowed("10.0.0.1", &nets));
        assert!(!ip_allowed("10.0.0.2", &nets));
        assert!(ip_allowed("2001:db8:abcd::1", &nets));
        assert!(!ip_allowed("2001:db9::1", &nets));
        assert!(ip_allowed("::ffff:1.2.3.9", &nets), "IPv4-mapped IPv6 is the IPv4 address");
        assert!(!ip_allowed("", &nets));
        let (all, _) = parse_allow_list("0.0.0.0/0 ::/0");
        assert!(ip_allowed("8.8.8.8", &all) && ip_allowed("fe80::1", &all));
        assert_eq!(v6("1::2:3::4"), None);
        assert_eq!(v6("::1"), Some(1));
    }
}
