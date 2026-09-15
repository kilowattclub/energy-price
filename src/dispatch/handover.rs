//! Persistent protection against inverter writes around Axle dispatches.
use std::path::PathBuf;

use super::{handover_at, resume_at};
use crate::AxleEvent;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct Window {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    passive_attempted: bool,
}

impl Window {
    fn contains(&self, now: DateTime<Utc>) -> bool {
        handover_at(self.start) <= now && now < resume_at(self.end)
    }
}

pub struct AxleHandover {
    path: PathBuf,
    windows: Vec<Window>,
}

impl AxleHandover {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let windows: Vec<Window> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.to_string()),
        };
        if windows.iter().any(|w| w.start >= w.end) {
            return Err("invalid saved Axle protection window".into());
        }
        Ok(Self { path, windows })
    }

    // Keep accepted windows through their grace period even if the API clears
    // the completed event, fails, or publishes a different upcoming event.
    pub fn observe(&mut self, event: Option<&AxleEvent>, now: DateTime<Utc>) -> Result<(), String> {
        let before = self.windows.clone();
        self.windows.retain(|w| now < resume_at(w.end));
        if let Some(event) =
            event.filter(|e| e.start_time < e.end_time && now < resume_at(e.end_time))
        {
            if let Some(window) = self
                .windows
                .iter_mut()
                .find(|w| w.start == event.start_time)
            {
                window.end = window.end.max(event.end_time);
            } else {
                self.windows.push(Window {
                    start: event.start_time,
                    end: event.end_time,
                    passive_attempted: false,
                });
            }
        }
        if self.windows != before {
            self.save()?;
        }
        Ok(())
    }

    pub fn protected(&self, now: DateTime<Utc>) -> bool {
        self.windows.iter().any(|w| w.contains(now))
    }

    pub fn next_handover(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.windows
            .iter()
            .map(|w| handover_at(w.start))
            .filter(|start| *start > now)
            .min()
    }

    /// None allows ordinary control. Some(true) requires one passive handover;
    /// Some(false) forbids writes. Persist before returning any permission.
    pub fn prepare(&mut self, now: DateTime<Utc>) -> Result<Option<bool>, String> {
        if !self.protected(now) {
            return Ok(None);
        }
        // Never reset an active event or its grace period, including when a
        // second event's preparation overlaps it. Never repeat a handover.
        let silent = self.windows.iter().any(|w| {
            w.contains(now)
                && (now >= w.start
                    || now >= handover_at(w.start) + chrono::Duration::minutes(1)
                    || w.passive_attempted)
        });
        let mut changed = false;
        for window in self.windows.iter_mut().filter(|w| w.contains(now)) {
            changed |= !window.passive_attempted;
            window.passive_attempted = true;
        }
        // Persist BEFORE attempting the write: a crash or partial write must
        // not cause repeated resets on restart. Failure leaves control blocked.
        if changed {
            self.save()?;
        }
        Ok(Some(!silent))
    }

    fn save(&self) -> Result<(), String> {
        let parent = self.path.parent().ok_or("Axle state path has no parent")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(
            &temp,
            serde_json::to_vec(&self.windows).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        std::fs::rename(temp, &self.path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AxleDirection;
    use chrono::{Duration, TimeZone};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct Spy {
        commands: Vec<&'static str>,
        fail: bool,
    }
    // Exercise the caller contract, including an error after permission was persisted.
    impl AxleHandover {
        fn apply(
            &mut self,
            now: DateTime<Utc>,
            spy: &mut Spy,
            observe_only: bool,
        ) -> Result<bool, String> {
            let Some(release) = self.prepare(now)? else {
                return Ok(false);
            };
            if release && !observe_only {
                spy.commands.push("passive");
                if spy.fail {
                    return Err("partial write".into());
                }
            }
            Ok(true)
        }
    }

    struct Fixture {
        dir: PathBuf,
        handover: AxleHandover,
        event: AxleEvent,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "kwc-axle-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let event = AxleEvent {
                start_time: Utc.with_ymd_and_hms(2026, 9, 7, 18, 0, 0).unwrap(),
                end_time: Utc.with_ymd_and_hms(2026, 9, 7, 19, 0, 0).unwrap(),
                direction: AxleDirection::Export,
            };
            let mut handover = AxleHandover::load(dir.join("axle.json")).unwrap();
            handover
                .observe(Some(&event), event.start_time - Duration::hours(1))
                .unwrap();
            Self {
                dir,
                handover,
                event,
            }
        }
        fn reload(&mut self) {
            self.handover = AxleHandover::load(self.dir.join("axle.json")).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn one_passive_at_prior_boundary_then_silence_through_buffered_resume() {
        for direction in [AxleDirection::Import, AxleDirection::Export] {
            let mut f = Fixture::new();
            f.event.direction = direction;
            let mut spy = Spy::default();
            let start = f.event.start_time;
            assert!(!f
                .handover
                .apply(handover_at(start) - Duration::seconds(1), &mut spy, false)
                .unwrap());
            assert!(spy.commands.is_empty());
            assert_eq!(
                f.handover
                    .next_handover(handover_at(start) - Duration::seconds(1)),
                Some(handover_at(start))
            );
            // Every 30-second tick through the event and post-event grace period.
            for seconds in (-1800..5400).step_by(30) {
                let now = start + Duration::seconds(seconds);
                f.handover.observe(None, now).unwrap(); // API may clear its event.
                assert!(f.handover.apply(now, &mut spy, false).unwrap());
                assert!(f.handover.protected(now)); // Shutdown must also be silent.
                assert_eq!(spy.commands, vec!["passive"]);
                f.reload(); // Persistence must prevent repeat handover on restart.
            }
            assert!(!f
                .handover
                .apply(resume_at(f.event.end_time), &mut spy, false)
                .unwrap());
            assert!(!f.handover.protected(resume_at(f.event.end_time)));
        }
    }

    #[test]
    fn late_discovery_and_restart_never_send_passive_during_event_or_grace() {
        for seconds in [-1740, -900, 0, 60, 3599, 3600, 5399] {
            let mut f = Fixture::new();
            let mut spy = Spy::default();
            let now = f.event.start_time + Duration::seconds(seconds);
            assert!(f.handover.apply(now, &mut spy, false).unwrap());
            f.reload();
            assert!(f.handover.apply(now, &mut spy, false).unwrap());
            assert!(spy.commands.is_empty());
        }
    }

    #[test]
    fn events_starting_in_the_handover_minute_are_already_protected() {
        for seconds in [1, 30, 59] {
            let mut f = Fixture::new();
            f.event.start_time += Duration::seconds(seconds);
            let now = f.event.start_time;
            // Discover an event whose start falls inside the one-minute handover allowance.
            f.handover = AxleHandover::load(f.dir.join("non-aligned.json")).unwrap();
            f.handover.observe(Some(&f.event), now).unwrap();
            let mut spy = Spy::default();
            assert!(f.handover.apply(now, &mut spy, false).unwrap());
            assert!(f.handover.protected(now));
            assert!(spy.commands.is_empty(), "start offset: {seconds}s");
            f.handover = AxleHandover::load(f.dir.join("non-aligned.json")).unwrap();
            assert!(f
                .handover
                .apply(now + Duration::seconds(1), &mut spy, false)
                .unwrap());
            assert!(spy.commands.is_empty());
        }
    }

    #[test]
    fn replacement_event_does_not_remove_previous_grace_period() {
        let mut f = Fixture::new();
        let mut spy = Spy::default();
        let mut next = f.event.clone();
        next.start_time = f.event.end_time + Duration::minutes(3);
        next.end_time = next.start_time + Duration::minutes(30);
        let now = f.event.end_time;
        f.handover.observe(Some(&next), now).unwrap();
        assert!(f.handover.apply(now, &mut spy, false).unwrap());
        assert!(f.handover.apply(next.start_time, &mut spy, false).unwrap());
        assert!(spy.commands.is_empty());
        assert!(!f
            .handover
            .apply(resume_at(next.end_time), &mut spy, false)
            .unwrap());
    }

    #[test]
    fn failed_passive_is_not_retried_and_observe_only_never_writes() {
        let mut f = Fixture::new();
        let mut spy = Spy {
            fail: true,
            ..Spy::default()
        };
        let now = handover_at(f.event.start_time);
        assert!(f.handover.apply(now, &mut spy, false).is_err());
        f.reload();
        assert!(f
            .handover
            .apply(now + Duration::seconds(30), &mut spy, false)
            .unwrap());
        assert_eq!(spy.commands.len(), 1);
        let mut f = Fixture::new();
        let mut spy = Spy::default();
        assert!(f.handover.apply(now, &mut spy, true).unwrap());
        assert!(spy.commands.is_empty());
    }

    #[test]
    fn corrupt_state_and_failed_persistence_cannot_authorize_a_write() {
        let mut f = Fixture::new();
        let mut spy = Spy::default();
        std::fs::write(f.dir.join("axle.json"), "not json").unwrap();
        assert!(AxleHandover::load(f.dir.join("axle.json")).is_err());
        std::fs::create_dir(f.dir.join("axle.json.tmp")).unwrap();
        let now = handover_at(f.event.start_time);
        assert!(f.handover.apply(now, &mut spy, false).is_err());
        assert!(spy.commands.is_empty());
        assert!(f.handover.protected(now));
    }
}
