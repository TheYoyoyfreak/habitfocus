//! Numbers derived from sessions; pure functions so they are easy to test.
//! Sessions count towards the local day they started on.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Datelike, Days, NaiveDate, TimeZone, Timelike};

use crate::model::{Book, Reading, Session, Source, Status};

const HOUR_MS: f64 = 3_600_000.0;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BookStats {
    pub sessions: usize,
    pub total_ms: i64,
    pub current_page: Option<u32>,
    pub pages_read: u32,
    /// Pages per hour, from sessions that recorded pages.
    pub pace_pph: Option<f64>,
    /// Estimated time to finish at that pace.
    pub remaining_ms: Option<i64>,
    pub last_read: Option<i64>,
    /// When the first session started: with `last_read`, when the book was
    /// read according to its sessions.
    pub first_read: Option<i64>,
}

/// Stats for one book; `sessions` are that book's.
pub fn book_stats(pages: Option<u32>, sessions: &[Session]) -> BookStats {
    let mut st = BookStats { sessions: sessions.len(), ..BookStats::default() };
    let (mut paced_pages, mut paced_ms) = (0u32, 0i64);
    for s in sessions {
        st.total_ms += s.duration_ms;
        if let Some(p) = s.pages_read() {
            st.pages_read += p;
            // Very short sessions (a quick page-number fix) would skew the pace.
            if s.duration_ms >= 60_000 {
                paced_pages += p;
                paced_ms += s.duration_ms;
            }
        }
    }
    st.current_page = sessions.iter().filter(|s| s.page_to.is_some()).max_by_key(|s| s.ended_at).and_then(|s| s.page_to);
    st.last_read = sessions.iter().map(|s| s.ended_at).max();
    st.first_read = sessions.iter().map(|s| s.started_at).min();
    // Below a minute of timed reading the pace is noise.
    if paced_pages > 0 && paced_ms >= 60_000 {
        let pace = f64::from(paced_pages) / (paced_ms as f64 / HOUR_MS);
        st.pace_pph = Some(pace);
        if let (Some(total), Some(cur)) = (pages, st.current_page) {
            st.remaining_ms = Some((f64::from(total.saturating_sub(cur)) / pace * HOUR_MS) as i64);
        }
    }
    st
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Insights {
    pub today_ms: i64,
    pub week_ms: i64,
    pub month_ms: i64,
    pub year_ms: i64,
    pub total_ms: i64,
    pub sessions: usize,
    pub pages: u32,
    /// Monday first.
    pub by_weekday: [i64; 7],
    pub by_hour: [i64; 24],
    /// The last 30 days, oldest first, today last.
    pub daily: Vec<(NaiveDate, i64)>,
    /// Largest first.
    pub by_source: Vec<(Source, i64)>,
    pub by_device: Vec<(String, i64)>,
    pub by_book: Vec<(String, i64)>,
    /// Days in a row with reading, up to today (or yesterday, if nothing yet today).
    pub current_streak: u32,
    pub longest_streak: u32,
    pub finished_this_year: usize,
}

/// The latest finished pass of each book whose status is "finished": a book
/// counts once, when it was last finished. One moved back to reading (even by
/// mistake) keeps its closed passes, but they don't count. The mobile app
/// counts the same way (`finishedReadings`).
pub fn finished_readings<'a>(books: &[Book], readings: &'a [Reading]) -> Vec<&'a Reading> {
    let finished: std::collections::HashSet<&str> =
        books.iter().filter(|b| b.status == Status::Finished).map(|b| b.id.as_str()).collect();
    let mut latest: HashMap<&str, &Reading> = HashMap::new();
    for r in readings {
        let (Some(at), Some(Status::Finished)) = (r.finished_at, r.outcome) else { continue };
        if !finished.contains(r.book_id.as_str()) {
            continue;
        }
        let seen = latest.entry(r.book_id.as_str()).or_insert(r);
        if seen.finished_at.is_some_and(|s| at > s) {
            *seen = r;
        }
    }
    latest.into_values().collect()
}

