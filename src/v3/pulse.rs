//! V3 pulse-waveform packetization, mirroring `buildPulseSequence`,
//! `parsePulseMessage`, `fitFramesToLength`, `splitFrames` and
//! `normalizeSendsPerSecond` from v3-server.ts.
//!
//! Returns just the outbound `message` field text per packet; the caller
//! wraps each into a full `{type:'msg', clientId, targetId, message}`
//! frame, keeping this module free of connection/id concerns.

use super::protocol::Channel;

const FRAME_LEN: usize = 16;

/// Parses `"prefix:JSONArray"` where the array is non-empty and every
/// element is a 16-hex-character string (case-insensitive; returned
/// frames are uppercased). Returns `None` on any parse/shape failure,
/// signaling the caller should fall back to raw passthrough.
pub fn parse_pulse_message(message: &str) -> Option<Vec<String>> {
    let separator = message.find(':')?;
    if separator == 0 {
        return None;
    }

    let json_part = &message[separator + 1..];
    let parsed: serde_json::Value = serde_json::from_str(json_part).ok()?;
    let items = parsed.as_array()?;
    if items.is_empty() {
        return None;
    }

    let mut frames = Vec::with_capacity(items.len());
    for item in items {
        let s = item.as_str()?;
        if s.len() != FRAME_LEN || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        frames.push(s.to_ascii_uppercase());
    }
    Some(frames)
}

/// Cycles `frames` to fill exactly `total_frames` entries.
pub fn fit_frames_to_length(frames: &[String], total_frames: usize) -> Vec<String> {
    if frames.is_empty() {
        return Vec::new();
    }
    (0..total_frames)
        .map(|i| frames[i % frames.len()].clone())
        .collect()
}

/// Splits `frames` into `packet_count` proportional contiguous chunks,
/// dropping any that end up empty.
pub fn split_into_chunks(frames: &[String], packet_count: usize) -> Vec<Vec<String>> {
    if packet_count == 0 {
        return Vec::new();
    }
    let len = frames.len();
    (0..packet_count)
        .map(|i| {
            let start = i * len / packet_count;
            let end = (i + 1) * len / packet_count;
            frames[start..end].to_vec()
        })
        .filter(|chunk| !chunk.is_empty())
        .collect()
}

/// Clamps the configured send rate to `[1, 10]` packets/sec, truncated.
pub fn normalize_sends_per_second(value: i64) -> i64 {
    value.clamp(1, 10)
}

pub struct PulseSequence {
    pub messages: Vec<String>,
    pub packet_count: usize,
    pub total_frames: Option<usize>,
    pub parsed: bool,
}

