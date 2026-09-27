//! What a notification says. Ported from the homeserver's alarm-uebersetzer.
use crate::alertmanager::Labels;
use jiff::{tz::TimeZone, Timestamp};

pub fn alertname(labels: &Labels) -> String {
    labels
        .get("alertname")
        .cloned()
        .unwrap_or_else(|| "Alert".into())
}

/// The sentence the rule wrote itself; failing that, the label set.
pub fn summary(labels: &Labels, annotations: &Labels) -> String {
    match annotations.get("summary") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => labels
            .iter()
            .filter(|(k, _)| k.as_str() != "alertname")
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// ntfy 2.26.0 turns a message above `message-size-limit` (4096 bytes by
/// default) into an attachment — and with an attachment cache configured,
/// answers a JSON publication of 4 to 8 KB with a 500 (error 50001,
/// "content length mismatch"), anything larger with 413. Measured in audit 3
/// (finding B78, 2026-09-27). Every notification is cut below that, with
/// room to spare for the rest of the message around a cut field.
pub const MESSAGE_MAX_BYTES: usize = 3500;

/// A title is one line on a lock screen; anything longer is noise.
pub const TITLE_MAX_CHARS: usize = 200;

/// Control characters become spaces, and so do the invisible direction
/// marks that make a text read differently than it is stored. A label, a
/// matcher or a silence comment is written by someone else: a `\n` in it
/// forged journal lines (every title is logged), an escape sequence
/// rewrote a terminal, and a comment could restyle the notice it sits in.
pub fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            let bidi = matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
            if c.is_control() || bidi {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// At most `max` characters; a cut text ends in "…" (counted).
pub fn cut_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// At most `max` bytes of UTF-8, never splitting a character; a cut text
/// ends in "…" (counted).
pub fn cut_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let ellipsis = '…'.len_utf8();
    let mut end = max.saturating_sub(ellipsis);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

pub fn clock(at: Timestamp, tz: &TimeZone) -> String {
    at.to_zoned(tz.clone()).strftime("%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn berlin() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("Europe/Berlin").unwrap()
    }

    #[test]
    fn clock_converts_to_local_time_with_dst() {
        let tz = berlin();

        // Summer time (UTC+2): 2026-09-13T12:36:04Z → 14:36 Berlin
        let summer = jiff::Timestamp::from_second(1789302964).unwrap();
        assert_eq!(clock(summer, &tz), "14:36");

        // Winter time (UTC+1): 2026-12-01T12:36:04Z → 13:36 Berlin
        let winter = jiff::Timestamp::from_second(1796128564).unwrap();
        assert_eq!(clock(winter, &tz), "13:36");
    }

    #[test]
    fn clean_turns_control_and_direction_characters_into_spaces() {
        assert_eq!(clean("a\nb\r\u{1b}[2Jc\u{202e}d\te"), "a b  [2Jc d e");
        assert_eq!(clean("Grüße · ok"), "Grüße · ok");
    }

    #[test]
    fn cutting_never_splits_a_character_and_marks_the_cut() {
        assert_eq!(cut_bytes("short", 10), "short");
        // "ü" is two bytes: a cut after 6 bytes must not end inside it.
        let cut = cut_bytes("abcdüüüü", 8);
        assert!(cut.len() <= 8, "{cut}");
        assert!(cut.ends_with('…'), "{cut}");
        let long = "ä".repeat(5000);
        assert!(cut_bytes(&long, MESSAGE_MAX_BYTES).len() <= MESSAGE_MAX_BYTES);
        assert_eq!(cut_chars("abcdef", 4), "abc…");
        assert_eq!(cut_chars("abcd", 4), "abcd");
        assert_eq!(
            cut_chars(&"x".repeat(900), TITLE_MAX_CHARS).chars().count(),
            TITLE_MAX_CHARS
        );
    }

    #[test]
    fn without_a_summary_the_labels_stand_in_sorted_and_without_alertname() {
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("alertname".to_string(), "X".to_string());
        labels.insert("b".to_string(), "2".to_string());
        labels.insert("a".to_string(), "1".to_string());
        assert_eq!(summary(&labels, &Default::default()), "a=1, b=2");
    }
}
