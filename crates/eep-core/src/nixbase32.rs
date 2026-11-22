//! Nix's base-32: its own alphabet (no e, o, t, u) and its own bit order
//! (the last character holds the lowest bits of the first byte).

const ALPHABET: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Length of the encoding of `n` bytes.
pub const fn encoded_len(n: usize) -> usize {
    (n * 8).div_ceil(5)
}

pub fn encode(bytes: &[u8]) -> String {
    let len = encoded_len(bytes.len());
    let mut out = String::with_capacity(len);
    for n in (0..len).rev() {
        let b = n * 5;
        let (i, j) = (b / 8, b % 8);
        let lo = u16::from(bytes[i]);
        let hi = u16::from(bytes.get(i + 1).copied().unwrap_or(0));
        let c = ((lo | hi << 8) >> j) & 0x1f;
        out.push(char::from(ALPHABET[usize::from(c)]));
    }
    out
}

pub fn decode(s: &str) -> Option<Vec<u8>> {
    let len = s.len() * 5 / 8;
    let mut out = vec![0u8; len];
    for (n, ch) in s.bytes().rev().enumerate() {
        let digit = u16::try_from(ALPHABET.iter().position(|&c| c == ch)?).ok()?;
        let b = n * 5;
        let (i, j) = (b / 8, b % 8);
        let v = digit << j;
        *out.get_mut(i)? |= (v & 0xff) as u8;
        let carry = (v >> 8) as u8;
        match out.get_mut(i + 1) {
            Some(next) => *next |= carry,
            None if carry != 0 => return None, // bits past the end: invalid
            None => {}
        }
    }
    Some(out)
}

pub fn is_valid_char(c: u8) -> bool {
    ALPHABET.contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    // glibc-2.42-103's NarHash, as narinfo (nix32) and as hex (from its SRI form).
    const NIX32: &str = "0d4zl9l50iprs7yv9q80ggsc5saazxdz9sq6vrrp0j8zdc8sxsw1";
    const HEX: &str = "81ebae116b1f497073de06ebf45bff4ae9c2f47b00e1b4fdd1f9465068a29f34";

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn decode_known() {
        assert_eq!(hex(&decode(NIX32).expect("valid")), HEX);
    }

    #[test]
    fn roundtrip() {
        let bytes = decode(NIX32).expect("valid");
        assert_eq!(encode(&bytes), NIX32);
        for n in 0..40u8 {
            let v: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37).wrapping_add(n)).collect();
            assert_eq!(decode(&encode(&v)).expect("roundtrip"), v);
        }
    }

    #[test]
    fn rejects() {
        assert!(decode("e").is_none()); // not in the alphabet
        assert!(decode("zz").is_none()); // 10 bits for 1 byte: overflow
    }
}
