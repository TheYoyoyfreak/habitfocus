use serde::{Deserialize, Deserializer};

/// Parses `90s`, `20m`, `1h30m`, `7d` or a bare number of minutes into milliseconds.
pub fn parse_duration(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(mins) = s.parse::<u64>() {
        return Ok(mins * 60_000);
    }
    let mut total = 0u64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        if c.is_whitespace() {
            continue;
        }
        let unit = match c {
            'd' => 86_400_000,
            'h' => 3_600_000,
            'm' => 60_000,
            's' => 1_000,
            _ => return Err(format!("invalid duration {input:?}: unknown unit {c:?}")),
        };
        let n: u64 = num
            .parse()
            .map_err(|_| format!("invalid duration {input:?}: expected a number before {c:?}"))?;
        total += n * unit;
        num.clear();
    }
    if !num.is_empty() {
        return Err(format!("invalid duration {input:?}: missing unit after {num}"));
    }
    Ok(total)
}

/// Formats milliseconds as `m:ss` or `h:mm:ss`.
pub fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Coarse human duration for long spans: `6d 23h`, `5h 12m`, `12m`, `45s`.
pub fn format_duration_long(ms: u64) -> String {
    let secs = ms / 1000;
    let (d, h, m) = (secs / 86_400, (secs / 3600) % 24, (secs / 60) % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// Formats milliseconds the way the config file spells durations: `1h30m`, `90s`.
pub fn format_duration_config(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        out += &format!("{h}h");
    }
    if m > 0 {
        out += &format!("{m}m");
    }
    if s > 0 || out.is_empty() {
        out += &format!("{s}s");
    }
    out
}

/// Parses a time of day `HH:MM` into milliseconds after midnight.
pub fn parse_time_of_day(input: &str) -> Result<u64, String> {
    let invalid = || format!("invalid time {input:?}: expected HH:MM, e.g. \"04:00\"");
    let (h, m) = input.trim().split_once(':').ok_or_else(invalid)?;
    let h: u64 = h.parse().map_err(|_| invalid())?;
    let m: u64 = m.parse().map_err(|_| invalid())?;
    if h > 23 || m > 59 {
        return Err(invalid());
    }
    Ok((h * 60 + m) * 60_000)
}

pub fn format_time_of_day(ms: u64) -> String {
    let minutes = ms / 60_000;
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Serde helper for `HH:MM` fields.
pub(crate) fn deserialize_time_of_day<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let text = String::deserialize(d)?;
    parse_time_of_day(&text).map_err(serde::de::Error::custom)
}

/// Like `deserialize_duration`, for optional fields.
pub(crate) fn deserialize_opt_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    deserialize_duration(d).map(Some)
}

/// Serde helper: accepts a duration string or an integer number of minutes.
pub(crate) fn deserialize_duration<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Minutes(u64),
        Text(String),
    }
    match Raw::deserialize(d)? {
        Raw::Minutes(m) => Ok(m * 60_000),
        Raw::Text(t) => parse_duration(&t).map_err(serde::de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_duration("90s"), Ok(90_000));
        assert_eq!(parse_duration("20m"), Ok(1_200_000));
        assert_eq!(parse_duration("1h30m"), Ok(5_400_000));
        assert_eq!(parse_duration("15"), Ok(900_000));
        assert_eq!(parse_duration("2d12h"), Ok(216_000_000));
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("1h30").is_err());
    }

    #[test]
    fn config_format_round_trips() {
        for ms in [90_000, 1_200_000, 5_400_000, 3_600_000, 0, 3_661_000] {
            assert_eq!(parse_duration(&format_duration_config(ms)).unwrap_or(0), ms);
        }
        assert_eq!(format_duration_config(5_400_000), "1h30m");
    }

    #[test]
    fn long_durations() {
        assert_eq!(format_duration_long(45_000), "45s");
        assert_eq!(format_duration_long(12 * 60_000), "12m");
        assert_eq!(format_duration_long(5 * 3_600_000 + 12 * 60_000), "5h 12m");
        assert_eq!(format_duration_long(7 * 86_400_000 - 3_600_000), "6d 23h");
    }

    #[test]
    fn time_of_day() {
        assert_eq!(parse_time_of_day("04:30"), Ok(16_200_000));
        assert_eq!(format_time_of_day(16_200_000), "04:30");
        assert!(parse_time_of_day("24:00").is_err());
        assert!(parse_time_of_day("4").is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(format_duration(65_000), "1:05");
        assert_eq!(format_duration(3_725_000), "1:02:05");
    }
}
