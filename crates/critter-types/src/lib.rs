//! Types shared between `critter-server` and the pushers that consume its
//! API. Kept free of web and database dependencies on purpose.

use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

/// URL slugs of every species the server serves.
pub const SPECIES: &[&str] = &["wolf", "coyote", "fox"];

/// Granularity of a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Hour,
    Day,
}

impl Kind {
    /// The lowercase name, as used in the JSON payload and the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Hour => "hour",
            Kind::Day => "day",
        }
    }

    /// Start of the slot that contains `at`.
    pub fn slot_containing(self, at: DateTime<Utc>) -> DateTime<Utc> {
        let length = match self {
            Kind::Hour => TimeDelta::hours(1),
            Kind::Day => TimeDelta::days(1),
        };
        at.duration_trunc(length).expect("truncating a valid timestamp cannot overflow")
    }

    /// The server path for a slot, e.g. `/fox/2026/10/30/15`.
    pub fn path(self, species: &str, slot: DateTime<Utc>) -> String {
        match self {
            Kind::Hour => slot.format(&format!("/{species}/%Y/%m/%d/%H")).to_string(),
            Kind::Day => slot.format(&format!("/{species}/%Y/%m/%d")).to_string(),
        }
    }
}

/// The JSON payload the server returns for a slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Photo {
    pub species: String,
    pub kind: Kind,
    /// Start of the slot, UTC.
    pub slot: DateTime<Utc>,
    pub image_url: String,
    pub source_url: String,
    /// Source slug, e.g. `commons` or `inaturalist`.
    pub source: String,
    pub author: Option<String>,
    pub author_url: Option<String>,
    /// Normalised licence code, e.g. `CC-BY-SA-4.0`.
    pub license: String,
    pub license_url: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn slot_containing_truncates() {
        let at = Utc.with_ymd_and_hms(2026, 10, 30, 15, 42, 7).unwrap();
        assert_eq!(
            Kind::Hour.slot_containing(at),
            Utc.with_ymd_and_hms(2026, 10, 30, 15, 0, 0).unwrap()
        );
        assert_eq!(
            Kind::Day.slot_containing(at),
            Utc.with_ymd_and_hms(2026, 10, 30, 0, 0, 0).unwrap()
        );
    }

    #[test]
    fn path_zero_pads() {
        let slot = Utc.with_ymd_and_hms(2026, 3, 5, 7, 0, 0).unwrap();
        assert_eq!(Kind::Hour.path("fox", slot), "/fox/2026/03/05/07");
        assert_eq!(Kind::Day.path("wolf", slot), "/wolf/2026/03/05");
    }
}
