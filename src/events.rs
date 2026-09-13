use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AxleDirection {
    Import,
    Export,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AxleEvent {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub direction: AxleDirection,
}

/// Expected external dispatch and reward used only by the planner.
/// The Home Assistant API supplies the window/direction, not power or reward.
#[derive(Debug, Clone, PartialEq)]
pub struct AxleForecast {
    pub event: AxleEvent,
    pub reward_p_per_kwh: f64,
    pub self_dispatch: bool,
}
