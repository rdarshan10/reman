//! Time words at the end of a search: `deploy last week`, `docker yesterday`, `migrate since
//! monday`, `npm test 3 days ago`. Only the trailing phrase is read as time, so a command with
//! `today` in the middle still matches as text. Days are local days.

/// A window of time a query names.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub since: i64,
    pub until: i64,
    /// the words that named it, as typed
    pub label: String,
}

const DAY: i64 = 86_400;
const WEEKDAYS: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];
/// the longest phrase read as time (`since 3 days ago`)
const MAX_WORDS: usize = 4;

/// Split a query into its text and the window its trailing words name. `now` is unix seconds,
/// `offset` the local time's distance from UTC in seconds (east positive).
pub fn split(query: &str, now: i64, offset: i64) -> (String, Option<Window>) {
    let words: Vec<&str> = query.split_whitespace().collect();
    for n in (1..=MAX_WORDS.min(words.len())).rev() {
        let tail = words[words.len() - n..].join(" ");
        if let Some((since, until)) = window(&tail.to_lowercase(), now, offset) {
            return (words[..words.len() - n].join(" "), Some(Window { since, until, label: tail }));
        }
    }
    (query.trim().to_string(), None)
}

/// (since, until) for a whole phrase, or None when it isn't one.
fn window(p: &str, now: i64, offset: i64) -> Option<(i64, i64)> {
    let today = (now + offset).div_euclid(DAY);
    let start = |day: i64| day * DAY - offset; // local midnight of a day number, in unix seconds
    let weekday = |day: i64| (day + 3).rem_euclid(7); // 0 = Monday (1970-01-01 was a Thursday)
    let w: Vec<&str> = p.split(' ').collect();
    let num = |s: &str| s.parse::<i64>().ok().filter(|n| (1..=3650).contains(n));
    let unit = |s: &str| match s.trim_end_matches('s') {
        "day" => Some(1),
        "week" => Some(7),
        _ => None,
    };
    // a point in the past that `since` can start from: (start of that day / week). A weekday may
    // be shortened (`fri`) only after `on`, `since` or `last`: alone, `mon` or `sat` is likelier
    // a word of the command
    let point = |w: &[&str], short_ok: bool| -> Option<i64> {
        match w {
            ["yesterday"] => Some(start(today - 1)),
            ["today"] => Some(start(today)),
            ["last", "week"] => Some(start(today - weekday(today) - 7)),
            ["this", "week"] => Some(start(today - weekday(today))),
            [n, u, "ago"] => Some(start(today - num(n)? * unit(u)?)),
            [d] | ["last", d] => {
                let short_ok = short_ok || w.len() == 2;
                let i = WEEKDAYS.iter().position(|x| x == d || short_ok && d.len() >= 3 && x.starts_with(&**d))? as i64;
                let back = (weekday(today) - i).rem_euclid(7);
                let back = if back == 0 && w.len() == 2 { 7 } else { back };
                Some(start(today - back))
            }
            _ => None,
        }
    };
    Some(match w.as_slice() {
        ["today"] => (start(today), now),
        ["yesterday"] => (start(today - 1), start(today)),
        ["this", "week"] => (start(today - weekday(today)), now),
        ["last", "week"] => (start(today - weekday(today) - 7), start(today - weekday(today))),
        ["this", "month"] => (month_start(today, 0, offset), now),
        ["last", "month"] => (month_start(today, 1, offset), month_start(today, 0, offset)),
        ["last", n, u] if num(n).is_some() && unit(u).is_some() => (start(today - num(n)? * unit(u)? + 1), now),
        [n, u, "ago"] if unit(u) == Some(1) => {
            let d = today - num(n)?;
            (start(d), start(d + 1))
        }
        [n, u, "ago"] => {
            // that whole week
            let d = today - num(n)? * unit(u)?;
            let mon = d - weekday(d);
            (start(mon), start(mon + 7))
        }
        ["since", rest @ ..] => (point(rest, true)?, now),
        ["on", d] => {
            let s = point(&[*d], true)?;
            (s, s + DAY)
        }
        [_] | ["last", _] => {
            let s = point(&w, false)?;
            (s, s + DAY)
        }
        _ => return None,
    })
}

/// A moment as local date and time: `2026-09-30 14:05`.
pub fn stamp(ts: i64, offset: i64) -> String {
    let local = ts + offset;
    let (y, m, d) = civil(local.div_euclid(DAY));
    let mins = local.rem_euclid(DAY) / 60;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", mins / 60, mins % 60)
}

