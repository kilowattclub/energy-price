//! Provider-owned event policy, persistence and rewards. The host owns hardware and clocks.
mod config;
mod handover;
mod rewards;
mod self_dispatch;
mod store;
mod time;

use crate::{axle::AxleEvents, AxleConfig, DispatchDirection, DispatchEvent, DispatchForecast};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
pub use config::ProviderConfig;
use handover::AxleHandover;
use rewards::AxleRewardTracker;
use self_dispatch::AxleSelfDispatch;
use std::path::Path;
use store::save_json;
use time::{handover_at, resume_at};

/// An observation supplied by the host. Grid and battery power are positive on import/charge.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    pub at: DateTime<Utc>,
    pub read_at: std::time::Instant,
    pub soc_pct: f64,
    pub load_kw: f64,
    pub solar_kw: f64,
    pub grid_kw: f64,
    pub battery_kw: f64,
}
fn fresh_reading(t: &Reading, now: DateTime<Utc>) -> bool {
    t.at <= now
        && now - t.at <= Duration::seconds(5)
        && t.read_at.elapsed() <= std::time::Duration::from_secs(5)
        && t.soc_pct.is_finite()
        && (0.0..=100.0).contains(&t.soc_pct)
        && [t.load_kw, t.solar_kw]
            .iter()
            .all(|v| v.is_finite() && (0.0..=1000.0).contains(v))
}

/// Electrical limits supplied by the hardware-owning application.
#[derive(Clone, Copy, Debug)]
pub struct BatteryLimits {
    pub usable_capacity_kwh: f64,
    pub max_charge_kw: f64,
    pub max_discharge_kw: f64,
    pub max_export_kw: f64,
    pub soc_floor_pct: f64,
    pub soc_ceiling_pct: f64,
    pub round_trip_efficiency: f64,
}
impl BatteryLimits {
    fn floor_kwh(&self) -> f64 {
        self.usable_capacity_kwh * self.soc_floor_pct / 100.0
    }
    fn ceiling_kwh(&self) -> f64 {
        self.usable_capacity_kwh * self.soc_ceiling_pct / 100.0
    }
    fn valid(&self) -> bool {
        [
            self.usable_capacity_kwh,
            self.max_charge_kw,
            self.max_discharge_kw,
            self.round_trip_efficiency,
        ]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0)
            && self.round_trip_efficiency <= 1.0
            && self.max_export_kw.is_finite()
            && self.max_export_kw >= 0.0
            && self.soc_floor_pct.is_finite()
            && self.soc_ceiling_pct.is_finite()
            && 0.0 <= self.soc_floor_pct
            && self.soc_floor_pct < self.soc_ceiling_pct
            && self.soc_ceiling_pct <= 100.0
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    Charging,
    Exporting,
    Resting,
}
/// A request for the host to translate into its own inverter command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Action {
    pub mode: DispatchMode,
    pub power_kw: f64,
    pub end: DateTime<Utc>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct EventInfo {
    pub id: Option<String>,
    pub provider: String,
    pub direction: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}
#[derive(Debug, Default, Clone, Copy)]
pub struct Update {
    pub changed: bool,
    pub replan: bool,
}
#[derive(Debug, Clone, Copy)]
pub enum Control {
    /// The feed has not yet established whether another party owns control.
    Unavailable,
    /// Suppress hardware writes except for the single permitted passive handover.
    External { release: bool, until: DateTime<Utc> },
    /// A provider request; the host still applies the action and reports write outcomes.
    Override {
        action: Action,
        retry_due: bool,
        retry_allowed: bool,
        next_refresh: DateTime<Utc>,
        retry_at: DateTime<Utc>,
    },
    /// Ordinary tariff control is permitted until the optional deadline.
    Tariff {
        until: Option<DateTime<Utc>>,
        replan: bool,
    },
}

/// Host-facing contract. No inverter driver or optimiser dependency is required.
pub trait DispatchProvider {
    fn poll(&mut self, _now: DateTime<Utc>) -> Result<Update, String> {
        Ok(Update::default())
    }
    fn control(
        &mut self,
        limits: &BatteryLimits,
        now: DateTime<Utc>,
        reading: Option<&Reading>,
    ) -> Result<Control, String>;
    fn forecasts(&self) -> Vec<DispatchForecast> {
        vec![]
    }
    fn active_event(&self) -> Option<EventInfo> {
        None
    }
    fn next_boundary(&self, _now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        None
    }
    /// True means the provider owns retry policy for this attempted write.
    fn record_attempt(&mut self, _now: DateTime<Utc>, _failed: bool) -> bool {
        false
    }
    fn permits_shutdown(&self, _now: DateTime<Utc>) -> bool {
        true
    }
    fn observe(&mut self, _now: DateTime<Utc>, _reading: Option<&Reading>) {}
    fn flush(&mut self) {}
}

