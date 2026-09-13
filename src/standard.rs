//! Regional Flexible Octopus price used for the no-battery comparison.
//!
//! Octopus publishes the active Flexible product and regional unit rates via
//! its public API. The last successful result is persisted so a Pi restart or
//! a temporary API outage does not reset the savings baseline.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::MarketConfig;

const API_BASE: &str = "https://api.octopus.energy/v1";
const FLEXIBLE_DISPLAY_NAME: &str = "Flexible Octopus";
const CACHE_VERSION: u8 = 1;
const REFRESH_AFTER: Duration = Duration::hours(6);
const RETRY_AFTER: Duration = Duration::minutes(30);
const MAX_PRODUCT_PAGES: usize = 10;

pub const STANDARD_TARIFF_FILE_NAME: &str = "standard-tariff.json";

#[derive(Debug, thiserror::Error)]
pub enum StandardTariffError {
    #[error("Flexible Octopus tariff unavailable: {0}")]
    Unavailable(String),
    #[error("Octopus standard-tariff request failed: {0}")]
    Http(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Product {
    pub code: String,
    pub display_name: String,
    pub available_from: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateQuote {
    pub price_p_per_kwh: f64,
    pub payment_method: String,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
}

pub trait StandardTariffSource: Send + Sync {
    fn products(&self, at: DateTime<Utc>) -> Result<Vec<Product>, StandardTariffError>;

    fn rates(
        &self,
        product: &str,
        tariff: &str,
        at: DateTime<Utc>,
    ) -> Result<Vec<RateQuote>, StandardTariffError>;
}

struct HttpStandardTariffSource {
    agent: ureq::Agent,
}

impl HttpStandardTariffSource {
    fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(5))
                .build(),
        }
    }

    fn get_json(&self, url: &str) -> Result<serde_json::Value, StandardTariffError> {
        self.agent
            .get(url)
            .call()
            .map_err(|error| StandardTariffError::Http(error.to_string()))?
            .into_json()
            .map_err(|error| StandardTariffError::Http(error.to_string()))
    }
}

impl StandardTariffSource for HttpStandardTariffSource {
    fn products(&self, at: DateTime<Utc>) -> Result<Vec<Product>, StandardTariffError> {
        let at = at.format("%Y-%m-%dT%H:%M:%SZ");
        let mut next = Some(format!(
            "{API_BASE}/products/?brand=OCTOPUS_ENERGY&is_business=false&is_prepay=false&is_variable=true&available_at={at}&page_size=100"
        ));
        let mut products = Vec::new();

        for _ in 0..MAX_PRODUCT_PAGES {
            let Some(url) = next.take() else {
                break;
            };
            if !url.starts_with(API_BASE) {
                return Err(StandardTariffError::Unavailable(
                    "Octopus returned an unexpected products page URL".into(),
                ));
            }
            let payload = self.get_json(&url)?;
            products.extend(
                payload
                    .get("results")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|row| {
                        Some(Product {
                            code: row.get("code")?.as_str()?.to_owned(),
                            display_name: row.get("display_name")?.as_str()?.to_owned(),
                            available_from: DateTime::parse_from_rfc3339(
                                row.get("available_from")?.as_str()?,
                            )
                            .ok()?
                            .with_timezone(&Utc),
                        })
                    }),
            );
            next = payload
                .get("next")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
        }
        if next.is_some() {
            return Err(StandardTariffError::Unavailable(
                "Octopus products response exceeded the pagination limit".into(),
            ));
        }
        Ok(products)
    }

    fn rates(
        &self,
        product: &str,
        tariff: &str,
        at: DateTime<Utc>,
    ) -> Result<Vec<RateQuote>, StandardTariffError> {
        let timestamp = |time: DateTime<Utc>| time.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let url = format!(
            "{API_BASE}/products/{product}/electricity-tariffs/{tariff}/standard-unit-rates/?period_from={}&period_to={}&page_size=100",
            timestamp(at),
            timestamp(at + Duration::seconds(1)),
        );
        let payload = self.get_json(&url)?;
        Ok(payload
            .get("results")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let price_p_per_kwh = row.get("value_inc_vat")?.as_f64()?;
                let payment_method = row.get("payment_method")?.as_str()?.to_owned();
                let valid_from = DateTime::parse_from_rfc3339(row.get("valid_from")?.as_str()?)
                    .ok()?
                    .with_timezone(&Utc);
                let valid_to = row
                    .get("valid_to")
                    .and_then(serde_json::Value::as_str)
                    .map(DateTime::parse_from_rfc3339)
                    .transpose()
                    .ok()?
                    .map(|time| time.with_timezone(&Utc));
                price_p_per_kwh.is_finite().then_some(RateQuote {
                    price_p_per_kwh,
                    payment_method,
                    valid_from,
                    valid_to,
                })
            })
            .collect())
    }
}

pub struct OctopusStandardTariff {
    cfg: MarketConfig,
    source: Box<dyn StandardTariffSource>,
}

impl OctopusStandardTariff {
    pub fn new(cfg: MarketConfig) -> Self {
        Self {
            cfg,
            source: Box::new(HttpStandardTariffSource::new()),
        }
    }

