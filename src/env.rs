//! Env var parsing helpers shared by the V3 and V4 servers, mirroring the
//! TS `numberFromEnv`/`Bun.env.VERBOSE === 'true'` helpers.

/// Mirrors JS `Number(raw)`: must parse to a finite, strictly positive
/// number, otherwise `default` is used. Pure so it's trivially unit
/// testable without touching real process env vars.
pub fn parse_number(raw: Option<&str>, default: f64) -> f64 {
    let text = match raw {
        Some(t) => t.trim(),
        None => return default,
    };
    if text.is_empty() {
        // JS `Number("")` is 0, which fails the `> 0` check below.
        return default;
    }
    match text.parse::<f64>() {
        Ok(n) if n.is_finite() && n > 0.0 => n,
        _ => default,
    }
}

pub fn number_from_env(name: &str, default: f64) -> f64 {
    parse_number(std::env::var(name).ok().as_deref(), default)
}

pub fn u64_from_env(name: &str, default: u64) -> u64 {
    number_from_env(name, default as f64) as u64
}

pub fn u16_from_env(name: &str, default: u16) -> u16 {
    number_from_env(name, default as f64) as u16
}

pub fn i64_from_env(name: &str, default: i64) -> i64 {
    number_from_env(name, default as f64) as i64
}

/// Mirrors `Bun.env.VERBOSE === 'true'`.
pub fn parse_bool_flag(raw: Option<&str>) -> bool {
    raw == Some("true")
}

pub fn bool_from_env(name: &str) -> bool {
    parse_bool_flag(std::env::var(name).ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_number_missing_uses_default() {
        assert_eq!(parse_number(None, 42.0), 42.0);
    }

    #[test]
    fn parse_number_empty_uses_default() {
        assert_eq!(parse_number(Some(""), 42.0), 42.0);
        assert_eq!(parse_number(Some("   "), 42.0), 42.0);
    }

    #[test]
    fn parse_number_non_numeric_uses_default() {
        assert_eq!(parse_number(Some("abc"), 42.0), 42.0);
    }

    #[test]
    fn parse_number_negative_or_zero_uses_default() {
        assert_eq!(parse_number(Some("-5"), 42.0), 42.0);
        assert_eq!(parse_number(Some("0"), 42.0), 42.0);
    }

    #[test]
    fn parse_number_valid() {
        assert_eq!(parse_number(Some("60000"), 1.0), 60000.0);
        assert_eq!(parse_number(Some("  60000  "), 1.0), 60000.0);
    }

    #[test]
    fn parse_bool_flag_matches_literal_true_only() {
        assert!(parse_bool_flag(Some("true")));
        assert!(!parse_bool_flag(Some("TRUE")));
        assert!(!parse_bool_flag(Some("1")));
        assert!(!parse_bool_flag(None));
    }
}
