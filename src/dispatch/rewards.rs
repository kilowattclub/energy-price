//! Estimated Axle rewards from signed grid energy, separate from export tariffs.
//! A confirmed payment is reconciled on the server; the Pi never claims settlement.
use std::path::{Path, PathBuf};

use super::Reading as Telemetry;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::config::AxleConfig;
use crate::{AxleDirection, AxleEvent};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RewardEvent {
    id: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    direction: AxleDirection,
    local_date: chrono::NaiveDate,
    reward_p_per_kwh: f64,
    net_grid_kwh: f64,
    coverage_seconds: f64,
    eligible_from: DateTime<Utc>,
    cancelled_at: Option<DateTime<Utc>>,
    accounted_until: DateTime<Utc>,
}

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    events: Vec<RewardEvent>,
}

fn load(path: &Path) -> Result<Ledger, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(e) => Err(e.to_string()),
    }
}

pub struct AxleRewardTracker {
    path: PathBuf,
    tz: Tz,
    max_gap_seconds: f64,
    ledger: Ledger,
    previous: Option<(DateTime<Utc>, f64)>,
    dirty: bool,
}

impl AxleRewardTracker {
    pub fn new(path: PathBuf, tz: Tz, max_gap_seconds: f64) -> Result<Self, String> {
        Ok(Self {
            ledger: load(&path)?,
            path,
            tz,
            max_gap_seconds,
            previous: None,
            dirty: false,
        })
    }

    /// Only call on successful feed reads. A timeout is not a cancellation.
    pub fn confirm(&mut self, event: Option<&AxleEvent>, now: DateTime<Utc>, cfg: &AxleConfig) {
        let id = event.map(|e| {
            format!(
                "axle-{}-{}",
                e.start_time.timestamp(),
                match e.direction {
                    AxleDirection::Export => "export",
                    AxleDirection::Import => "import",
                }
            )
        });
        for row in &mut self.ledger.events {
            if Some(&row.id) != id.as_ref() && row.end > now && row.cancelled_at.is_none() {
                row.cancelled_at = Some(now);
                self.dirty = true;
            }
        }
        if let (Some(event), Some(id)) = (event, id) {
            if let Some(row) = self.ledger.events.iter_mut().find(|row| row.id == id) {
                if row.end != event.end_time || row.cancelled_at.is_some() {
                    if row.cancelled_at.is_some() {
                        row.eligible_from = now.max(row.start);
                    }
                    if event.end_time < row.accounted_until {
                        // A retrospective correction cannot be reconstructed from
                        // cumulative samples. Withdraw the estimate for settlement
                        // rather than retaining energy outside the revised window.
                        log::warn!(target: "kwc.axle.accounting", "event end revised before accounted energy; estimate withdrawn pending settlement");
                        row.net_grid_kwh = 0.0;
                        row.coverage_seconds = 0.0;
                        row.eligible_from = now.max(row.start);
                    }
                    row.end = event.end_time;
                    row.cancelled_at = None;
                    self.dirty = true;
                }
            } else {
                self.ledger.events.push(RewardEvent {
                    id,
                    start: event.start_time,
                    end: event.end_time,
                    direction: event.direction,
                    local_date: event.start_time.with_timezone(&self.tz).date_naive(),
                    reward_p_per_kwh: match event.direction {
                        AxleDirection::Export => cfg.export_reward_p_per_kwh,
                        AxleDirection::Import => cfg.import_reward_p_per_kwh,
                    },
                    net_grid_kwh: 0.0,
                    coverage_seconds: 0.0,
                    eligible_from: now.max(event.start_time),
                    cancelled_at: None,
                    accounted_until: now.max(event.start_time),
                });
                if self.ledger.events.len() > 200 {
                    self.ledger.events.remove(0);
                }
                self.dirty = true;
            }
        }
        self.flush();
    }

    /// Grid power is positive on import. Integrate signed power before clamping
    /// reward: imports within the event must reduce earlier export earnings.
    pub fn observe(&mut self, now: DateTime<Utc>, telemetry: Option<&Telemetry>) {
        let Some(t) = telemetry.filter(|t| {
            super::fresh_reading(t, now) && t.grid_kw.is_finite() && t.grid_kw.abs() <= 1000.0
        }) else {
            self.previous = None;
            return;
        };
        let at = t.at;
        if self.previous.is_some_and(|(previous, _)| at <= previous) {
            return;
        }
        if let Some((previous, grid_kw)) = self.previous {
            let seconds = seconds_between(previous, at);
            if seconds > 0.0 && seconds <= self.max_gap_seconds {
                for row in &mut self.ledger.events {
                    let from = previous
                        .max(row.start)
                        .max(row.eligible_from)
                        .max(row.accounted_until);
                    let to = at.min(row.end).min(row.cancelled_at.unwrap_or(row.end));
                    if to <= from {
                        continue;
                    }
                    let power = |time| {
                        grid_kw + (t.grid_kw - grid_kw) * seconds_between(previous, time) / seconds
                    };
                    let covered = seconds_between(from, to);
                    row.net_grid_kwh += (power(from) + power(to)) / 2.0 * covered / 3600.0;
                    row.coverage_seconds += covered;
                    row.accounted_until = to;
                    self.dirty = true;
                }
            }
        }
        self.previous = Some((at, t.grid_kw));
        self.flush();
    }

    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        match super::save_json(&self.path, &self.ledger) {
            Ok(()) => self.dirty = false,
            Err(error) => {
                log::warn!(target: "kwc.axle.accounting", "reward ledger save failed: {error}")
            }
        }
    }
}

