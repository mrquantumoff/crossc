//! Hex helpers for `raw`.

/// Parse hex like `"10 01"`, `"1001"`, `"0x10,0x01"`, `"10:01"`.
pub fn parse(s: &str) -> Result<Vec<u8>, String> {
    let mut digits = Vec::new();
    for tok in s.split(|c: char| c.is_whitespace() || c == ',' || c == ':' || c == '-') {
        let t = tok
            .strip_prefix("0x")
            .or_else(|| tok.strip_prefix("0X"))
            .unwrap_or(tok);
        if t.len() % 2 != 0 {
            return Err(format!("odd number of hex digits in {tok:?}"));
        }
        for c in t.chars() {
            digits.push(
                c.to_digit(16)
                    .ok_or_else(|| format!("bad hex digit {c:?}"))? as u8,
            );
        }
    }
    Ok(digits.chunks(2).map(|p| (p[0] << 4) | p[1]).collect())
}

pub fn format(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 3);
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&std::format!("{x:02X}"));
    }
    s
}

/// Parse a number in decimal or 0x-hex.
pub fn parse_num(s: &str) -> Result<u32, String> {
    let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u32::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|_| format!("bad number {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forms() {
        assert_eq!(parse("10 01").unwrap(), vec![0x10, 0x01]);
        assert_eq!(parse("1001").unwrap(), vec![0x10, 0x01]);
        assert_eq!(parse("0x10,0x01").unwrap(), vec![0x10, 0x01]);
        assert_eq!(parse("aB:cd").unwrap(), vec![0xAB, 0xCD]);
        assert_eq!(parse("").unwrap(), Vec::<u8>::new());
        assert!(parse("1").is_err());
        assert!(parse("zz").is_err());
    }

    #[test]
    fn format_and_nums() {
        assert_eq!(format(&[0x10, 0x01, 0xff]), "10 01 FF");
        assert_eq!(parse_num("0x19").unwrap(), 25);
        assert_eq!(parse_num("25").unwrap(), 25);
        assert!(parse_num("x").is_err());
    }
}