    fn resolve(&self, now: DateTime<Utc>) -> Result<StoredRate, StandardTariffError> {
        let region = self.cfg.gsp_region().ok_or_else(|| {
            StandardTariffError::Unavailable(
                "the configured import tariff has no valid GSP region".into(),
            )
        })?;
        let product = self
            .source
            .products(now)?
            .into_iter()
            .filter(|product| product.display_name == FLEXIBLE_DISPLAY_NAME)
            .max_by_key(|product| product.available_from)
            .ok_or_else(|| {
                StandardTariffError::Unavailable(
                    "no active Flexible Octopus product was returned".into(),
                )
            })?;
        let tariff = format!("E-1R-{}-{region}", product.code);
        let rate = self
            .source
            .rates(&product.code, &tariff, now)?
            .into_iter()
            .filter(|rate| {
                rate.payment_method == self.cfg.standard_payment_method
                    && rate.valid_from <= now
                    && rate.valid_to.is_none_or(|valid_to| now < valid_to)
            })
            .max_by_key(|rate| rate.valid_from)
            .ok_or_else(|| {
                StandardTariffError::Unavailable(format!(
                    "no active {} rate for {tariff}",
                    self.cfg.standard_payment_method
                ))
            })?;

        Ok(StoredRate {
            version: CACHE_VERSION,
            product: product.code,
            tariff,
            payment_method: rate.payment_method,
            price_p_per_kwh: rate.price_p_per_kwh,
            fetched_at: now,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
struct StoredRate {
    version: u8,
    product: String,
    tariff: String,
    payment_method: String,
    price_p_per_kwh: f64,
    fetched_at: DateTime<Utc>,
}

impl StoredRate {
    fn valid_for(&self, cfg: &MarketConfig) -> bool {
        let Some(region) = cfg.gsp_region() else {
            return false;
        };
        self.version == CACHE_VERSION
            && self.tariff.ends_with(&format!("-{region}"))
            && self.payment_method == cfg.standard_payment_method
            && self.price_p_per_kwh.is_finite()
            && self.price_p_per_kwh >= 0.0
    }
}

/// Refreshes and persists the regional comparison rate while always retaining
/// a configured fallback for first boot without network access.
pub struct StandardTariff {
    path: PathBuf,
    cfg: MarketConfig,
    client: OctopusStandardTariff,
    current: Option<StoredRate>,
    next_fetch_at: DateTime<Utc>,
}

impl StandardTariff {
    pub fn new(path: PathBuf, cfg: MarketConfig, now: DateTime<Utc>) -> Self {
        let client = OctopusStandardTariff::new(cfg.clone());
        Self::with_client(path, cfg, client, now)
    }

    fn with_client(
        path: PathBuf,
        cfg: MarketConfig,
        client: OctopusStandardTariff,
        now: DateTime<Utc>,
    ) -> Self {
        let current = load(&path).filter(|rate| rate.valid_for(&cfg));
        Self {
            path,
            cfg,
            client,
            current,
            next_fetch_at: now,
        }
    }

    pub fn price_p_per_kwh(&mut self, now: DateTime<Utc>) -> f64 {
        if now >= self.next_fetch_at {
            match self.client.resolve(now) {
                Ok(rate) => {
                    let changed = self.current.as_ref().is_none_or(|current| {
                        current.product != rate.product
                            || current.tariff != rate.tariff
                            || current.payment_method != rate.payment_method
                            || (current.price_p_per_kwh - rate.price_p_per_kwh).abs() > f64::EPSILON
                    });
                    if changed {
                        log::info!(
                            target: "kwc.savings",
                            "Flexible Octopus comparison: product={} tariff={} payment={} rate={:.4}p/kWh",
                            rate.product,
                            rate.tariff,
                            rate.payment_method,
                            rate.price_p_per_kwh,
                        );
                    }
                    if let Err(error) = save_atomic(&self.path, &rate) {
                        log::warn!(
                            target: "kwc.savings",
                            "could not persist standard tariff: path={} error={error}",
                            self.path.display()
                        );
                    }
                    self.current = Some(rate);
                    self.next_fetch_at = now + REFRESH_AFTER;
                }
                Err(error) => {
                    let source = if self.current.is_some() {
                        "last persisted API rate"
                    } else {
                        "configured fallback"
                    };
                    log::warn!(
                        target: "kwc.savings",
                        "{error}; using {source} and retrying in {} minutes",
                        RETRY_AFTER.num_minutes(),
                    );
                    self.next_fetch_at = now + RETRY_AFTER;
                }
            }
        }
        self.current
            .as_ref()
            .map_or(self.cfg.standard_tariff_p, |rate| rate.price_p_per_kwh)
    }
}

fn load(path: &Path) -> Option<StoredRate> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::warn!(target: "kwc.savings", "could not read standard tariff: path={} error={error}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(rate) => Some(rate),
        Err(error) => {
            log::warn!(target: "kwc.savings", "corrupt standard tariff ignored: path={} error={error}", path.display());
            None
        }
    }
}

fn save_atomic(path: &Path, rate: &StoredRate) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "standard tariff path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("standard-tariff")
    ));
    let json = serde_json::to_vec_pretty(rate).map_err(|error| error.to_string())?;
    std::fs::write(&temp, json).map_err(|error| error.to_string())?;
    std::fs::rename(&temp, path).map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "../tests/unit/standard.rs"]
mod tests;
