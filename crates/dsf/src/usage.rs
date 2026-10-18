//! The subscription usage Claude last reported, kept so dsf can stop short
//! of the weekly allowance (`weekly_usage_cap`) and show where it stands.
//! Every run spends the same allowance, so there's one reading, not one
//! per run.

use anyhow::Result;
use dsf_core::schedule::usage_cap_until;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::agent::Usage;
use crate::store::Store;

/// `kv` key: the latest [`UsageReading`].
pub const USAGE: &str = "usage";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageReading {
    pub seen_at: Timestamp,
    pub usage: Usage,
}

impl UsageReading {
    /// The stored reading. One that can't be read counts as none: the
    /// next agent stage replaces it.
    pub fn load(store: &Store) -> Result<Option<Self>> {
        Ok(store
            .kv_get(USAGE)?
            .and_then(|json| serde_json::from_str(&json).ok()))
    }

    pub fn save(&self, store: &mut Store) -> Result<()> {
        store.kv_set(USAGE, &serde_json::to_string(self)?)
    }

    /// While the weekly allowance is used to `cap` percent or more, when
    /// work may start again.
    pub fn capped_until(&self, cap: u8, now: Timestamp) -> Option<Timestamp> {
        let weekly = self.usage.seven_day?;
        usage_cap_until(weekly.utilization, weekly.resets_at, cap, now)
    }

    /// Why the cap stops work.
    pub fn over_cap(&self, cap: u8) -> String {
        let used = self.usage.seven_day.map_or(0.0, |w| w.utilization);
        format!(
            "the weekly Claude allowance is {}% used, over the {cap}% cap",
            percent(used)
        )
    }
}

/// A utilization as a whole percentage, rounded down so a reading just
/// under a cap never shows as at it.
pub fn percent(utilization: f64) -> u32 {
    // Clamped first, so the cast can neither truncate nor lose a sign.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let percent = (utilization * 100.0).floor().clamp(0.0, 999.0) as u32;
    percent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Window;

    fn reading(utilization: f64, resets_at: &str) -> UsageReading {
        UsageReading {
            seen_at: "2026-12-01T23:00:00Z".parse().unwrap(),
            usage: Usage {
                status: "allowed".to_owned(),
                five_hour: None,
                seven_day: Some(Window {
                    utilization,
                    resets_at: Some(resets_at.parse().unwrap()),
                }),
            },
        }
    }

    #[test]
    fn a_reading_round_trips_through_the_store() {
        let mut store = Store::open_in_memory().unwrap();
        assert_eq!(UsageReading::load(&store).unwrap(), None);
        let saved = reading(0.42, "2026-12-04T09:00:00Z");
        saved.save(&mut store).unwrap();
        assert_eq!(UsageReading::load(&store).unwrap(), Some(saved));
        store.kv_set(USAGE, "{not json").unwrap();
        assert_eq!(UsageReading::load(&store).unwrap(), None);
    }

    #[test]
    fn over_the_cap_until_just_after_the_reset() {
        let now = "2026-12-02T00:00:00Z".parse().unwrap();
        let over = reading(0.857, "2026-12-04T09:00:00Z");
        assert_eq!(
            over.capped_until(80, now),
            Some("2026-12-04T09:02:00Z".parse().unwrap())
        );
        assert_eq!(over.capped_until(90, now), None);
        assert_eq!(
            over.over_cap(80),
            "the weekly Claude allowance is 85% used, over the 80% cap"
        );
        let mut five_hour_only = over;
        five_hour_only.usage.seven_day = None;
        assert_eq!(five_hour_only.capped_until(80, now), None);
    }

    #[test]
    fn percentages_round_down() {
        assert_eq!(percent(0.799_9), 79);
        assert_eq!(percent(0.8), 80);
        assert_eq!(percent(1.2), 120);
        assert_eq!(percent(-0.5), 0);
    }
}
