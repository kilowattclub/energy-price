//! Direct access to Axle's current event. There is deliberately no cache.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::AxleConfig;

const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum AxleError {
    #[error("Axle events unavailable: {0}")]
    Unavailable(String),
    #[error("Axle request failed: {0}")]
    Http(String),
}

pub use crate::events::{AxleDirection, AxleEvent, AxleForecast};

pub trait AxleSource: Send + Sync {
    fn fetch(&self, url: &str, api_key: &str) -> Result<Option<AxleEvent>, AxleError>;
}

struct HttpAxleSource {
    agent: ureq::Agent,
}

impl HttpAxleSource {
    fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct EventResponse {
    #[serde(default)]
    opted_out: bool,
    #[serde(default)]
    start_time: Option<DateTime<Utc>>,
    #[serde(default)]
    end_time: Option<DateTime<Utc>>,
    #[serde(default)]
    import_export: Option<AxleDirection>,
}

impl EventResponse {
    fn into_event(self) -> Result<Option<AxleEvent>, AxleError> {
        if self.opted_out {
            return Ok(None);
        }
        match (self.start_time, self.end_time, self.import_export) {
            (None, None, None) => Ok(None),
            (Some(start_time), Some(end_time), Some(direction))
                if end_time > start_time
                    && end_time - start_time <= chrono::Duration::hours(24) =>
            {
                Ok(Some(AxleEvent {
                    start_time,
                    end_time,
                    direction,
                }))
            }
            _ => Err(AxleError::Unavailable(
                "expected an empty event or ordered UTC start/end times with a direction".into(),
            )),
        }
    }
}

impl AxleSource for HttpAxleSource {
    fn fetch(&self, url: &str, api_key: &str) -> Result<Option<AxleEvent>, AxleError> {
        let response: Option<EventResponse> = self
            .agent
            .get(url)
            .set("Authorization", &format!("Bearer {api_key}"))
            .call()
            .map_err(|error| AxleError::Http(error.to_string()))?
            .into_json()
            .map_err(|error| AxleError::Http(error.to_string()))?;
        response
            .map(EventResponse::into_event)
            .transpose()
            .map(Option::flatten)
    }
}

pub struct AxleEvents {
    cfg: AxleConfig,
    source: Box<dyn AxleSource>,
}

impl AxleEvents {
    /// Supply another provider or an offline source through the same API.
    pub fn with_source(cfg: AxleConfig, source: Box<dyn AxleSource>) -> Self {
        Self { cfg, source }
    }

    pub fn new(cfg: AxleConfig) -> Self {
        Self {
            cfg,
            source: Box::new(HttpAxleSource::new()),
        }
    }

    /// Called directly on every controller tick.
    pub fn get_event(&self) -> Result<Option<AxleEvent>, AxleError> {
        if !self.cfg.enabled {
            return Ok(None);
        }
        let url = format!(
            "{}/vpp/home-assistant/event",
            self.cfg.api_url.trim_end_matches('/')
        );
        self.source.fetch(&url, &self.cfg.api_key)
    }
}

#[cfg(test)]
#[path = "../tests/unit/events.rs"]
mod tests;
