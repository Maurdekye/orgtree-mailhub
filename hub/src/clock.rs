//! The hub clock: UTC at millisecond precision, rendered exactly as v1
//! rendered it (`datetime.isoformat(timespec="milliseconds")` with `Z`).

use chrono::{DateTime, SubsecRound, Utc};

pub fn now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(3)
}

pub fn iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

pub fn now_iso() -> String {
    iso(now())
}

/// A v1 timestamp string back to the instant it names.
pub fn parse(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_like_v1() {
        let t = parse("2026-10-08T12:00:00.123456Z").unwrap().trunc_subsecs(3);
        assert_eq!(iso(t), "2026-10-08T12:00:00.123Z");
        assert_eq!(iso(parse("2026-01-01T00:00:00Z").unwrap()), "2026-01-01T00:00:00.000Z");
        assert_eq!(now_iso().len(), 24);
    }
}
