//! Published Octopus export prices.
use crate::MarketConfig;
use chrono::{DateTime, Duration, Utc};
const API_BASE: &str = "https://api.octopus.energy/v1";
const SLOT_SECONDS: i64 = 1800;

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
        Ok(rate_slots(&payload, from, to))
    }
}

/// Expand each published rate over the half-hours it covers within `from..to`.
///
/// Agile Outgoing publishes one row per half-hour. Flat products such as
/// Outgoing Octopus publish one row per rate, with `valid_to: null` while the
/// rate is current, so a rate change inside the window yields both prices.
fn rate_slots(
    payload: &serde_json::Value,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<ExportPriceSlot> {
    let time = |row: &serde_json::Value, key| {
        DateTime::parse_from_rfc3339(row.get(key)?.as_str()?)
            .ok()
            .map(|time| time.with_timezone(&Utc))
    };
    payload
        .get("results")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let valid_from = time(row, "valid_from")?;
            let valid_to = match row.get("valid_to") {
                None | Some(serde_json::Value::Null) => None,
                Some(_) => Some(time(row, "valid_to")?),
            };
            let price_p_per_kwh = row
                .get("value_inc_vat")?
                .as_f64()
                .filter(|price| price.is_finite())?;
            let end = valid_to.map_or(to, |valid_to| valid_to.min(to));
            Some(
                half_hours(valid_from.max(from), end).map(move |start| ExportPriceSlot {
                    start,
                    price_p_per_kwh,
                }),
            )
        })
        .flatten()
        .collect()
}

/// UTC half-hour starts within `from..to`, beginning at the first boundary at or after `from`.
fn half_hours(from: DateTime<Utc>, to: DateTime<Utc>) -> impl Iterator<Item = DateTime<Utc>> {
    let seconds = from.timestamp() + i64::from(from.timestamp_subsec_nanos() > 0);
    let first =
        DateTime::from_timestamp(seconds + (-seconds).rem_euclid(SLOT_SECONDS), 0).unwrap_or(to);
    std::iter::successors(Some(first), |start| Some(*start + Duration::minutes(30)))
        .take_while(move |start| *start < to)
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
        let mut slots = match self.cfg.export_fixed_p {
            Some(price) if !(price.is_finite() && price >= 0.0) => {
                return Err(ExportPriceError::Unavailable(
                    "export_fixed_p must be finite and non-negative".into(),
                ));
            }
            Some(price_p_per_kwh) => half_hours(start, end)
                .map(|start| ExportPriceSlot {
                    start,
                    price_p_per_kwh,
                })
                .collect(),
            None => self.source.fetch(
                &self.cfg.export_product,
                &self.cfg.export_tariff,
                start,
                end,
            )?,
        };
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

#[cfg(test)]
#[path = "../tests/unit/export.rs"]
mod tests;