/// One whole local day, named `today`, `yesterday`, a weekday or `YYYY-MM-DD`: (since, until).
pub fn day(name: &str, now: i64, offset: i64) -> Option<(i64, i64)> {
    let name = name.trim().to_lowercase();
    let parts: Vec<i64> = name.split('-').filter_map(|p| p.parse().ok()).collect();
    if let [y, m, d] = parts.as_slice() {
        if (1..=12).contains(m) && (1..=31).contains(d) {
            let start = days_from_civil(*y, *m, *d) * DAY - offset;
            return Some((start, start + DAY));
        }
    }
    let (since, until) = window(&name, now, offset)?;
    // `today` runs until now; as a day it's the whole day
    Some((since, until.max(since + 1)))
}

/// Local midnight of the first day of this month (`back` = 0) or an earlier one.
fn month_start(today: i64, back: i64, offset: i64) -> i64 {
    let (y, m, _) = civil(today);
    let months = y * 12 + (m - 1) - back;
    days_from_civil(months.div_euclid(12), months.rem_euclid(12) + 1, 1) * DAY - offset
}

/// (year, month, day) of a day number (days since 1970-01-01). Howard Hinnant's algorithm.
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    // Thursday 2026-10-01 14:00 local, at UTC+5:30
    const OFF: i64 = 19_800;
    fn at(y: i64, m: i64, d: i64, h: i64) -> i64 {
        days_from_civil(y, m, d) * DAY + h * 3600 - OFF
    }

    #[test]
    fn phrases() {
        let now = at(2026, 10, 1, 14);
        let w = |q: &str| split(q, now, OFF);
        assert_eq!(w("docker yesterday"), ("docker".into(), Some(Window { since: at(2026, 9, 30, 0), until: at(2026, 10, 1, 0), label: "yesterday".into() })));
        let (t, x) = w("deploy last week");
        assert_eq!((t.as_str(), x.as_ref().map(|x| (x.since, x.until))), ("deploy", Some((at(2026, 9, 21, 0), at(2026, 9, 28, 0)))));
        assert_eq!(w("npm test this week").1.map(|x| x.since), Some(at(2026, 9, 28, 0)));
        assert_eq!(w("migrate since monday").1.map(|x| (x.since, x.until)), Some((at(2026, 9, 28, 0), now)));
        assert_eq!(w("pytest 3 days ago").1.map(|x| (x.since, x.until)), Some((at(2026, 9, 28, 0), at(2026, 9, 29, 0))));
        assert_eq!(w("build last month").1.map(|x| (x.since, x.until)), Some((at(2026, 9, 1, 0), at(2026, 10, 1, 0))));
        assert_eq!(w("build this month").1.map(|x| x.since), Some(at(2026, 10, 1, 0)));
        assert_eq!(w("logs last 3 days").1.map(|x| x.since), Some(at(2026, 9, 29, 0)));
        assert_eq!(w("tests on tuesday").1.map(|x| (x.since, x.until)), Some((at(2026, 9, 29, 0), at(2026, 9, 30, 0))));
        // a weekday: the most recent one, today included; `last thursday` is the week before
        assert_eq!(w("deploy thursday").1.map(|x| x.since), Some(at(2026, 10, 1, 0)));
        assert_eq!(w("deploy last thursday").1.map(|x| x.since), Some(at(2026, 9, 24, 0)));
        assert_eq!(w("deploy on fri").1.map(|x| x.since), Some(at(2026, 9, 25, 0)));
        assert_eq!(w("docker logs mon"), ("docker logs mon".into(), None), "alone, `mon` is a word of the command");
        // only a time: browse that window
        assert_eq!(w("yesterday").0, "");
        // time words in the middle are text
        assert_eq!(w("echo today is fine"), ("echo today is fine".into(), None));
        assert_eq!(w("git log"), ("git log".into(), None));
        assert_eq!(w("sleep 3"), ("sleep 3".into(), None));
    }

    #[test]
    fn calendar_round_trips() {
        for d in [-1000, 0, 59, 365, 11_000, 20_727, 30_000] {
            let (y, m, dd) = civil(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(civil(20_727), (2026, 10, 1));
        assert_eq!(stamp(at(2026, 10, 1, 14) + 5 * 60, OFF), "2026-10-01 14:05");
        assert_eq!(stamp(at(2026, 10, 1, 0) - 60, OFF), "2026-09-30 23:59");
    }
}