pub fn build_pulse_sequence(
    raw_message: &str,
    channel: Channel,
    time: i64,
    sends_per_second: i64,
) -> PulseSequence {
    let packet_count = std::cmp::max(1, time * sends_per_second) as usize;

    match parse_pulse_message(raw_message) {
        None => PulseSequence {
            messages: std::iter::repeat_n(format!("pulse-{raw_message}"), packet_count).collect(),
            packet_count,
            total_frames: None,
            parsed: false,
        },
        Some(frames) => {
            let total_frames = std::cmp::max(1, time * 10) as usize;
            let fitted = fit_frames_to_length(&frames, total_frames);
            let chunks = split_into_chunks(&fitted, packet_count);
            let messages: Vec<String> = chunks
                .iter()
                .map(|chunk| {
                    let json_arr = serde_json::to_string(chunk).expect("Vec<String> serializes");
                    format!("pulse-{}:{}", channel.letter(), json_arr)
                })
                .collect();
            PulseSequence {
                packet_count: messages.len(),
                messages,
                total_frames: Some(total_frames),
                parsed: true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pulse_message_valid_uppercases_and_case_insensitive() {
        let frames = parse_pulse_message(r#"A:["0a0a0a0a0a0a0a0a","0B0B0B0B0B0B0B0B"]"#).unwrap();
        assert_eq!(frames, vec!["0A0A0A0A0A0A0A0A", "0B0B0B0B0B0B0B0B"]);
    }

    #[test]
    fn parse_pulse_message_rejects_no_colon() {
        assert_eq!(parse_pulse_message("justastring"), None);
    }

    #[test]
    fn parse_pulse_message_rejects_colon_at_start() {
        assert_eq!(parse_pulse_message(r#":["0a0a0a0a0a0a0a0a"]"#), None);
    }

    #[test]
    fn parse_pulse_message_rejects_empty_array() {
        assert_eq!(parse_pulse_message("A:[]"), None);
    }

    #[test]
    fn parse_pulse_message_rejects_wrong_length_or_non_hex() {
        assert_eq!(parse_pulse_message(r#"A:["0a0a"]"#), None);
        assert_eq!(parse_pulse_message(r#"A:["zzzzzzzzzzzzzzzz"]"#), None);
    }

    #[test]
    fn parse_pulse_message_rejects_invalid_json() {
        assert_eq!(parse_pulse_message("A:not json"), None);
    }

    #[test]
    fn fit_frames_cycles_modulo() {
        let frames = vec!["AA".to_string(), "BB".to_string(), "CC".to_string()];
        let fitted = fit_frames_to_length(&frames, 7);
        assert_eq!(fitted, vec!["AA", "BB", "CC", "AA", "BB", "CC", "AA"]);
    }

    #[test]
    fn fit_frames_empty_input_yields_empty() {
        assert_eq!(fit_frames_to_length(&[], 5), Vec::<String>::new());
    }

    #[test]
    fn split_into_chunks_proportional_and_drops_empty() {
        let frames: Vec<String> = (0..5).map(|i| i.to_string()).collect();
        // 5 frames into 10 packets: several chunks will be empty and get dropped.
        let chunks = split_into_chunks(&frames, 10);
        assert_eq!(chunks.len(), 5);
        for chunk in &chunks {
            assert_eq!(chunk.len(), 1);
        }
    }

    #[test]
    fn split_into_chunks_even_division() {
        let frames: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        let chunks = split_into_chunks(&frames, 5);
        assert_eq!(chunks.len(), 5);
        for chunk in &chunks {
            assert_eq!(chunk.len(), 2);
        }
    }

    #[test]
    fn normalize_sends_per_second_clamps() {
        assert_eq!(normalize_sends_per_second(0), 1);
        assert_eq!(normalize_sends_per_second(-5), 1);
        assert_eq!(normalize_sends_per_second(5), 5);
        assert_eq!(normalize_sends_per_second(50), 10);
    }

    #[test]
    fn build_pulse_sequence_raw_fallback_repeats_verbatim() {
        let seq = build_pulse_sequence("legacywave", Channel::A, 2, 1);
        assert!(!seq.parsed);
        assert_eq!(seq.packet_count, 2);
        assert_eq!(seq.messages, vec!["pulse-legacywave", "pulse-legacywave"]);
        assert_eq!(seq.total_frames, None);
    }

    #[test]
    fn build_pulse_sequence_parsed_uses_resolved_channel_letter_not_original_prefix() {
        // Sent with prefix "X", but channel resolves to B -- output must use B.
        let seq = build_pulse_sequence(
            r#"X:["0a0a0a0a0a0a0a0a"]"#,
            Channel::B,
            1,
            1,
        );
        assert!(seq.parsed);
        assert_eq!(seq.total_frames, Some(10));
        assert_eq!(seq.packet_count, 1);
        assert_eq!(seq.messages.len(), 1);
        assert!(seq.messages[0].starts_with("pulse-B:"));
    }

    #[test]
    fn build_pulse_sequence_packet_count_is_max_one() {
        let seq = build_pulse_sequence("raw", Channel::A, 0, 0);
        assert_eq!(seq.packet_count, 1);
        assert_eq!(seq.messages.len(), 1);
    }
}
