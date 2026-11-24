//! Cron expression handling for `restart_cron`.
//!
//! The `cron` crate wants 6 or 7 fields (seconds first) and numbers the days
//! of the week from Sunday = 1, while the documented form (README) is the
//! classic 5-field one where Sunday is 0 or 7 and Monday is 1.
//! [`normalize_cron`] accepts both and makes the 5-field form mean what a
//! user of crontab expects. Times are UTC.

use std::collections::BTreeSet;
use std::str::FromStr;

use crate::error::{Error, Result};

const DAY_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// Parse a cron expression.
///
/// - 5 fields (`min hour dom month dow`): `0 ` (seconds) is prepended and the
///   day-of-week field is translated from standard numbering (0 or 7 =
///   Sunday, 1 = Monday) to day names.
/// - 6 or 7 fields: passed to the `cron` crate as written (seconds first,
///   Quartz numbering, Sunday = 1).
/// - anything else is an error.
pub fn normalize_cron(expr: &str) -> Result<cron::Schedule> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let full = match fields.len() {
        5 => {
            let dow = translate_dow(fields[4]).map_err(|why| bad(expr, &why))?;
            format!(
                "0 {} {} {} {} {}",
                fields[0], fields[1], fields[2], fields[3], dow
            )
        }
        6 | 7 => fields.join(" "),
        n => {
            return Err(bad(
                expr,
                &format!("expected 5, 6 or 7 fields, found {}", n),
            ))
        }
    };
    cron::Schedule::from_str(&full).map_err(|e| bad(expr, &e.to_string()))
}

fn bad(expr: &str, why: &str) -> Error {
    Error::InvalidSpec(format!("invalid cron expression '{}': {}", expr, why))
}

/// Translate a standard-numbered day-of-week field to day names.
/// Shapes that are not understood are rejected rather than guessed.
fn translate_dow(field: &str) -> std::result::Result<String, String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut days: BTreeSet<usize> = BTreeSet::new();

    for elem in field.split(',') {
        if elem == "*" {
            pieces.push("*".to_string());
        } else if let Some(step) = elem.strip_prefix("*/") {
            if step.is_empty() || !step.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("unsupported day-of-week '{}'", elem));
            }
            pieces.push(elem.to_string());
        } else if elem.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
            && !has_mixed_range(elem)
        {
            // Names and name ranges are validated by the schedule parser.
            pieces.push(elem.to_string());
        } else {
            expand_range(elem, &mut days)
                .ok_or_else(|| format!("unsupported day-of-week '{}'", elem))?;
        }
    }

    pieces.extend(days.into_iter().map(|d| DAY_NAMES[d].to_string()));
    Ok(pieces.join(","))
}

fn has_mixed_range(elem: &str) -> bool {
    let range = elem.split('/').next().unwrap_or("");
    range
        .split_once('-')
        .is_some_and(|(lo, hi)| parse_num(lo).is_some() != parse_num(hi).is_some())
}

/// A single number or a range with an optional step. Numbers use crontab's 0..=7.
fn expand_range(elem: &str, days: &mut BTreeSet<usize>) -> Option<()> {
    let (range, step) = match elem.split_once('/') {
        Some((r, s)) => (r, Some(parse_num(s)?)),
        None => (elem, None),
    };
    let (lo, hi) = match range.split_once('-') {
        Some((a, b)) => {
            let lo = weekday_number(a)?;
            let mut hi = weekday_number(b)?;
            if hi == 0 && lo > 0 && parse_num(b).is_none() {
                hi = 7; // a named Sunday at the end of a range
            }
            (lo, hi)
        }
        None if step.is_none() => {
            let n = parse_num(range)?;
            (n, n)
        }
        None => return None,
    };
    let step = step.unwrap_or(1);
    if lo > hi || hi > 7 || step == 0 {
        return None;
    }
    days.extend((lo..=hi).step_by(step).map(|d| d % 7));
    Some(())
}

fn weekday_number(s: &str) -> Option<usize> {
    use chrono::Weekday;
    if let Some(day) = parse_num(s) {
        return Some(day);
    }
    let day = match s.to_ascii_lowercase().as_str() {
        "tues" => Weekday::Tue,
        "thurs" => Weekday::Thu,
        _ => s.parse().ok()?,
    };
    Some(day.num_days_from_sunday() as usize)
}

