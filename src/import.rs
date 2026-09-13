//! Octopus import prices for one explicitly requested UTC range.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::MarketConfig;

const API_BASE: &str = "https://api.octopus.energy/v1";

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("Octopus import prices unavailable: {0}")]
    Unavailable(String),
    #[error("Octopus request failed: {0}")]
    Http(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImportSlot {
    pub start: DateTime<Utc>,
    pub price_p_per_kwh: f64,
}

pub trait ImportSource: Send + Sync {
    fn fetch(
        &self,
        product: &str,
        tariff: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ImportSlot>, ImportError>;
}

struct HttpImportSource {
    agent: ureq::Agent,
}

impl HttpImportSource {
    fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(5))
                .build(),
        }
    }
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    results: Vec<ResponseSlot>,
}

#[derive(Deserialize)]
struct ResponseSlot {
    valid_from: DateTime<Utc>,
    value_inc_vat: f64,
}

impl ImportSource for HttpImportSource {
    fn fetch(
        &self,
        product: &str,
        tariff: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ImportSlot>, ImportError> {
        let timestamp = |time: DateTime<Utc>| time.format("%Y-%m-%dT%H:%M:%SZ");
        let url = format!(
            "{API_BASE}/products/{product}/electricity-tariffs/{tariff}/standard-unit-rates/?period_from={}&period_to={}&page_size=100",
            timestamp(from),
            timestamp(to),
        );
        let response: Response = self
            .agent
            .get(&url)
            .call()
            .map_err(|error| ImportError::Http(error.to_string()))?
            .into_json()
            .map_err(|error| ImportError::Http(error.to_string()))?;
        Ok(response
            .results
            .into_iter()
            .filter(|slot| slot.value_inc_vat.is_finite())
            .map(|slot| ImportSlot {
                start: slot.valid_from,
                price_p_per_kwh: slot.value_inc_vat,
            })
            .collect())
    }
}

pub struct OctopusImports {
    cfg: MarketConfig,
    source: Box<dyn ImportSource>,
}

impl OctopusImports {
    /// Supply another provider or an offline source through the same API.
    pub fn with_source(cfg: MarketConfig, source: Box<dyn ImportSource>) -> Self {
        Self { cfg, source }
    }

    pub fn new(cfg: MarketConfig) -> Self {
        Self {
            cfg,
            source: Box::new(HttpImportSource::new()),
        }
    }

    pub fn get_slots(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ImportSlot>, ImportError> {
        let mut slots = self.source.fetch(
            &self.cfg.import_product,
            &self.cfg.import_tariff,
            start,
            end,
        )?;
        slots.sort_by_key(|slot| slot.start);
        slots.dedup_by_key(|slot| slot.start);
        slots.retain(|slot| start <= slot.start && slot.start < end);
        if slots.is_empty() {
            return Err(ImportError::Unavailable(format!(
                "no usable slots from {start} to {end}"
            )));
        }
        Ok(slots)
    }
}

#[cfg(test)]
#[path = "../tests/unit/import.rs"]
mod tests;
