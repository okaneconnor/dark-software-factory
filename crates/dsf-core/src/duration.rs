//! Durations as people write them in config and workflow files: `90s`,
//! `30m`, `1h30m`, `3d`.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DurationError {
    #[error("empty duration")]
    Empty,
    #[error("expected a number at `{0}`")]
    ExpectedNumber(String),
    #[error("`{0}` has no unit (use s, m, h or d)")]
    MissingUnit(String),
    #[error("unknown unit `{0}` (use s, m, h or d)")]
    UnknownUnit(String),
    #[error("duration is too large")]
    Overflow,
}

/// Parses one or more `<integer><unit>` parts, optionally separated by
/// spaces. Units are `s`, `m`, `h` and `d` (24 hours). Only whole seconds
/// are representable, which is all a schedule or timeout needs.
pub fn parse(text: &str) -> Result<Duration, DurationError> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err(DurationError::Empty);
    }
    let mut total: u64 = 0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return Err(DurationError::ExpectedNumber(rest.to_owned()));
        }
        let (number, tail) = rest.split_at(digits);
        let unit_len = tail
            .find(|c: char| c.is_ascii_digit() || c.is_whitespace())
            .unwrap_or(tail.len());
        let (unit, tail) = tail.split_at(unit_len);
        let seconds_per_unit = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            "" => return Err(DurationError::MissingUnit(number.to_owned())),
            other => return Err(DurationError::UnknownUnit(other.to_owned())),
        };
        let value: u64 = number.parse().map_err(|_| DurationError::Overflow)?;
        total = value
            .checked_mul(seconds_per_unit)
            .and_then(|seconds| total.checked_add(seconds))
            .ok_or(DurationError::Overflow)?;
        rest = tail.trim_start();
    }
    Ok(Duration::from_secs(total))
}

/// Formats whole seconds in the same notation [`parse`] reads, largest
/// unit first: `1h30m`, `3d`, `0s`.
pub fn format(duration: Duration) -> String {
    let mut seconds = duration.as_secs();
    if seconds == 0 {
        return "0s".to_owned();
    }
    let mut out = String::new();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)] {
        let count = seconds / size;
        if count > 0 {
            out.push_str(&count.to_string());
            out.push_str(unit);
            seconds %= size;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_single_and_compound_durations() {
        assert_eq!(parse("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse("30m"), Ok(Duration::from_mins(30)));
        assert_eq!(parse("1h30m"), Ok(Duration::from_mins(90)));
        assert_eq!(parse("1h 30m"), Ok(Duration::from_mins(90)));
        assert_eq!(parse(" 3d "), Ok(Duration::from_hours(72)));
    }

    #[test]
    fn rejects_malformed_durations() {
        assert_eq!(parse(""), Err(DurationError::Empty));
        assert_eq!(parse("30"), Err(DurationError::MissingUnit("30".into())));
        assert_eq!(parse("5w"), Err(DurationError::UnknownUnit("w".into())));
        assert_eq!(parse("m"), Err(DurationError::ExpectedNumber("m".into())));
        assert_eq!(
            parse("-5m"),
            Err(DurationError::ExpectedNumber("-5m".into()))
        );
        assert_eq!(parse("99999999999999999999s"), Err(DurationError::Overflow));
        assert_eq!(parse("999999999999999d"), Err(DurationError::Overflow));
    }

    #[test]
    fn formats_largest_unit_first() {
        assert_eq!(format(Duration::ZERO), "0s");
        assert_eq!(format(Duration::from_mins(90)), "1h30m");
        assert_eq!(format(Duration::from_secs(90_061)), "1d1h1m1s");
    }

    proptest! {
        #[test]
        fn format_round_trips(seconds in 0u64..10_000_000_000) {
            let duration = Duration::from_secs(seconds);
            prop_assert_eq!(parse(&format(duration)), Ok(duration));
        }

        #[test]
        fn parse_never_panics(text in "\\PC{0,24}") {
            let _ = parse(&text);
        }
    }
}
