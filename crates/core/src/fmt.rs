//! Human formatting, shared by the CLI and the window so that "Yesterday"
//! means the same thing in both.
//!
//! Formatting happens here, in Rust, rather than in the frontend: the local
//! timezone and the day boundary are the kind of thing that goes subtly wrong
//! when two layers each have an opinion about them.

use chrono::{DateTime, Local, Utc};

/// "Today", "Yesterday", "Friday", or "Wed 27 Aug" beyond a week.
pub fn day_label(t: DateTime<Utc>) -> String {
    let local = t.with_timezone(&Local).date_naive();
    let today = Local::now().date_naive();
    match (today - local).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => local.format("%A").to_string(),
        _ => local.format("%a %d %b").to_string(),
    }
}

/// A stable key for grouping sessions under one day heading.
pub fn day_key(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%Y-%m-%d").to_string()
}

/// The heading shown above a day's sessions: "Yesterday · Fri 29 Aug".
pub fn day_heading(t: DateTime<Utc>) -> String {
    let local = t.with_timezone(&Local);
    format!("{} · {}", day_label(t), local.format("%a %d %b"))
}

pub fn clock(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%-I:%M %p").to_string()
}

/// "2:15 PM – 6:40 PM", or "2:15 PM – now" while a session is still open.
pub fn time_range(start: DateTime<Utc>, end: Option<DateTime<Utc>>) -> String {
    match end {
        Some(e) => format!("{} – {}", clock(start), clock(e)),
        None => format!("{} – now", clock(start)),
    }
}

/// "4h 25m", "38m", "12s".
pub fn duration(seconds: i64) -> String {
    let (h, m) = (seconds / 3600, (seconds % 3600) / 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{seconds}s")
    }
}

/// A full timestamp for status lines: "Mon 31 Aug, 1:58 PM".
pub fn stamp(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local)
        .format("%a %d %b, %-I:%M %p")
        .to_string()
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Two-letter monogram for an app tile: "Visual Studio Code" becomes "VS",
/// "Figma" becomes "Fi". Recognisable at seventeen pixels.
pub fn monogram(app_name: &str) -> String {
    let words: Vec<&str> = app_name.split_whitespace().filter(|w| !w.is_empty()).collect();
    match words.len() {
        0 => "?".into(),
        1 => words[0].chars().take(2).collect::<String>(),
        _ => words
            .iter()
            .take(2)
            .filter_map(|w| w.chars().next())
            .collect::<String>(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_the_way_people_say_them() {
        assert_eq!(duration(15_900), "4h 25m");
        assert_eq!(duration(2_280), "38m");
        assert_eq!(duration(12), "12s");
        assert_eq!(duration(3_600), "1h 00m");
    }

    #[test]
    fn monograms_are_two_characters() {
        assert_eq!(monogram("Visual Studio Code"), "VS");
        assert_eq!(monogram("Figma"), "Fi");
        assert_eq!(monogram("Windows Terminal"), "WT");
        assert_eq!(monogram(""), "?");
    }

    #[test]
    fn truncation_keeps_the_ellipsis_inside_the_budget() {
        assert_eq!(truncate("abcdefgh", 4).chars().count(), 4);
        assert_eq!(truncate("abc", 10), "abc");
    }
}