fn seconds_between(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    (to - from).num_milliseconds() as f64 / 1000.0
}

pub fn snapshot(path: &Path, now: DateTime<Utc>) -> serde_json::Value {
    let ledger = match load(path) {
        Ok(ledger) => ledger,
        Err(error) => {
            log::warn!(target: "kwc.axle.accounting", "reward ledger read failed: {error}");
            return serde_json::json!([]);
        }
    };
    serde_json::json!(ledger.events.iter().map(|row| {
        let net_kwh = match row.direction { AxleDirection::Export => -row.net_grid_kwh, AxleDirection::Import => row.net_grid_kwh };
        serde_json::json!({
            "id": row.id, "start": row.start, "end": row.end, "direction": row.direction,
            "local_date": row.local_date, "net_kwh": net_kwh,
            "coverage_seconds": row.coverage_seconds,
            "reward_p_per_kwh": row.reward_p_per_kwh,
            "estimate_gbp": net_kwh.max(0.0) * row.reward_p_per_kwh / 100.0,
            "estimated": true, "settled": false,
            "status": if row.cancelled_at.is_some() { "cancelled" } else if now < row.start { "upcoming" } else if now < row.end { "active" } else { "completed" },
        })
    }).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use std::time::Instant;

    fn at(second: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 11, 17, 0, 0).unwrap() + Duration::seconds(second)
    }
    fn event() -> AxleEvent {
        AxleEvent {
            start_time: at(0),
            end_time: at(3600),
            direction: AxleDirection::Export,
        }
    }
    fn tracker(name: &str) -> AxleRewardTracker {
        let path =
            std::env::temp_dir().join(format!("kwc-rewards-{}-{name}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        AxleRewardTracker::new(path, chrono_tz::Europe::London, 90.0).unwrap()
    }
    fn sample(tracker: &mut AxleRewardTracker, second: i64, grid_kw: f64) {
        let t = Telemetry {
            soc_pct: 50.0,
            battery_kw: -5.0,
            grid_kw,
            load_kw: 1.0,
            solar_kw: 0.0,
            at: at(second),
            read_at: Instant::now(),
        };
        tracker.observe(at(second), Some(&t));
    }
    fn reward(t: &AxleRewardTracker) -> f64 {
        snapshot(&t.path, at(4000))[0]["estimate_gbp"]
            .as_f64()
            .unwrap()
    }
    fn near(left: f64, right: f64) {
        assert!((left - right).abs() < 1e-8, "{left} != {right}");
    }

    #[test]
    fn whole_event_net_grid_contribution_offsets_exports_with_imports() {
        let mut t = tracker("net");
        t.confirm(Some(&event()), at(-60), &AxleConfig::default());
        for second in (0..=1800).step_by(30) {
            sample(&mut t, second, -4.0);
        }
        near(reward(&t), 2.0); // Battery discharge is deliberately different from grid flow.
        for second in (1830..=3600).step_by(30) {
            sample(&mut t, second, 2.0);
        }
        near(reward(&t), 1.025); // Includes the 30-second ramp from export to import.
        near(t.ledger.events[0].coverage_seconds, 3600.0);
    }

    #[test]
    fn clips_interpolated_power_to_exact_event_boundaries() {
        let mut t = tracker("clip");
        let e = AxleEvent {
            end_time: at(60),
            ..event()
        };
        t.confirm(Some(&e), at(-60), &AxleConfig::default());
        sample(&mut t, -30, 0.0);
        sample(&mut t, 30, -6.0);
        sample(&mut t, 90, 0.0);
        near(reward(&t), 4.5 / 60.0);
        near(t.ledger.events[0].coverage_seconds, 60.0);
        sample(&mut t, 120, -5.0);
        near(reward(&t), 4.5 / 60.0);
    }

    #[test]
    fn cancellation_and_reinstatement_do_not_credit_the_gap() {
        let mut t = tracker("cancel");
        t.confirm(Some(&event()), at(-60), &AxleConfig::default());
        sample(&mut t, 0, -6.0);
        sample(&mut t, 30, -6.0);
        t.confirm(None, at(45), &AxleConfig::default());
        sample(&mut t, 60, -6.0);
        sample(&mut t, 90, -6.0);
        near(reward(&t), 6.0 * 45.0 / 3600.0);
        t.confirm(Some(&event()), at(105), &AxleConfig::default());
        sample(&mut t, 120, -6.0);
        near(reward(&t), 0.1);
        near(t.ledger.events[0].coverage_seconds, 60.0);
    }

    #[test]
    fn late_discovery_and_rescheduling_do_not_retroactively_claim_energy() {
        let mut t = tracker("late");
        sample(&mut t, 0, -6.0);
        t.confirm(Some(&event()), at(15), &AxleConfig::default());
        sample(&mut t, 30, -6.0);
        near(reward(&t), 0.025);
        let moved = AxleEvent {
            start_time: at(60),
            ..event()
        };
        t.confirm(Some(&moved), at(45), &AxleConfig::default());
        sample(&mut t, 60, -6.0);
        sample(&mut t, 90, -6.0);
        let rows = snapshot(&t.path, at(120));
        near(rows[0]["estimate_gbp"].as_f64().unwrap(), 0.05);
        near(rows[1]["estimate_gbp"].as_f64().unwrap(), 0.05);
    }

    #[test]
    fn gaps_missing_invalid_and_duplicate_readings_cannot_manufacture_energy() {
        let mut t = tracker("gaps");
        t.confirm(Some(&event()), at(-60), &AxleConfig::default());
        sample(&mut t, 0, -6.0);
        sample(&mut t, 30, -6.0);
        sample(&mut t, 30, -600.0);
        sample(&mut t, 15, -600.0);
        sample(&mut t, 60, -6.0);
        sample(&mut t, 600, -6.0);
        sample(&mut t, 630, f64::NAN);
        sample(&mut t, 660, -6.0);
        t.observe(at(675), None);
        sample(&mut t, 690, -6.0);
        sample(&mut t, 720, -6.0);
        near(reward(&t), 0.15);
        near(t.ledger.events[0].coverage_seconds, 90.0);
    }

    #[test]
    fn restart_keeps_earnings_without_bridging_downtime_or_replaying_old_samples() {
        let mut t = tracker("restart");
        t.confirm(Some(&event()), at(-60), &AxleConfig::default());
        sample(&mut t, 0, -6.0);
        sample(&mut t, 30, -6.0);
        let mut restored = AxleRewardTracker::new(t.path.clone(), t.tz, 90.0).unwrap();
        sample(&mut restored, 0, -6.0);
        sample(&mut restored, 30, -6.0);
        near(reward(&restored), 0.05);
        sample(&mut restored, 600, -6.0);
        sample(&mut restored, 630, -6.0);
        near(reward(&restored), 0.1);
    }

    #[test]
    fn signed_import_rewards_and_zero_floor_use_configured_rates() {
        let mut t = tracker("import");
        let cfg = AxleConfig {
            import_reward_p_per_kwh: 50.0,
            ..AxleConfig::default()
        };
        t.confirm(
            Some(&AxleEvent {
                direction: AxleDirection::Import,
                ..event()
            }),
            at(-60),
            &cfg,
        );
        sample(&mut t, 0, -6.0);
        sample(&mut t, 30, -6.0);
        near(reward(&t), 0.0);
        for second in (60..=120).step_by(30) {
            sample(&mut t, second, 6.0);
        }
        near(reward(&t), 0.025);
    }

    #[test]
    fn event_start_local_date_owns_cross_midnight_reward() {
        let mut t = tracker("midnight");
        let e = AxleEvent {
            start_time: at(21570),
            end_time: at(21630),
            ..event()
        };
        t.confirm(Some(&e), at(21500), &AxleConfig::default());
        sample(&mut t, 21570, -6.0);
        sample(&mut t, 21600, -6.0);
        sample(&mut t, 21630, -6.0);
        let row = &snapshot(&t.path, at(21660))[0];
        assert_eq!(row["local_date"], "2026-09-11");
        near(row["estimate_gbp"].as_f64().unwrap(), 0.1);
    }

    #[test]
    fn corrupt_ledger_is_preserved_and_never_silently_reset() {
        let t = tracker("corrupt");
        std::fs::write(&t.path, "broken").unwrap();
        assert!(AxleRewardTracker::new(t.path.clone(), t.tz, 90.0).is_err());
        assert_eq!(std::fs::read_to_string(&t.path).unwrap(), "broken");
    }

    #[test]
    fn retrospective_end_correction_withdraws_unreconstructable_estimate() {
        let mut t = tracker("correction");
        t.confirm(Some(&event()), at(-60), &AxleConfig::default());
        sample(&mut t, 0, -6.0);
        sample(&mut t, 30, -6.0);
        sample(&mut t, 60, -6.0);
        t.confirm(
            Some(&AxleEvent {
                end_time: at(15),
                ..event()
            }),
            at(60),
            &AxleConfig::default(),
        );
        near(reward(&t), 0.0);
        near(t.ledger.events[0].coverage_seconds, 0.0);
    }
}
