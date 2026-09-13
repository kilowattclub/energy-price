use serde::Deserialize;
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarketConfig {
    pub import_product: String,
    pub import_tariff: String,
    /// The member's active Octopus export product/tariff. Empty values keep
    /// deliberate export disabled until the tariff has been discovered or
    /// configured explicitly.
    pub export_product: String,
    pub export_tariff: String,
    /// Payment method used for the regional Flexible Octopus comparison.
    pub standard_payment_method: String,
    /// No-battery comparison tariff in p/kWh, used only when Octopus and the
    /// last persisted API result are both unavailable.
    pub standard_tariff_p: f64,
}
impl Default for MarketConfig {
    fn default() -> Self {
        Self {
            import_product: "AGILE-24-10-01".into(),
            import_tariff: "E-1R-AGILE-24-10-01-C".into(),
            export_product: String::new(),
            export_tariff: String::new(),
            standard_payment_method: "DIRECT_DEBIT".into(),
            standard_tariff_p: 26.11,
        }
    }
}
impl MarketConfig {
    pub fn gsp_region(&self) -> Option<char> {
        let suffix = self.import_tariff.rsplit('-').next()?;
        let region = suffix.chars().next()?;
        (suffix.len() == 1 && "ABCDEFGHJKLMNP".contains(region)).then_some(region)
    }
}
fn err<T>(message: impl Into<String>) -> Result<T, String> {
    Err(message.into())
}
/// Direct, read-only access to the member's Axle VPP event feed.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AxleConfig {
    /// Enable only after selecting Self-Dispatch Mode in the Axle account.
    pub self_dispatch: bool,
    /// Expected reward, additional to the export tariff; not supplied by the API.
    pub export_reward_p_per_kwh: f64,
    /// Expected reward per net imported kWh (planning and measured estimates); zero unless explicitly configured.
    pub import_reward_p_per_kwh: f64,
    pub enabled: bool,
    pub api_url: String,
    pub api_key: String,
}

impl Default for AxleConfig {
    fn default() -> Self {
        Self {
            self_dispatch: false,
            export_reward_p_per_kwh: 100.0,
            import_reward_p_per_kwh: 0.0,
            enabled: false,
            api_url: "https://api.axle.energy".into(),
            api_key: String::new(),
        }
    }
}
impl AxleConfig {
    pub fn validate(&self) -> Result<(), String> {
        if [self.export_reward_p_per_kwh, self.import_reward_p_per_kwh]
            .iter()
            .any(|p| !p.is_finite() || *p < 0.0)
        {
            return err("Axle planning rewards must be finite and non-negative");
        }
        if self.enabled {
            let loopback_http = [
                "http://localhost/",
                "http://localhost:",
                "http://127.0.0.1/",
                "http://127.0.0.1:",
                "http://[::1]/",
                "http://[::1]:",
            ]
            .iter()
            .any(|prefix| self.api_url.starts_with(prefix));
            if !(self.api_url.starts_with("https://") || loopback_http) {
                return err(
                    "axle.api_url must use HTTPS (HTTP is allowed only for loopback development)",
                );
            }
            if self.api_key.trim().is_empty() {
                return err("axle.api_key is required when axle is enabled");
            }
        }
        Ok(())
    }
}
