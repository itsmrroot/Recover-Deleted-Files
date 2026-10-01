//! Human-readable byte sizes (`64K`, `1.5GB`, `4096`) in binary units.

pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let value: f64 = num.parse().map_err(|_| format!("invalid size: {s:?}"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        other => return Err(format!("unknown size unit {other:?} in {s:?}")),
    };
    let bytes = value * mult as f64;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("size out of range: {s:?}"));
    }
    Ok(bytes as u64)
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("64k"), Ok(65536));
        assert_eq!(parse_size("1.5 MB"), Ok(1_572_864));
        assert_eq!(parse_size("2GiB"), Ok(2 << 30));
        assert!(parse_size("12 parsecs").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(3 << 30), "3.0 GiB");
    }
}