pub struct DispatchService {
    cfg: AxleConfig,
    feed: AxleEvents,
    last: Option<DispatchEvent>,
    ready: bool,
    handover: AxleHandover,
    local: Option<AxleSelfDispatch>,
    rewards: Option<AxleRewardTracker>,
}
impl DispatchService {
    pub fn new(
        cfg: &ProviderConfig,
        directory: &Path,
        timezone: Tz,
        max_gap_seconds: f64,
    ) -> Result<Self, String> {
        Self::with_feed(
            cfg,
            directory,
            timezone,
            max_gap_seconds,
            AxleEvents::new(cfg.axle.clone()),
        )
    }
    fn with_feed(
        cfg: &ProviderConfig,
        directory: &Path,
        timezone: Tz,
        max_gap_seconds: f64,
        feed: AxleEvents,
    ) -> Result<Self, String> {
        cfg.validate()?;
        let handover = AxleHandover::load(directory.join("axle-handover.json"))?;
        let local = (cfg.axle.enabled && cfg.axle.self_dispatch)
            .then(|| AxleSelfDispatch::load(directory.join("axle-self-dispatch.json")))
            .transpose()?;
        let ready = !cfg.axle.enabled || local.is_some();
        let rewards = AxleRewardTracker::new(
            directory.join("axle-rewards.json"),
            timezone,
            max_gap_seconds,
        )
        .map_err(|error| log::warn!("reward accounting unavailable: {error}"))
        .ok();
        Ok(Self {
            cfg: cfg.axle.clone(),
            feed,
            last: None,
            ready,
            handover,
            local,
            rewards,
        })
    }
}
impl DispatchProvider for DispatchService {
    fn poll(&mut self, now: DateTime<Utc>) -> Result<Update, String> {
        let previous = self.last.clone();
        let was_ready = self.ready;
        match self.feed.get_event() {
            Ok(event) => {
                if let Some(rewards) = &mut self.rewards {
                    rewards.confirm(event.as_ref(), now, &self.cfg);
                }
                if let Some(local) = &mut self.local {
                    local.observe(event.as_ref(), now)?;
                }
                self.last = event;
                self.ready = true;
            }
            Err(error) => {
                log::warn!("{error}");
                if let Some(local) = &self.local {
                    self.last = local.event().cloned();
                }
                if self.last.as_ref().is_some_and(|e| e.end_time <= now) {
                    self.last = None;
                }
            }
        }
        if self.local.is_none() {
            self.handover.observe(self.last.as_ref(), now)?;
        }
        let changed = self.last != previous;
        Ok(Update {
            changed,
            replan: (changed && self.local.is_some()) || (!was_ready && self.ready),
        })
    }
    fn control(
        &mut self,
        limits: &BatteryLimits,
        now: DateTime<Utc>,
        reading: Option<&Reading>,
    ) -> Result<Control, String> {
        if !self.ready {
            return Ok(Control::Unavailable);
        }
        if self.local.is_none() {
            if let Some(release) = self.handover.prepare(now)? {
                return Ok(Control::External {
                    release,
                    until: next_half_hour(now),
                });
            }
        }
        if !limits.valid() {
            return Err("invalid dispatch battery limits".into());
        }
        let mut replan = false;
        if let Some(local) = &mut self.local {
            replan = local.finish_if_due(now)?;
            if let Some(action) = local.decision(limits, now, reading)? {
                let minute = DateTime::from_timestamp((now.timestamp().div_euclid(60) + 1) * 60, 0)
                    .expect("minute");
                return Ok(Control::Override {
                    action,
                    retry_due: local.retry_due(now, reading),
                    retry_allowed: local.retry_write_allowed(now),
                    next_refresh: action.end.min(minute),
                    retry_at: now + Duration::seconds(30),
                });
            }
        }
        let until = if self.local.is_some() {
            self.last
                .as_ref()
                .filter(|e| e.start_time > now)
                .map(|e| e.start_time)
        } else {
            self.handover.next_handover(now)
        };
        Ok(Control::Tariff { until, replan })
    }
    fn forecasts(&self) -> Vec<DispatchForecast> {
        self.last
            .as_ref()
            .map(|event| DispatchForecast {
                event: event.clone(),
                self_dispatch: self.cfg.self_dispatch,
                reward_p_per_kwh: match event.direction {
                    DispatchDirection::Import => self.cfg.import_reward_p_per_kwh,
                    DispatchDirection::Export => self.cfg.export_reward_p_per_kwh,
                },
            })
            .into_iter()
            .collect()
    }
    fn active_event(&self) -> Option<EventInfo> {
        self.last.as_ref().map(|event| EventInfo {
            id: Some(format!("axle-{}", event.start_time.timestamp())),
            provider: "axle".into(),
            direction: match event.direction {
                DispatchDirection::Import => "import",
                DispatchDirection::Export => "export",
            }
            .into(),
            start: event.start_time,
            end: event.end_time,
        })
    }
    fn next_boundary(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.last
            .as_ref()
            .into_iter()
            .flat_map(|e| [e.start_time, e.end_time])
            .filter(|at| *at > now)
            .min()
    }
    fn record_attempt(&mut self, now: DateTime<Utc>, failed: bool) -> bool {
        if let Some(local) = self.local.as_mut().filter(|local| local.active(now)) {
            local.record_attempt(now, failed);
            true
        } else {
            false
        }
    }
    fn permits_shutdown(&self, now: DateTime<Utc>) -> bool {
        self.local.is_some() || (self.ready && !self.handover.protected(now))
    }
    fn observe(&mut self, now: DateTime<Utc>, reading: Option<&Reading>) {
        if let Some(rewards) = &mut self.rewards {
            rewards.observe(now, reading);
        }
    }
    fn flush(&mut self) {
        if let Some(rewards) = &mut self.rewards {
            rewards.flush();
        }
    }
}
fn next_half_hour(now: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp((now.timestamp().div_euclid(1800) + 1) * 1800, 0).expect("half-hour")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::axle::{AxleError, AxleSource};
    use chrono::TimeZone;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    #[derive(Clone)]
    struct Feed(Arc<Mutex<Result<Option<DispatchEvent>, String>>>);
    impl AxleSource for Feed {
        fn fetch(&self, _: &str, _: &str) -> Result<Option<DispatchEvent>, AxleError> {
            self.0
                .lock()
                .unwrap()
                .clone()
                .map_err(AxleError::Unavailable)
        }
    }
    struct Fixture {
        directory: std::path::PathBuf,
        cfg: ProviderConfig,
        feed: Feed,
        service: DispatchService,
    }
    impl Fixture {
        fn new(self_dispatch: bool) -> Self {
            static ID: AtomicUsize = AtomicUsize::new(0);
            let directory = std::env::temp_dir().join(format!(
                "price-dispatch-{}-{}",
                std::process::id(),
                ID.fetch_add(1, Ordering::Relaxed)
            ));
            let cfg = ProviderConfig {
                axle: AxleConfig {
                    enabled: true,
                    api_key: "test-key".into(),
                    self_dispatch,
                    ..Default::default()
                },
            };
            let feed = Feed(Arc::new(Mutex::new(Ok(None))));
            let service = DispatchService::with_feed(
                &cfg,
                &directory,
                chrono_tz::Europe::London,
                90.0,
                AxleEvents::with_source(cfg.axle.clone(), Box::new(feed.clone())),
            )
            .unwrap();
            Self {
                directory,
                cfg,
                feed,
                service,
            }
        }
        fn reload(&mut self) {
            self.service = DispatchService::with_feed(
                &self.cfg,
                &self.directory,
                chrono_tz::Europe::London,
                90.0,
                AxleEvents::with_source(self.cfg.axle.clone(), Box::new(self.feed.clone())),
            )
            .unwrap();
        }
        fn publish(&mut self, event: Option<DispatchEvent>, now: DateTime<Utc>) -> Update {
            *self.feed.0.lock().unwrap() = Ok(event);
            self.service.poll(now).unwrap()
        }
        fn control(&mut self, now: DateTime<Utc>) -> Control {
            self.service
                .control(&limits(), now, Some(&reading(now)))
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
    fn at(minute: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap() + Duration::minutes(minute)
    }
    fn event() -> DispatchEvent {
        DispatchEvent {
            start_time: at(17),
            end_time: at(27),
            direction: DispatchDirection::Export,
        }
    }
    fn limits() -> BatteryLimits {
        BatteryLimits {
            usable_capacity_kwh: 5.76,
            max_charge_kw: 5.0,
            max_discharge_kw: 5.0,
            max_export_kw: 5.0,
            soc_floor_pct: 10.0,
            soc_ceiling_pct: 100.0,
            round_trip_efficiency: 0.88,
        }
    }
    fn reading(at: DateTime<Utc>) -> Reading {
        Reading {
            at,
            read_at: std::time::Instant::now(),
            soc_pct: 40.0,
            load_kw: 1.0,
            solar_kw: 0.0,
            grid_kw: -6.0,
            battery_kw: -5.0,
        }
    }

    #[test]
    fn unknown_ownership_and_late_event_discovery_never_allow_a_reset() {
        let mut f = Fixture::new(false);
        *f.feed.0.lock().unwrap() = Err("offline".into());
        f.service.poll(at(0)).unwrap();
        assert!(matches!(f.control(at(0)), Control::Unavailable));
        assert!(!f.service.permits_shutdown(at(0)));
        let event = DispatchEvent {
            start_time: at(0) + Duration::seconds(1),
            end_time: at(10),
            direction: DispatchDirection::Export,
        };
        assert!(f.publish(Some(event.clone()), event.start_time).replan);
        assert!(matches!(
            f.control(event.start_time),
            Control::External { release: false, .. }
        ));
        assert!(!f.service.permits_shutdown(event.start_time));
        f.publish(None, at(11));
        assert!(matches!(
            f.control(at(11)),
            Control::External { release: false, .. }
        ));
        assert!(matches!(f.control(at(30)), Control::Tariff { .. }));
        assert!(f.service.permits_shutdown(at(30)));
    }
    #[test]
    fn one_handover_and_its_protection_survive_restart_and_an_empty_feed() {
        let mut f = Fixture::new(false);
        f.publish(Some(event()), at(-1));
        assert!(matches!(f.control(at(-1)), Control::Tariff { until: Some(t), .. } if t == at(0)));
        assert!(matches!(
            f.control(at(0)),
            Control::External { release: true, .. }
        ));
        f.reload();
        f.publish(None, at(1));
        assert!(matches!(
            f.control(at(1)),
            Control::External { release: false, .. }
        ));
        assert!(matches!(
            f.control(at(30)),
            Control::External { release: false, .. }
        ));
        assert!(matches!(f.control(at(60)), Control::Tariff { .. }));
    }
    #[test]
    fn local_dispatch_preserves_deadlines_feed_grace_and_cancellation() {
        let mut f = Fixture::new(true);
        assert!(f.publish(Some(event()), at(16)).replan);
        assert!(matches!(f.control(at(16)), Control::Tariff { until: Some(t), .. } if t == at(17)));
        assert_eq!(f.service.next_boundary(at(16)), Some(at(17)));
        let Control::Override {
            action,
            next_refresh,
            ..
        } = f.control(at(17))
        else {
            panic!("expected override")
        };
        assert_eq!(action.mode, DispatchMode::Exporting);
        assert_eq!(action.power_kw, 5.0);
        assert_eq!(action.end, at(19));
        assert_eq!(next_refresh, at(18));
        assert!(f.service.record_attempt(at(17), false));
        *f.feed.0.lock().unwrap() = Err("offline".into());
        f.service.poll(at(22)).unwrap();
        assert!(matches!(
            f.control(at(22)),
            Control::Override {
                action: Action {
                    mode: DispatchMode::Resting,
                    ..
                },
                ..
            }
        ));
        assert!(f.publish(None, at(23)).replan);
        assert!(matches!(
            f.control(at(23)),
            Control::Tariff { replan: true, .. }
        ));
        assert!(f.service.permits_shutdown(at(23)));
    }
    #[test]
    fn provider_forecasts_metadata_and_legacy_reward_snapshots_stay_consistent() {
        let mut f = Fixture::new(false);
        f.publish(Some(event()), at(16));
        let forecasts = f.service.forecasts();
        assert_eq!(forecasts[0].event, event());
        assert_eq!(forecasts[0].reward_p_per_kwh, 100.0);
        assert!(!forecasts[0].self_dispatch);
        for time in [at(17), at(17) + Duration::seconds(30)] {
            f.service.observe(time, Some(&reading(time)));
        }
        f.service.flush();
        let info = f.service.active_event().unwrap();
        assert_eq!(info.provider, "axle");
        assert_eq!(info.id, Some(format!("axle-{}", at(17).timestamp())));
        let snapshot = f.cfg.snapshot_fields(&f.directory, at(18), Some(&info));
        assert_eq!(snapshot["axle_event"]["start"], serde_json::json!(at(17)));
        assert!(
            (snapshot["axle_rewards"][0]["estimate_gbp"]
                .as_f64()
                .unwrap()
                - 0.05)
                .abs()
                < 1e-9
        );
        assert!(snapshot["axle_rewards"][0]["estimated"].as_bool().unwrap());
        assert!(!snapshot["axle_rewards"][0]["settled"].as_bool().unwrap());
        f.reload();
        let restored = f.cfg.snapshot_fields(&f.directory, at(18), Some(&info));
        assert_eq!(snapshot, restored);
    }
}
