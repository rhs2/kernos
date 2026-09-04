//! Duration literals of the policy language (`30m`, `4h`, `2d`).

/// Parses a duration literal into whole seconds. Accepts a decimal number
/// followed by one of `s`, `m`, `h`, `d`; anything else returns `None`. Fractions
/// are allowed and floored to seconds. Exists so the lexer, the CLI (`--ttl 24h`)
/// and the corpus exporter (`--since 30d`) share one definition.
pub fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim();
    let unit = text.chars().last()?;
    let multiplier: f64 = match unit {
        's' => 1.0,
        'm' => 60.0,
        'h' => 3600.0,
        'd' => 86400.0,
        _ => return None,
    };
    let number = &text[..text.len() - 1];
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some((value * multiplier).floor() as u64)
}

#[cfg(test)]
mod tests {
    use super::parse_duration;

    #[test]
    fn parses_every_unit() {
        assert_eq!(parse_duration("30s"), Some(30));
        assert_eq!(parse_duration("30m"), Some(1800));
        assert_eq!(parse_duration("4h"), Some(14400));
        assert_eq!(parse_duration("2d"), Some(172800));
        assert_eq!(parse_duration("1.5h"), Some(5400));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("4"), None);
        assert_eq!(parse_duration("4w"), None);
        assert_eq!(parse_duration("-4h"), None);
        assert_eq!(parse_duration(""), None);
    }
}
