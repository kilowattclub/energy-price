//! Published Octopus export prices.
use crate::MarketConfig;
use chrono::{DateTime, Utc};
const API_BASE: &str = "https://api.octopus.energy/v1";

#[derive(Debug, thiserror::Error)]
pub enum ExportPriceError {
    #[error("Octopus export prices unavailable: {0}")]
    Unavailable(String),
    #[error("Octopus export request failed: {0}")]
    Http(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportPriceSlot {
    pub start: DateTime<Utc>,
    pub price_p_per_kwh: f64,
}

pub trait ExportPriceSource: Send + Sync {
    fn fetch(
        &self,
        product: &str,
        tariff: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ExportPriceSlot>, ExportPriceError>;
}

struct HttpExportPriceSource {
    agent: ureq::Agent,
}

impl HttpExportPriceSource {
    fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(5))
                .build(),
        }
    }
}

impl ExportPriceSource for HttpExportPriceSource {
    fn fetch(
        &self,
        product: &str,
        tariff: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ExportPriceSlot>, ExportPriceError> {
        let timestamp = |time: DateTime<Utc>| time.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let url = format!(
            "{API_BASE}/products/{product}/electricity-tariffs/{tariff}/standard-unit-rates/?period_from={}&period_to={}&page_size=100",
            timestamp(from),
            timestamp(to),
        );
        let payload: serde_json::Value = self
            .agent
            .get(&url)
            .call()
            .map_err(|error| ExportPriceError::Http(error.to_string()))?
            .into_json()
            .map_err(|error| ExportPriceError::Http(error.to_string()))?;
        Ok(payload
            .get("results")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let start = DateTime::parse_from_rfc3339(row.get("valid_from")?.as_str()?)
                    .ok()?
                    .with_timezone(&Utc);
                let price_p_per_kwh = row.get("value_inc_vat")?.as_f64()?;
                price_p_per_kwh.is_finite().then_some(ExportPriceSlot {
                    start,
                    price_p_per_kwh,
                })
            })
            .collect())
    }
}

pub struct OctopusExports {
    cfg: MarketConfig,
    source: Box<dyn ExportPriceSource>,
}

impl OctopusExports {
    /// Supply another provider or an offline source through the same API.
    pub fn with_source(cfg: MarketConfig, source: Box<dyn ExportPriceSource>) -> Self {
        Self { cfg, source }
    }

    pub fn new(cfg: MarketConfig) -> Self {
        Self {
            cfg,
            source: Box::new(HttpExportPriceSource::new()),
        }
    }

    pub fn get_slots(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ExportPriceSlot>, ExportPriceError> {
        if self.cfg.export_product.is_empty() || self.cfg.export_tariff.is_empty() {
            return Err(ExportPriceError::Unavailable(
                "export product and tariff are not configured".into(),
            ));
        }
        let mut slots = self.source.fetch(
            &self.cfg.export_product,
            &self.cfg.export_tariff,
            start,
            end,
        )?;
        slots.sort_by_key(|slot| slot.start);
        slots.dedup_by_key(|slot| slot.start);
        slots.retain(|slot| start <= slot.start && slot.start < end);
        if slots.is_empty() {
            return Err(ExportPriceError::Unavailable(format!(
                "no usable slots from {start}"
            )));
        }
        Ok(slots)
    }
}
