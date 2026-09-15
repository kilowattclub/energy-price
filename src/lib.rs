#![doc = include_str!("../README.md")]
//! Electricity prices in pence/kWh, with optional additional Axle export rewards.
pub mod axle;
pub mod dispatch;
pub use dispatch::ProviderConfig;
pub use events::{
    AxleDirection as DispatchDirection, AxleEvent as DispatchEvent,
    AxleForecast as DispatchForecast,
};
mod config;
mod events;
pub mod export;
pub mod import;
pub mod standard;
use chrono::{DateTime, Duration, Utc};
pub use config::{AxleConfig, MarketConfig};
pub use events::{AxleDirection, AxleEvent, AxleForecast};
pub use export::{ExportPriceSlot, OctopusExports};
pub use import::{ImportSlot, OctopusImports};
use std::ops::Range;

/// One half-hour of export prices. Rewards are additional to the supplier tariff.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportSlot {
    pub start: DateTime<Utc>,
    pub tariff_p_per_kwh: f64,
    pub axle_reward_p_per_kwh: f64,
    pub price_p_per_kwh: f64,
}

/// Fetch the published import prices within the requested UTC range.
/// Missing and unpublished slots are never filled with invented prices.
pub fn get_import(
    client: &OctopusImports,
    horizon: Range<DateTime<Utc>>,
) -> Result<Vec<ImportSlot>, import::ImportError> {
    client.get_slots(horizon.start, horizon.end)
}

/// Fetch export prices with an optional Axle reward forecast.
///
/// Only export events earn export rewards. A partial-slot event is weighted by
/// its overlap with the half-hour, assuming constant export power in that slot.
/// Both components are exposed so exact event-window planners can use the raw
/// tariff and event separately without counting the reward twice. The configured
/// reward is an estimate, not a reward rate returned by Axle or a settlement.
pub fn get_export(
    client: &OctopusExports,
    horizon: Range<DateTime<Utc>>,
    axle: Option<&AxleForecast>,
) -> Result<Vec<ExportSlot>, export::ExportPriceError> {
    with_axle(&client.get_slots(horizon.start, horizon.end)?, axle)
}

fn with_axle(
    slots: &[ExportPriceSlot],
    axle: Option<&AxleForecast>,
) -> Result<Vec<ExportSlot>, export::ExportPriceError> {
    if axle.is_some_and(|a| {
        !a.reward_p_per_kwh.is_finite()
            || a.reward_p_per_kwh < 0.0
            || a.event.end_time <= a.event.start_time
            || a.event.end_time - a.event.start_time > Duration::hours(24)
    }) {
        return Err(export::ExportPriceError::Unavailable(
            "invalid Axle reward forecast".into(),
        ));
    }
    Ok(slots
        .iter()
        .map(|slot| {
            let reward = axle
                .filter(|a| a.event.direction == AxleDirection::Export)
                .map_or(0.0, |a| {
                    let start = slot.start.max(a.event.start_time);
                    let end = (slot.start + Duration::minutes(30)).min(a.event.end_time);
                    a.reward_p_per_kwh
                        * ((end - start).num_milliseconds() as f64 / 1_800_000.0).clamp(0.0, 1.0)
                });
            ExportSlot {
                start: slot.start,
                tariff_p_per_kwh: slot.price_p_per_kwh,
                axle_reward_p_per_kwh: reward,
                price_p_per_kwh: slot.price_p_per_kwh + reward,
            }
        })
        .collect())
}