fn parse_num(s: &str) -> Option<usize> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Utc, Weekday};

    fn fire_days(expr: &str) -> std::collections::HashSet<Weekday> {
        let s = normalize_cron(expr).unwrap();
        s.upcoming(Utc).take(14).map(|t| t.weekday()).collect()
    }

    #[test]
    fn mixed_day_ranges_use_crontab_numbering() {
        let expected = fire_days("0 4 * * 1-5");
        assert_eq!(fire_days("0 4 * * MON-5"), expected);
        assert_eq!(fire_days("0 4 * * 1-FRI"), expected);
    }

    #[test]
    fn normalize_cron_five_field_prepends_seconds() {
        use chrono::Timelike;
        let s = normalize_cron("0 4 * * *").unwrap();
        for t in s.upcoming(Utc).take(3) {
            assert_eq!((t.hour(), t.minute(), t.second()), (4, 0, 0));
        }
    }

    #[test]
    fn normalize_cron_translates_numeric_dow() {
        assert_eq!(
            fire_days("0 4 * * 1"),
            std::collections::HashSet::from([Weekday::Mon])
        );
        assert_eq!(
            fire_days("0 4 * * 0"),
            std::collections::HashSet::from([Weekday::Sun])
        );
        assert_eq!(
            fire_days("0 4 * * 7"),
            std::collections::HashSet::from([Weekday::Sun])
        );
        assert_eq!(
            fire_days("0 4 * * 1-5"),
            std::collections::HashSet::from([
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri
            ])
        );
        assert_eq!(
            fire_days("0 4 * * 1,3"),
            std::collections::HashSet::from([Weekday::Mon, Weekday::Wed])
        );
        // 5-7 wraps over Sunday (7 is Sunday).
        assert_eq!(
            fire_days("0 4 * * 5-7"),
            std::collections::HashSet::from([Weekday::Fri, Weekday::Sat, Weekday::Sun])
        );
        // Step over a numeric range.
        assert_eq!(
            fire_days("0 4 * * 1-5/2"),
            std::collections::HashSet::from([Weekday::Mon, Weekday::Wed, Weekday::Fri])
        );
        // Names and `*` pass through.
        assert_eq!(
            fire_days("0 4 * * MON"),
            std::collections::HashSet::from([Weekday::Mon])
        );
        assert_eq!(fire_days("0 4 * * *").len(), 7);
        assert_eq!(fire_days("0 4 * * */2").len(), 4);
    }

    #[test]
    fn normalize_cron_rejects_garbage() {
        for bad in [
            "nonsense",
            "",
            "* * * *",
            "0 4 * * 8",
            "0 4 * * 5-1",
            "0 4 * * 1/2",
            "0 4 * * -1",
            "0 4 * * 1-",
            "0 4 * * */x",
            "61 4 * * *",
            "0 25 * * *",
            "0 4 * * * * * *",
        ] {
            assert!(
                matches!(normalize_cron(bad), Err(Error::InvalidSpec(_))),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn normalize_cron_passes_six_field_through() {
        // Six fields are seconds-first Quartz: Sunday is 1, so `1` is Sunday.
        use chrono::Timelike;
        let s = normalize_cron("*/10 * * * * *").unwrap();
        let t = s.upcoming(Utc).next().unwrap();
        assert_eq!(t.second() % 10, 0);
        assert_eq!(
            s.upcoming(Utc).take(2).count(),
            2,
            "six fields parse as written"
        );
        assert_eq!(
            fire_days("0 0 4 * * 1"),
            std::collections::HashSet::from([Weekday::Sun])
        );
        assert!(normalize_cron("0 0 4 * * * 2099").is_ok(), "seven fields");
    }

    #[test]
    fn every_cron_example_in_the_docs_parses() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let re = regex::Regex::new(r#"restart_cron\s*=\s*"([^"]*)""#).unwrap();
        let mut files = vec![root.join("README.md")];
        for entry in std::fs::read_dir(root.join("examples")).unwrap() {
            files.push(entry.unwrap().path());
        }
        let mut found = 0;
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            for cap in re.captures_iter(&text) {
                found += 1;
                normalize_cron(&cap[1]).unwrap_or_else(|e| panic!("{}: {}", file.display(), e));
            }
        }
        assert!(
            found >= 4,
            "expected the README and example cron lines, found {found}"
        );
    }
}
