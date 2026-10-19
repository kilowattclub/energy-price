//! Provider-owned event feed, forecasts and reward accounting. The host owns
//! hardware, clocks and control: a dispatch event is an export like any other,
//! priced by the planner, so there is no handover and no provider override.
mod config;
mod rewards;
mod store;

use crate::{axle::AxleEvents, AxleConfig, DispatchDirection, DispatchEvent, DispatchForecast};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
pub use config::ProviderConfig;
use rewards::AxleRewardTracker;
use std::path::Path;
use store::save_json;

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

#[derive(Debug, Clone, PartialEq)]
pub struct EventInfo {
    pub id: Option<String>,
    pub provider: String,
    pub direction: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Update {
    pub changed: bool,
    /// The known events changed, so the host should ask its planner again.
    pub replan: bool,
}

/// Host-facing contract. No inverter driver or optimiser dependency is required.
pub trait DispatchProvider {
    fn poll(&mut self, _now: DateTime<Utc>) -> Result<Update, String> {
        Ok(Update::default())
    }
    /// Known events with their reward assumptions, for the planner.
    fn forecasts(&self) -> Vec<DispatchForecast> {
        vec![]
    }
    fn active_event(&self) -> Option<EventInfo> {
        None
    }
    /// The next event start or end after `now`: the host plans again there.
    fn next_boundary(&self, _now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        None
    }
    fn observe(&mut self, _now: DateTime<Utc>, _reading: Option<&Reading>) {}
    fn flush(&mut self) {}
}

pub struct DispatchService {
    cfg: AxleConfig,
    feed: AxleEvents,
    last: Option<DispatchEvent>,
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
            rewards,
        })
    }
}
impl DispatchProvider for DispatchService {
    fn poll(&mut self, now: DateTime<Utc>) -> Result<Update, String> {
        let previous = self.last.clone();
        match self.feed.get_event() {
            Ok(event) => {
                if let Some(rewards) = &mut self.rewards {
                    rewards.confirm(event.as_ref(), now, &self.cfg);
                }
                self.last = event;
            }
            Err(error) => {
                log::warn!("{error}");
                // A feed outage keeps the last known event until it ends.
                if self.last.as_ref().is_some_and(|e| e.end_time <= now) {
                    self.last = None;
                }
            }
        }
        let changed = self.last != previous;
        Ok(Update {
            changed,
            replan: changed,
        })
    }
    fn forecasts(&self) -> Vec<DispatchForecast> {
        self.last
            .as_ref()
            .map(|event| DispatchForecast {
                event: event.clone(),
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
        fn new() -> Self {
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
        fn outage(&mut self, now: DateTime<Utc>) -> Update {
            *self.feed.0.lock().unwrap() = Err("offline".into());
            self.service.poll(now).unwrap()
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
    fn event_changes_ask_for_a_replan_and_boundaries_follow_the_event() {
        let mut f = Fixture::new();
        assert_eq!(f.publish(None, at(0)), Update::default());
        assert!(f.service.forecasts().is_empty());
        assert_eq!(f.service.next_boundary(at(0)), None);
        let update = f.publish(Some(event()), at(16));
        assert!(update.changed && update.replan);
        assert_eq!(f.service.forecasts()[0].event, event());
        assert_eq!(f.service.next_boundary(at(16)), Some(at(17)));
        assert_eq!(f.service.next_boundary(at(17)), Some(at(27)));
        assert_eq!(f.service.next_boundary(at(27)), None);
        assert_eq!(f.publish(Some(event()), at(18)), Update::default());
        let cleared = f.publish(None, at(23));
        assert!(cleared.changed && cleared.replan);
        assert!(f.service.forecasts().is_empty());
    }
    #[test]
    fn a_feed_outage_keeps_the_known_event_until_it_ends() {
        let mut f = Fixture::new();
        f.publish(Some(event()), at(16));
        assert_eq!(f.outage(at(20)), Update::default());
        assert_eq!(f.service.forecasts()[0].event, event());
        assert_eq!(f.service.next_boundary(at(20)), Some(at(27)));
        let ended = f.outage(at(27));
        assert!(ended.changed && ended.replan);
        assert!(f.service.forecasts().is_empty());
        assert!(f.service.active_event().is_none());
    }
    #[test]
    fn provider_forecasts_metadata_and_legacy_reward_snapshots_stay_consistent() {
        let mut f = Fixture::new();
        f.publish(Some(event()), at(16));
        let forecasts = f.service.forecasts();
        assert_eq!(forecasts[0].event, event());
        assert_eq!(forecasts[0].reward_p_per_kwh, 100.0);
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