/// `books` and `readings` give the books finished this year ([`finished_readings`]).
pub fn insights<Tz: TimeZone>(
    sessions: &[Session],
    books: &[Book],
    readings: &[Reading],
    now_ms: i64,
    tz: &Tz,
) -> Insights {
    let local = |ms: i64| tz.timestamp_millis_opt(ms).single();
    let Some(now) = local(now_ms) else { return Insights::default() };
    let today = now.date_naive();
    let week_start = today - Days::new(u64::from(today.weekday().num_days_from_monday()));
    let first_daily = today - Days::new(29);

    let mut ins = Insights { sessions: sessions.len(), ..Insights::default() };
    let mut daily: HashMap<NaiveDate, i64> = HashMap::new();
    let mut by_source: HashMap<Source, i64> = HashMap::new();
    let mut by_device: HashMap<String, i64> = HashMap::new();
    let mut by_book: HashMap<String, i64> = HashMap::new();
    let mut days = BTreeSet::new();

    for s in sessions {
        let Some(start): Option<DateTime<Tz>> = local(s.started_at) else { continue };
        let (day, ms) = (start.date_naive(), s.duration_ms);
        ins.total_ms += ms;
        ins.pages += s.pages_read().unwrap_or(0);
        if day == today {
            ins.today_ms += ms;
        }
        if day >= week_start && day <= today {
            ins.week_ms += ms;
        }
        if day.year() == today.year() {
            ins.year_ms += ms;
            if day.month() == today.month() {
                ins.month_ms += ms;
            }
        }
        ins.by_weekday[day.weekday().num_days_from_monday() as usize] += ms;
        ins.by_hour[start.hour() as usize] += ms;
        if day >= first_daily && day <= today {
            *daily.entry(day).or_default() += ms;
        }
        *by_source.entry(s.source).or_default() += ms;
        *by_device.entry(s.device.clone()).or_default() += ms;
        *by_book.entry(s.book_id.clone()).or_default() += ms;
        days.insert(day);
    }

    ins.daily = (0..30u64).rev().map(|i| today - Days::new(i)).map(|d| (d, daily.get(&d).copied().unwrap_or(0))).collect();
    ins.by_source = sorted(by_source);
    ins.by_device = sorted(by_device);
    ins.by_book = sorted(by_book);
    (ins.current_streak, ins.longest_streak) = streaks(&days, today);
    ins.finished_this_year = finished_readings(books, readings)
        .into_iter()
        .filter_map(|r| r.finished_at.and_then(local))
        .filter(|d| d.year() == today.year())
        .count();
    ins
}

