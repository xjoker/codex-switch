//! Scheduled warmup slots that cooperate with `daemon.auto_warmup`.
//!
//! Due detection is "this local HH:MM has passed today and has not been
//! completed yet", not an equality check against the current minute string.
//! A missed slot catch-up fires only the latest overdue slot for today.

use chrono::{DateTime, Local, TimeZone};

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

/// Last completed slot identity in `daemon-state.json`: `YYYY-MM-DD HH:MM`.
pub fn parse_slot_stamp(value: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M").ok()
}

pub fn slot_stamp_for(now: DateTime<Local>, hhmm: &str) -> String {
    format!("{} {hhmm}", now.format("%Y-%m-%d"))
}

/// Latest slot that has already passed today and is newer than `last_fired`.
/// Yesterday's slots are not replayed.
pub fn latest_due_slot(
    times: &[String],
    now: DateTime<Local>,
    last_fired: Option<&str>,
) -> Option<String> {
    let last = last_fired.and_then(parse_slot_stamp);
    let date = now.date_naive();
    let mut due = None;
    for hhmm in times {
        let Some((hour, minute)) = parse_schedule_time(hhmm) else {
            continue;
        };
        let Some(naive) = date.and_hms_opt(hour as u32, minute as u32, 0) else {
            continue;
        };
        let slot_local = match Local.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) => dt,
            _ => continue,
        };
        if slot_local > now {
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
    use chrono::TimeZone;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("valid local datetime")
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
        let now = local(2026, 8, 26, 14, 5);
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
        let now = local(2026, 8, 26, 9, 0);
        assert_eq!(latest_due_slot(&times, now, None).as_deref(), Some("08:00"));
    }

    #[test]
    fn cache_refresh_warms_only_without_time_points() {
        assert!(warmup_on_cache_refresh(true, &[]));
        assert!(!warmup_on_cache_refresh(true, &["08:00".into()]));
        assert!(!warmup_on_cache_refresh(false, &[]));
        assert!(!warmup_on_cache_refresh(false, &["08:00".into()]));
    }
}
