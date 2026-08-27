//! Scheduled warmup slots that cooperate with `daemon.auto_warmup`.
//!
//! Due detection is "this HH:MM has passed today in the configured timezone
//! and has not been completed yet", not an equality check against the current
//! minute string. A missed slot catch-up fires only the latest overdue slot
//! for today. Empty `daemon.timezone` uses the process local timezone.

use chrono::{DateTime, Local, NaiveDateTime, Utc};

/// `HH:MM` with hours 00–23 and minutes 00–59. Surrounding whitespace is ignored.
pub fn parse_schedule_time(value: &str) -> Option<(u8, u8)> {
    let value = value.trim();
    let (hour, minute) = value.split_once(':')?;
    if hour.len() != 2 || minute.len() != 2 || minute.contains(':') {
        return None;
    }
    let hour = hour.parse::<u8>().ok()?;
    let minute = minute.parse::<u8>().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
}

fn format_hhmm(hour: u8, minute: u8) -> String {
    format!("{hour:02}:{minute:02}")
}

/// Trim, validate, deduplicate, and sort. Invalid entries are dropped and warned.
pub fn normalize_warmup_times(times: Vec<String>, warnings: &mut Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in times {
        match parse_schedule_time(&value) {
            Some((hour, minute)) => {
                let stamp = format_hhmm(hour, minute);
                if !normalized.iter().any(|existing| existing == &stamp) {
                    normalized.push(stamp);
                }
            }
            None => warnings.push(format!(
                "config.daemon.warmup_times contains invalid time '{value}'; expected HH:MM"
            )),
        }
    }
    normalized.sort();
    if normalized.len() >= 2 {
        for pair in normalized.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            if minutes_apart(a, b) < 5 * 60 {
                warnings.push(format!(
                    "config.daemon.warmup_times {a} and {b} are less than 5 hours apart; \
                     the later slot is a no-op if the earlier warmup opened a 5h window"
                ));
            }
        }
    }
    normalized
}

fn minutes_apart(a: &str, b: &str) -> i32 {
    let Some((ah, am)) = parse_schedule_time(a) else {
        return 0;
    };
    let Some((bh, bm)) = parse_schedule_time(b) else {
        return 0;
    };
    (bh as i32 * 60 + bm as i32) - (ah as i32 * 60 + am as i32)
}

/// IANA name such as `Asia/Shanghai` or `UTC`. Empty means system local time.
pub fn parse_iana_timezone(name: &str) -> Option<chrono_tz::Tz> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    name.parse().ok()
}

/// Trim. Invalid names are kept so the file still shows what was typed, and
/// runtime falls back to system local time.
pub fn normalize_timezone(value: String, warnings: &mut Vec<String>) -> String {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        return String::new();
    }
    if parse_iana_timezone(&trimmed).is_none() {
        warnings.push(format!(
            "config.daemon.timezone '{trimmed}' is not a valid IANA time zone; using system local time"
        ));
    }
    trimmed
}

/// Wall clock used for due detection: named IANA zone, or process local time
/// when the name is empty or invalid.
pub fn wall_clock(utc: DateTime<Utc>, timezone: &str) -> NaiveDateTime {
    match parse_iana_timezone(timezone) {
        Some(tz) => utc.with_timezone(&tz).naive_local(),
        None => utc.with_timezone(&Local).naive_local(),
    }
}

pub fn schedule_now(timezone: &str) -> NaiveDateTime {
    wall_clock(Utc::now(), timezone)
}

