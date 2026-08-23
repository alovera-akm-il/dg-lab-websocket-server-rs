//! V3 wire-frame parsing/validation and value-normalization helpers,
//! mirroring `parseProtocolMessage`, `validateSource`, `numericType`,
//! `normalizeChannel`, `normalizeNumber`, `normalizePositiveInteger` and
//! `isAppReportMessage` from v3-server.ts.
//!
//! `type`/`channel`/`strength`/`time` are loosely-typed unions on the wire
//! (string or number), so inbound frames are hand-parsed off
//! `serde_json::Value` rather than fought into a single serde derive.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    A,
    B,
}

impl Channel {
    pub fn letter(self) -> char {
        match self {
            Channel::A => 'A',
            Channel::B => 'B',
        }
    }

    pub fn number(self) -> u8 {
        match self {
            Channel::A => 1,
            Channel::B => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub type_: Value,
    pub client_id: String,
    pub target_id: String,
    pub message: String,
    pub channel: Option<Value>,
    pub strength: Option<Value>,
    pub time: Option<Value>,
}

/// Frame-level malformed-input error code (always "403"), matching
/// `parseProtocolMessage`'s single failure code.
pub const ERR_MALFORMED: &str = "403";

pub fn parse_frame(raw: &str) -> Result<Frame, &'static str> {
    let value: Value = serde_json::from_str(raw).map_err(|_| ERR_MALFORMED)?;
    let obj = value.as_object().ok_or(ERR_MALFORMED)?;

    for key in ["type", "clientId", "targetId", "message"] {
        if !obj.contains_key(key) {
            return Err(ERR_MALFORMED);
        }
    }

    let type_ = obj.get("type").expect("checked above").clone();
    let type_is_valid = match &type_ {
        Value::String(s) => !s.is_empty(),
        Value::Number(_) => true,
        _ => false,
    };
    if !type_is_valid {
        return Err(ERR_MALFORMED);
    }

    let client_id = obj
        .get("clientId")
        .and_then(Value::as_str)
        .ok_or(ERR_MALFORMED)?
        .to_string();
    let target_id = obj
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or(ERR_MALFORMED)?
        .to_string();
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .ok_or(ERR_MALFORMED)?
        .to_string();

    if client_id.is_empty() || target_id.is_empty() {
        return Err(ERR_MALFORMED);
    }

    Ok(Frame {
        type_,
        client_id,
        target_id,
        message,
        channel: obj.get("channel").cloned(),
        strength: obj.get("strength").cloned(),
        time: obj.get("time").cloned(),
    })
}

/// The real connection's own clientId must equal either `clientId` or
/// `targetId` on the frame.
pub fn validate_source(frame: &Frame, sender_id: &str) -> bool {
    sender_id == frame.client_id || sender_id == frame.target_id
}

pub fn type_is(type_: &Value, literal: &str) -> bool {
    matches!(type_, Value::String(s) if s == literal)
}

/// Coerces a JSON `type` value to an integer route selector: numbers pass
/// through (fractional values won't match any literal route anyway,
/// mirroring the TS `typeof type === 'number'` branch); digit-only strings
/// are parsed.
pub fn numeric_type(type_: &Value) -> Option<i64> {
    match type_ {
        Value::Number(n) => n.as_i64(),
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse().ok()
        }
        _ => None,
    }
}

pub fn is_app_report_message(message: &str) -> bool {
    message.starts_with("feedback") || message.starts_with("strength")
}

/// `value ?? fallback` semantics: an absent (or explicit `null`) channel
/// field takes `fallback` as-is; a *present* invalid value is rejected
/// outright, never silently defaulted.
pub fn normalize_channel(value: Option<&Value>, fallback: Option<Channel>) -> Option<Channel> {
    let value = match value {
        None | Some(Value::Null) => return fallback,
        Some(v) => v,
    };
    match value {
        Value::Number(n) => match n.as_i64() {
            Some(1) => Some(Channel::A),
            Some(2) => Some(Channel::B),
            _ => None,
        },
        Value::String(s) => match s.as_str() {
            "1" | "A" => Some(Channel::A),
            "2" | "B" => Some(Channel::B),
            "a" => Some(Channel::A),
            "b" => Some(Channel::B),
            _ => None,
        },
        _ => None,
    }
}

/// Mirrors `normalizeNumber`: `typeof value === 'number' ? value :
/// Number(value)`, falling back on non-finite. As a deliberate
/// simplification vs. full JS coercion (`Number(null) === 0`,
/// `Number([]) === 0`, ...), an explicit `null`/absent field falls back
/// directly rather than coercing to 0 — real protocol clients never send
/// `null` for these fields.
pub fn normalize_number(value: Option<&Value>, fallback: i64) -> i64 {
    let parsed = match value {
        None | Some(Value::Null) => return fallback,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Some(0.0)
            } else {
                trimmed.parse::<f64>().ok()
            }
        }
        Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    };
    match parsed {
        Some(f) if f.is_finite() => f.trunc() as i64,
        _ => fallback,
    }
}

