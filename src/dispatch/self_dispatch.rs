//! Local ownership of opted-in Axle events. All overrides have a short deadline.
use std::path::PathBuf;

use super::fresh_reading as fresh_telemetry;
use super::BatteryLimits as BatteryConfig;
use super::Reading as Telemetry;
use super::{Action as ModeDecision, DispatchMode as PlannedMode};
use crate::{AxleDirection, AxleEvent};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

const FEED_GRACE: Duration = Duration::minutes(5);

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
struct State {
    event: Option<AxleEvent>,
    confirmed_at: Option<DateTime<Utc>>,
    owned: bool,
    limited: bool,
}

pub struct AxleSelfDispatch {
    path: PathBuf,
    state: State,
    last_attempt: Option<DateTime<Utc>>,
    verified_flow: bool,
    write_failed: bool,
}

impl AxleSelfDispatch {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let state: State = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.to_string()),
        };
        if state.event.as_ref().is_some_and(|event| {
            event.start_time >= event.end_time
                || event.end_time - event.start_time > Duration::hours(24)
                || state.confirmed_at.is_none()
        }) {
            return Err("invalid saved Axle self-dispatch event".into());
        }
        Ok(Self {
            path,
            state,
            last_attempt: None,
            verified_flow: false,
            write_failed: false,
        })
    }

    pub fn event(&self) -> Option<&AxleEvent> {
        self.state.event.as_ref()
    }

    /// Only successful API responses call this. Empty/opted-out means cancel;
    /// a connection error retains the last confirmation for a bounded grace.
    pub fn observe(&mut self, event: Option<&AxleEvent>, now: DateTime<Utc>) -> Result<(), String> {
        let before = self.state.clone();
        let event = event.filter(|e| e.end_time > now);
        let changed = self.state.event.as_ref() != event;
        if changed {
            self.state.event = event.cloned();
            self.state.limited = false;
            self.last_attempt = None;
            self.verified_flow = false;
            self.write_failed = false;
            if let Some(event) = event {
                log::info!(target: "kwc.axle", "self-dispatch {:?} event confirmed: {} to {}", event.direction, event.start_time, event.end_time);
            }
        }
        // Avoid a disk write for every poll; the saved confirmation may lag a
        // minute, but never claims more freshness than was actually observed.
        if changed
            || before
                .confirmed_at
                .is_none_or(|at| now - at >= Duration::minutes(1))
        {
            self.state.confirmed_at = Some(now);
            self.save()?;
        }
        Ok(())
    }

    pub fn active(&self, now: DateTime<Utc>) -> bool {
        self.event()
            .is_some_and(|e| e.start_time <= now && now < e.end_time)
    }

    /// Includes recovery after a restart outside the old event window.
    pub fn finish_if_due(&mut self, now: DateTime<Utc>) -> Result<bool, String> {
        if self.state.owned && !self.active(now) {
            self.state.owned = false;
            self.save()?;
            log::info!(target: "kwc.axle", "self-dispatch event ended or cancelled; resuming tariff control");
            return Ok(true);
        }
        Ok(false)
    }

    pub fn decision(
        &mut self,
        cfg: &BatteryConfig,
        now: DateTime<Utc>,
        telemetry: Option<&Telemetry>,
    ) -> Result<Option<ModeDecision>, String> {
        if !self.active(now) {
            return Ok(None);
        }
        let event = self.state.event.clone().expect("active event");
        let before = self.state.clone();
        self.state.owned = true;
        if !before.owned {
            log::info!(target: "kwc.axle", "self-dispatch event started; Pi owns inverter control until {}", event.end_time);
        }
        let end = event.end_time.min(
            DateTime::from_timestamp((now.timestamp().div_euclid(60) + 2) * 60, 0)
                .expect("minute boundary"),
        );
        let mut decision = ModeDecision {
            power_kw: 0.0,
            mode: PlannedMode::Resting,
            end,
        };
        let confirmed = self
            .state
            .confirmed_at
            .is_some_and(|at| at <= now && now - at <= FEED_GRACE);
        let telemetry = telemetry.filter(|t| {
            fresh_telemetry(t, now)
                && [t.battery_kw, t.grid_kw]
                    .iter()
                    .all(|kw| kw.is_finite() && kw.abs() <= 1000.0)
        });
        if let Some(t) = telemetry.filter(|_| confirmed) {
            self.state.limited |= match event.direction {
                AxleDirection::Export => t.soc_pct <= cfg.soc_floor_pct,
                AxleDirection::Import => t.soc_pct >= cfg.soc_ceiling_pct,
            };
            if self.state.limited && !before.limited {
                log::info!(target: "kwc.axle", "self-dispatch battery limit reached at {:.1}% SOC; passive for remainder of event", t.soc_pct);
            }
            if !self.state.limited {
                let hours = (end - now).num_milliseconds() as f64 / 3_600_000.0;
                let stored = cfg.usable_capacity_kwh * t.soc_pct / 100.0;
                let ttl = (end - now).to_std().unwrap_or_default();
                let (power, mode) = match event.direction {
                    AxleDirection::Export => (
                        cfg.max_export_kw.min(cfg.max_discharge_kw).min(
                            (stored - cfg.floor_kwh()).max(0.0) * cfg.round_trip_efficiency.sqrt()
                                / hours,
                        ),
                        PlannedMode::Exporting,
                    ),
                    AxleDirection::Import => (
                        cfg.max_charge_kw.min(
                            (cfg.ceiling_kwh() - stored).max(0.0)
                                / cfg.round_trip_efficiency.sqrt()
                                / hours,
                        ),
                        PlannedMode::Charging,
                    ),
                };
                // Native inverter leases require at least one second.
                if ttl >= std::time::Duration::from_secs(1) && power.is_finite() && power > 0.0 {
                    decision.mode = mode;
                    decision.power_kw = power;
                }
            }
        }
        if self.state != before {
            self.save()?;
        }
        Ok(Some(decision))
    }

    pub fn record_attempt(&mut self, now: DateTime<Utc>, failed: bool) {
        self.last_attempt = Some(now);
        self.write_failed = failed;
    }

    pub fn retry_write_allowed(&self, now: DateTime<Utc>) -> bool {
        !self.write_failed
            || self
                .last_attempt
                .is_none_or(|at| now - at >= Duration::seconds(30))
    }

    /// Check the meter rather than treating an accepted command as delivery.
    /// Retry at most once per 30 seconds; normal minute lease refresh continues.
    pub fn retry_due(&mut self, now: DateTime<Utc>, telemetry: Option<&Telemetry>) -> bool {
        if !self.active(now) || self.state.limited {
            return false;
        }
        let Some(t) = telemetry.filter(|t| fresh_telemetry(t, now)) else {
            return false;
        };
        let delivered = match self.event().expect("active").direction {
            AxleDirection::Export => t.grid_kw < -0.05,
            AxleDirection::Import => t.battery_kw > 0.05,
        };
        if delivered {
            if !self.verified_flow {
                log::info!(target: "kwc.axle", "self-dispatch flow verified: grid {:.3} kW, battery {:.3} kW", t.grid_kw, t.battery_kw);
                self.verified_flow = true;
            }
            return false;
        }
        if self
            .last_attempt
            .is_some_and(|at| now - at >= Duration::seconds(30))
        {
            log::warn!(target: "kwc.axle", "self-dispatch flow not measured: grid {:.3} kW, battery {:.3} kW; retrying within event limits", t.grid_kw, t.battery_kw);
            return true;
        }
        false
    }

    fn save(&self) -> Result<(), String> {
        let parent = self.path.parent().ok_or("Axle state path has no parent")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec(&self.state).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        std::fs::rename(tmp, &self.path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 11, 17, 7, 0).unwrap()
    }
    fn event() -> AxleEvent {
        AxleEvent {
            start_time: start(),
            end_time: start() + Duration::hours(1),
            direction: AxleDirection::Export,
        }
    }
    fn config() -> BatteryConfig {
        BatteryConfig {
            usable_capacity_kwh: 10.0,
            max_charge_kw: 4.0,
            max_export_kw: 5.0,
            max_discharge_kw: 4.5,
            soc_floor_pct: 10.0,
            soc_ceiling_pct: 90.0,
            round_trip_efficiency: 0.81,
        }
    }
    fn telemetry(now: DateTime<Utc>, soc: f64) -> Telemetry {
        Telemetry {
            soc_pct: soc,
            battery_kw: 0.0,
            grid_kw: 0.3,
            load_kw: 0.3,
            solar_kw: 0.0,
            at: now,
            read_at: std::time::Instant::now(),
        }
    }
    struct Fixture {
        dir: PathBuf,
        dispatch: AxleSelfDispatch,
    }
    impl Fixture {
        fn new() -> Self {
            static ID: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "kwc-self-dispatch-{}-{}",
                std::process::id(),
                ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let dispatch = AxleSelfDispatch::load(dir.join("state.json")).unwrap();
            Self { dir, dispatch }
        }
        fn reload(&mut self) {
            self.dispatch = AxleSelfDispatch::load(self.dir.join("state.json")).unwrap();
        }
        fn decide(&mut self, at: DateTime<Utc>, soc: f64) -> Option<ModeDecision> {
            self.dispatch
                .decision(&config(), at, Some(&telemetry(at, soc)))
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn export_starts_at_exact_event_boundary_and_stops_at_end_without_handover_buffers() {
        let mut f = Fixture::new();
        let e = event();
        f.dispatch
            .observe(Some(&e), start() - Duration::minutes(1))
            .unwrap();
        assert!(f
            .decide(start() - Duration::milliseconds(1), 80.0)
            .is_none());
        let d = f.decide(start(), 80.0).unwrap();
        assert_eq!(d.mode, PlannedMode::Exporting);
        assert_eq!(d.power_kw, 4.5);
        assert!(d.end <= start() + Duration::minutes(2));
        let at = e.end_time - Duration::milliseconds(500);
        f.dispatch.observe(Some(&e), at).unwrap();
        let d = f.decide(at, 80.0).unwrap();
        assert_eq!(d.end, e.end_time);
        assert_eq!(d.mode, PlannedMode::Resting);
        assert_eq!(d.power_kw, 0.0);
        assert!(f.decide(e.end_time, 80.0).is_none());
        assert!(f.dispatch.finish_if_due(e.end_time).unwrap());
        assert!(!f.dispatch.finish_if_due(e.end_time).unwrap());
    }

    #[test]
    fn persisted_event_and_safety_latch_survive_restart() {
        let mut f = Fixture::new();
        f.dispatch.observe(Some(&event()), start()).unwrap();
        assert_eq!(
            f.decide(start(), 80.0).unwrap().mode,
            PlannedMode::Exporting
        );
        f.reload();
        assert_eq!(
            f.decide(start() + Duration::seconds(30), 80.0)
                .unwrap()
                .mode,
            PlannedMode::Exporting
        );
        assert_eq!(
            f.decide(start() + Duration::seconds(30), 10.0)
                .unwrap()
                .mode,
            PlannedMode::Resting
        );
        f.reload();
        assert_eq!(
            f.decide(start() + Duration::minutes(1), 11.0).unwrap().mode,
            PlannedMode::Resting
        );
        f.reload();
        assert!(f.dispatch.finish_if_due(event().end_time).unwrap());
    }

    #[test]
    fn leases_bound_near_floor_energy_and_import_room() {
        let mut f = Fixture::new();
        f.dispatch.observe(Some(&event()), start()).unwrap();
        let d = f.decide(start(), 10.1).unwrap();
        let energy = d.power_kw * (d.end - start()).num_milliseconds() as f64 / 1000.0 / 3600.0;
        assert!(energy <= 0.009 + 1e-10);
        let mut e = event();
        e.direction = AxleDirection::Import;
        f.dispatch.observe(Some(&e), start()).unwrap();
        let d = f.decide(start(), 89.9).unwrap();
        assert_eq!(d.mode, PlannedMode::Charging);
        assert!(d.power_kw <= 4.0);
        let stored =
            d.power_kw * (d.end - start()).num_milliseconds() as f64 / 1000.0 / 3600.0 * 0.9;
        assert!(stored <= 0.01 + 1e-10);
        assert_eq!(f.decide(start(), 90.0).unwrap().mode, PlannedMode::Resting);
    }

    #[test]
    fn missing_or_stale_telemetry_stops_then_fresh_telemetry_can_resume() {
        let mut f = Fixture::new();
        f.dispatch.observe(Some(&event()), start()).unwrap();
        assert_eq!(
            f.dispatch
                .decision(&config(), start(), None)
                .unwrap()
                .unwrap()
                .mode,
            PlannedMode::Resting
        );
        let old = telemetry(start() - Duration::seconds(6), 80.0);
        assert_eq!(
            f.dispatch
                .decision(&config(), start(), Some(&old))
                .unwrap()
                .unwrap()
                .mode,
            PlannedMode::Resting
        );
        let mut invalid = telemetry(start(), 80.0);
        invalid.grid_kw = f64::NAN;
        assert_eq!(
            f.dispatch
                .decision(&config(), start(), Some(&invalid))
                .unwrap()
                .unwrap()
                .mode,
            PlannedMode::Resting
        );
        assert_eq!(
            f.decide(start(), 80.0).unwrap().mode,
            PlannedMode::Exporting
        );
    }

    #[test]
    fn api_failure_grace_never_extends_event_and_expired_confirmation_stays_passive() {
        let mut f = Fixture::new();
        f.dispatch.observe(Some(&event()), start()).unwrap();
        f.reload();
        assert_eq!(
            f.decide(start() + Duration::minutes(4), 80.0).unwrap().mode,
            PlannedMode::Exporting
        );
        assert_eq!(
            f.decide(start() + Duration::minutes(6), 80.0).unwrap().mode,
            PlannedMode::Resting
        );
        assert!(f.decide(event().end_time, 80.0).is_none());
        f.dispatch
            .observe(Some(&event()), start() + Duration::minutes(7))
            .unwrap();
        assert_eq!(
            f.decide(start() + Duration::minutes(7), 80.0).unwrap().mode,
            PlannedMode::Exporting
        );
    }

    #[test]
    fn successful_confirmation_is_persisted_periodically() {
        let mut f = Fixture::new();
        f.dispatch.observe(Some(&event()), start()).unwrap();
        for i in 1..=10 {
            f.dispatch
                .observe(Some(&event()), start() + Duration::seconds(i * 30))
                .unwrap();
        }
        f.reload();
        assert_eq!(
            f.decide(start() + Duration::minutes(8), 80.0).unwrap().mode,
            PlannedMode::Exporting
        );
    }

    #[test]
    fn cancellation_replacement_and_shortened_event_release_control_immediately() {
        for replacement in [
            None,
            Some(AxleEvent {
                start_time: start() + Duration::hours(2),
                end_time: start() + Duration::hours(3),
                direction: AxleDirection::Export,
            }),
            Some(AxleEvent {
                end_time: start() + Duration::minutes(1),
                ..event()
            }),
        ] {
            let mut f = Fixture::new();
            f.dispatch.observe(Some(&event()), start()).unwrap();
            f.decide(start(), 80.0);
            let at = start() + Duration::minutes(2);
            f.dispatch.observe(replacement.as_ref(), at).unwrap();
            assert!(f.decide(at, 80.0).is_none());
            assert!(f.dispatch.finish_if_due(at).unwrap());
        }
    }

    #[test]
    fn detects_real_grid_export_and_throttles_failed_command_retries() {
        let mut f = Fixture::new();
        f.dispatch.observe(Some(&event()), start()).unwrap();
        f.decide(start(), 80.0);
        f.dispatch.record_attempt(start(), false);
        assert!(!f.dispatch.retry_due(
            start() + Duration::seconds(29),
            Some(&telemetry(start() + Duration::seconds(29), 80.0))
        ));
        let now = start() + Duration::seconds(30);
        assert!(f.dispatch.retry_due(now, Some(&telemetry(now, 80.0))));
        let mut t = telemetry(now, 80.0);
        t.grid_kw = -4.2;
        t.battery_kw = -4.5;
        assert!(!f.dispatch.retry_due(now, Some(&t)));
        f.dispatch.record_attempt(now, true);
        assert!(!f.dispatch.retry_write_allowed(now + Duration::seconds(29)));
        assert!(f.dispatch.retry_write_allowed(now + Duration::seconds(30)));
    }

    #[test]
    fn corrupt_or_unwritable_state_cannot_authorize_dispatch() {
        let mut f = Fixture::new();
        std::fs::write(f.dir.join("state.json"), "broken").unwrap();
        assert!(AxleSelfDispatch::load(f.dir.join("state.json")).is_err());
        std::fs::create_dir(f.dir.join("state.json.tmp")).unwrap();
        assert!(f.dispatch.observe(Some(&event()), start()).is_err());
    }
}