pub fn timezone_label(timezone: &str) -> String {
    let trimmed = timezone.trim();
    if trimmed.is_empty() {
        "(system local)".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Last completed slot identity in `daemon-state.json`: `YYYY-MM-DD HH:MM`.
pub fn parse_slot_stamp(value: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M").ok()
}

pub fn slot_stamp_for(now: NaiveDateTime, hhmm: &str) -> String {
    format!("{} {hhmm}", now.format("%Y-%m-%d"))
}

/// Latest slot that has already passed today and is newer than `last_fired`.
/// Yesterday's slots are not replayed. `now` is the wall clock in the schedule timezone.
pub fn latest_due_slot(
    times: &[String],
    now: NaiveDateTime,
    last_fired: Option<&str>,
) -> Option<String> {
    let last = last_fired.and_then(parse_slot_stamp);
    let date = now.date();
    let mut due = None;
    for hhmm in times {
        let Some((hour, minute)) = parse_schedule_time(hhmm) else {
            continue;
        };
        let Some(naive) = date.and_hms_opt(hour as u32, minute as u32, 0) else {
            continue;
        };
        if naive > now {
            continue;
        }
        if last.is_some_and(|fired| naive <= fired) {
            continue;
        }
        due = Some(hhmm.clone());
    }
    due
}

/// Cache refresh warms only when auto-warmup is on and no slots are configured.
pub fn warmup_on_cache_refresh(auto_warmup: bool, warmup_times: &[String]) -> bool {
    auto_warmup && warmup_times.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};

    fn naive(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .expect("valid date")
            .and_hms_opt(h, min, 0)
            .expect("valid time")
    }

    #[test]
    fn parse_rejects_non_hhmm() {
        assert!(parse_schedule_time("5:00").is_none());
        assert!(parse_schedule_time("24:00").is_none());
        assert!(parse_schedule_time("10:60").is_none());
        assert!(parse_schedule_time("aa:bb").is_none());
        assert_eq!(parse_schedule_time(" 08:00 "), Some((8, 0)));
    }

    #[test]
    fn normalize_trims_dedupes_sorts_and_warns() {
        let mut warnings = Vec::new();
        let times = normalize_warmup_times(
            vec![
                " 13:10 ".into(),
                "08:00".into(),
                "13:10".into(),
                "24:00".into(),
                "08:30".into(),
            ],
            &mut warnings,
        );
        assert_eq!(times, vec!["08:00", "08:30", "13:10"]);
        assert!(warnings.iter().any(|w| w.contains("24:00")));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("08:00") && w.contains("08:30"))
        );
    }

    #[test]
    fn catch_up_fires_latest_overdue_today_only() {
        let times = vec!["08:00".into(), "13:10".into(), "18:20".into()];
        let now = naive(2026, 8, 26, 14, 5);
        assert_eq!(latest_due_slot(&times, now, None).as_deref(), Some("13:10"));
        assert_eq!(
            latest_due_slot(&times, now, Some("2026-08-26 08:00")).as_deref(),
            Some("13:10")
        );
        assert_eq!(latest_due_slot(&times, now, Some("2026-08-26 13:10")), None);
        assert_eq!(
            latest_due_slot(&times, now, Some("2026-08-25 18:20")).as_deref(),
            Some("13:10")
        );
    }

    #[test]
    fn future_slot_is_not_due() {
        let times = vec!["08:00".into(), "18:20".into()];
        let now = naive(2026, 8, 26, 9, 0);
        assert_eq!(latest_due_slot(&times, now, None).as_deref(), Some("08:00"));
    }

    #[test]
    fn cache_refresh_warms_only_without_time_points() {
        assert!(warmup_on_cache_refresh(true, &[]));
        assert!(!warmup_on_cache_refresh(true, &["08:00".into()]));
        assert!(!warmup_on_cache_refresh(false, &[]));
        assert!(!warmup_on_cache_refresh(false, &["08:00".into()]));
    }

    #[test]
    fn empty_timezone_uses_system_local() {
        let utc = Utc
            .with_ymd_and_hms(2026, 8, 26, 1, 0, 0)
            .single()
            .expect("valid utc");
        assert_eq!(wall_clock(utc, ""), utc.with_timezone(&Local).naive_local());
        assert_eq!(timezone_label(""), "(system local)");
        assert_eq!(timezone_label("Asia/Shanghai"), "Asia/Shanghai");
    }

    #[test]
    fn named_timezone_shifts_the_wall_clock() {
        let utc = Utc
            .with_ymd_and_hms(2026, 8, 26, 1, 0, 0)
            .single()
            .expect("valid utc");
        assert_eq!(wall_clock(utc, "UTC"), naive(2026, 8, 26, 1, 0));
        assert_eq!(wall_clock(utc, "Asia/Shanghai"), naive(2026, 8, 26, 9, 0));
        assert_eq!(
            wall_clock(utc, "  Asia/Shanghai "),
            naive(2026, 8, 26, 9, 0)
        );
    }

    #[test]
    fn due_slot_follows_the_timezone_wall_clock_not_utc() {
        let times = vec!["08:00".into()];
        // 07:00 UTC: 08:00 has not passed in UTC, but it has in Asia/Shanghai (15:00).
        assert_eq!(
            latest_due_slot(&times, naive(2026, 8, 26, 7, 0), None),
            None
        );
        assert_eq!(
            latest_due_slot(&times, naive(2026, 8, 26, 15, 0), None).as_deref(),
            Some("08:00")
        );
    }

    #[test]
    fn invalid_timezone_warns_and_is_kept() {
        let mut warnings = Vec::new();
        let tz = normalize_timezone("Not/A_Zone".into(), &mut warnings);
        assert_eq!(tz, "Not/A_Zone");
        assert!(warnings.iter().any(|w| w.contains("Not/A_Zone")));
        assert!(parse_iana_timezone("Not/A_Zone").is_none());
        let utc = Utc
            .with_ymd_and_hms(2026, 8, 26, 1, 0, 0)
            .single()
            .expect("valid utc");
        assert_eq!(
            wall_clock(utc, "Not/A_Zone"),
            utc.with_timezone(&Local).naive_local()
        );
    }
}