pub fn normalize_positive_integer(value: Option<&Value>, fallback: i64) -> i64 {
    let n = normalize_number(value, fallback);
    if n > 0 { n } else { fallback }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_frame_rejects_invalid_json() {
        assert_eq!(parse_frame("not json"), Err(ERR_MALFORMED));
    }

    #[test]
    fn parse_frame_rejects_non_object() {
        assert_eq!(parse_frame("[1,2,3]"), Err(ERR_MALFORMED));
    }

    #[test]
    fn parse_frame_rejects_missing_fields() {
        assert_eq!(
            parse_frame(r#"{"type":"bind","clientId":"a","targetId":"b"}"#),
            Err(ERR_MALFORMED)
        );
    }

    #[test]
    fn parse_frame_rejects_empty_ids() {
        assert_eq!(
            parse_frame(r#"{"type":"bind","clientId":"","targetId":"b","message":""}"#),
            Err(ERR_MALFORMED)
        );
    }

    #[test]
    fn parse_frame_accepts_numeric_type() {
        let frame = parse_frame(r#"{"type":3,"clientId":"a","targetId":"b","message":"strength"}"#)
            .unwrap();
        assert_eq!(frame.type_, json!(3));
        assert_eq!(frame.client_id, "a");
    }

    #[test]
    fn parse_frame_rejects_empty_string_type() {
        assert_eq!(
            parse_frame(r#"{"type":"","clientId":"a","targetId":"b","message":""}"#),
            Err(ERR_MALFORMED)
        );
    }

    #[test]
    fn validate_source_accepts_either_id() {
        let frame =
            parse_frame(r#"{"type":"x","clientId":"a","targetId":"b","message":""}"#).unwrap();
        assert!(validate_source(&frame, "a"));
        assert!(validate_source(&frame, "b"));
        assert!(!validate_source(&frame, "c"));
    }

    #[test]
    fn numeric_type_handles_numbers_and_digit_strings() {
        assert_eq!(numeric_type(&json!(3)), Some(3));
        assert_eq!(numeric_type(&json!("4")), Some(4));
        assert_eq!(numeric_type(&json!("bind")), None);
        assert_eq!(numeric_type(&json!("-1")), None);
        assert_eq!(numeric_type(&json!("")), None);
    }

    #[test]
    fn is_app_report_message_checks_prefixes() {
        assert!(is_app_report_message("feedback-1"));
        assert!(is_app_report_message("strength-1+2+20"));
        assert!(!is_app_report_message("clientMsg"));
    }

    #[test]
    fn normalize_channel_all_spellings() {
        assert_eq!(normalize_channel(Some(&json!(1)), None), Some(Channel::A));
        assert_eq!(normalize_channel(Some(&json!("1")), None), Some(Channel::A));
        assert_eq!(normalize_channel(Some(&json!("A")), None), Some(Channel::A));
        assert_eq!(normalize_channel(Some(&json!("a")), None), Some(Channel::A));
        assert_eq!(normalize_channel(Some(&json!(2)), None), Some(Channel::B));
        assert_eq!(normalize_channel(Some(&json!("2")), None), Some(Channel::B));
        assert_eq!(normalize_channel(Some(&json!("B")), None), Some(Channel::B));
        assert_eq!(normalize_channel(Some(&json!("b")), None), Some(Channel::B));
    }

    #[test]
    fn normalize_channel_missing_uses_fallback() {
        assert_eq!(normalize_channel(None, Some(Channel::A)), Some(Channel::A));
        assert_eq!(normalize_channel(None, None), None);
    }

    #[test]
    fn normalize_channel_present_invalid_is_not_defaulted() {
        // A *present* bad value is rejected outright, even with a fallback
        // available -- fallback only applies when the field is absent.
        assert_eq!(normalize_channel(Some(&json!(5)), Some(Channel::A)), None);
        assert_eq!(normalize_channel(Some(&json!("C")), Some(Channel::A)), None);
    }

    #[test]
    fn normalize_number_basic_cases() {
        assert_eq!(normalize_number(Some(&json!(20)), 0), 20);
        assert_eq!(normalize_number(Some(&json!(20.9)), 0), 20);
        assert_eq!(normalize_number(Some(&json!("20")), 0), 20);
        assert_eq!(normalize_number(Some(&json!("abc")), 5), 5);
        assert_eq!(normalize_number(None, 5), 5);
    }

    #[test]
    fn normalize_positive_integer_rejects_non_positive() {
        assert_eq!(normalize_positive_integer(Some(&json!(5)), 1), 5);
        assert_eq!(normalize_positive_integer(Some(&json!(0)), 1), 1);
        assert_eq!(normalize_positive_integer(Some(&json!(-5)), 1), 1);
        assert_eq!(normalize_positive_integer(None, 1), 1);
    }
}