fn sorted<K: Ord>(map: HashMap<K, i64>) -> Vec<(K, i64)> {
    let mut v: Vec<_> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// (current, longest) runs of consecutive days.
fn streaks(days: &BTreeSet<NaiveDate>, today: NaiveDate) -> (u32, u32) {
    let (mut longest, mut run, mut prev): (u32, u32, Option<NaiveDate>) = (0, 0, None);
    for &d in days {
        run = if prev.and_then(|p| p.succ_opt()) == Some(d) { run + 1 } else { 1 };
        longest = longest.max(run);
        prev = Some(d);
    }
    let mut current = 0;
    let mut day = if days.contains(&today) { today } else { today - Days::new(1) };
    while days.contains(&day) {
        current += 1;
        day = day - Days::new(1);
    }
    (current, longest)
}

/// `1h 05m`, `12m`, `45s`.
pub fn format_duration(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    let (h, m) = (secs / 3600, secs / 60 % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDateTime};

    const MIN: i64 = 60_000;

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).unwrap()
    }

    /// Local time "YYYY-MM-DD HH:MM" in `tz()` as unix ms.
    fn at(s: &str) -> i64 {
        let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        tz().from_local_datetime(&naive).unwrap().timestamp_millis()
    }

    fn session(book: &str, start: &str, minutes: i64, pages: Option<(u32, u32)>, source: Source) -> Session {
        let started_at = at(start);
        Session {
            id: format!("{book}-{start}"),
            book_id: book.into(),
            started_at,
            ended_at: started_at + minutes * MIN,
            duration_ms: minutes * MIN,
            page_from: pages.map(|p| p.0),
            page_to: pages.map(|p| p.1),
            source,
            device: if source == Source::Habitfocus { "desk".into() } else { "laptop".into() },
            external_ref: None,
        }
    }

    #[test]
    fn book_stats_pace_and_remaining() {
        let sessions = [
            session("a", "2026-10-01 20:00", 60, Some((0, 30)), Source::Manual),
            session("a", "2026-10-02 20:00", 30, None, Source::Habitfocus),
            session("a", "2026-10-03 20:00", 60, Some((30, 60)), Source::Manual),
        ];
        let st = book_stats(Some(300), &sessions);
        assert_eq!(st.sessions, 3);
        assert_eq!(st.total_ms, 150 * MIN);
        assert_eq!(st.pages_read, 60);
        assert_eq!(st.current_page, Some(60));
        assert_eq!(st.pace_pph, Some(30.0));
        // 240 pages left at 30 p/h.
        assert_eq!(st.remaining_ms, Some(8 * 60 * MIN));
        assert_eq!(book_stats(Some(300), &[]), BookStats::default());
    }

    #[test]
    fn insights_buckets_and_streaks() {
        // 2026-10-07 is a Wednesday.
        let now = at("2026-10-07 21:00");
        let sessions = [
            session("a", "2026-10-07 08:30", 20, Some((10, 20)), Source::Manual),
            session("a", "2026-10-06 22:00", 40, None, Source::Habitfocus),
            session("b", "2026-10-05 22:15", 30, None, Source::Habitfocus),
            // Sunday before: last week, same month.
            session("b", "2026-10-04 10:00", 10, None, Source::Manual),
            session("b", "2026-09-20 10:00", 60, None, Source::Manual),
            session("b", "2025-12-31 23:30", 60, None, Source::Manual),
        ];
        let readings = [
            Reading { id: "r1".into(), book_id: "a".into(), started_at: 0, finished_at: Some(at("2026-03-01 12:00")), outcome: Some(Status::Finished) },
            Reading { id: "r2".into(), book_id: "b".into(), started_at: 0, finished_at: Some(at("2026-04-01 12:00")), outcome: Some(Status::Abandoned) },
            Reading { id: "r3".into(), book_id: "c".into(), started_at: 0, finished_at: Some(at("2025-04-01 12:00")), outcome: Some(Status::Finished) },
        ];
        let book = |id: &str, status: Status| {
            let mut b = Book::new(id);
            b.id = id.into();
            b.status = status;
            b
        };
        let books = [book("a", Status::Finished), book("b", Status::Abandoned), book("c", Status::Finished)];
        let ins = insights(&sessions, &books, &readings, now, &tz());
        assert_eq!(ins.today_ms, 20 * MIN);
        assert_eq!(ins.week_ms, 90 * MIN);
        assert_eq!(ins.month_ms, 100 * MIN);
        assert_eq!(ins.year_ms, 160 * MIN);
        assert_eq!(ins.total_ms, 220 * MIN);
        assert_eq!(ins.pages, 10);
        assert_eq!(ins.by_weekday[2], 80 * MIN); // Wednesdays: today and 2025-12-31
        assert_eq!(ins.by_hour[22], 70 * MIN);
        assert_eq!(ins.daily.len(), 30);
        assert_eq!(ins.daily.last().unwrap().1, 20 * MIN);
        assert_eq!(ins.daily.iter().map(|d| d.1).sum::<i64>(), 160 * MIN);
        assert_eq!(ins.by_source[0], (Source::Manual, 150 * MIN));
        assert_eq!(ins.by_device[0].0, "laptop");
        assert_eq!(ins.by_book[0], ("b".into(), 160 * MIN));
        assert_eq!((ins.current_streak, ins.longest_streak), (4, 4));
        assert_eq!(ins.finished_this_year, 1);
    }

    #[test]
    fn only_books_finished_now_count_once() {
        let pass = |id: &str, book: &str, finished: i64| Reading {
            id: id.into(),
            book_id: book.into(),
            started_at: 0,
            finished_at: Some(finished),
            outcome: Some(Status::Finished),
        };
        let mut dune = Book::new("Dune");
        dune.id = "a".into();
        dune.status = Status::Finished;
        let mut emma = Book::new("Emma");
        emma.id = "b".into();
        emma.status = Status::Reading; // finished by mistake, set back
        let readings = [pass("r1", "a", 10), pass("r2", "a", 30), pass("r3", "b", 20)];
        let counted = finished_readings(&[dune, emma], &readings);
        assert_eq!(counted.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["r2"], "once, at the last finish");
    }

    #[test]
    fn streak_survives_until_today_is_over() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 7).unwrap();
        let days: BTreeSet<_> = [5, 6].map(|d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap()).into();
        assert_eq!(streaks(&days, today), (2, 2));
        let days: BTreeSet<_> = [1, 2, 3, 5].map(|d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap()).into();
        assert_eq!(streaks(&days, today), (0, 3));
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(45_000), "45s");
        assert_eq!(format_duration(12 * MIN), "12m");
        assert_eq!(format_duration(65 * MIN), "1h 05m");
    }
}
