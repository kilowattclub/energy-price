use super::{rewards, EventInfo};
use crate::AxleConfig;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::Path;

/// Provider-specific configuration stays inside energy-price.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub(super) axle: AxleConfig,
}

impl ProviderConfig {
    /// Consume only provider-owned sections, leaving the host to validate its own fields.
    /// Retains compatibility with existing `[axle]` configurations.
    pub fn extract(table: &mut toml::Table) -> Result<Self, String> {
        let axle = table
            .remove("axle")
            .map(|value| value.try_into().map_err(|e: toml::de::Error| e.to_string()))
            .transpose()?
            .unwrap_or_default();
        Ok(Self { axle })
    }
    pub fn validate(&self) -> Result<(), String> {
        self.axle.validate()
    }
    /// Preserve the dashboard wire schema and historical ledger paths without
    /// requiring the host to know any provider-specific field names.
    pub fn snapshot_fields(
        &self,
        directory: &Path,
        now: DateTime<Utc>,
        event: Option<&EventInfo>,
    ) -> serde_json::Map<String, serde_json::Value> {
        serde_json::json!({
            "axle_self_dispatch": self.axle.enabled && self.axle.self_dispatch,
            "axle_event": event.map(|e| serde_json::json!({"start":e.start, "end":e.end, "direction":e.direction})),
            "axle_rewards": rewards::snapshot(&directory.join("axle-rewards.json"), now),
        }).as_object().expect("object").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    #[test]
    fn extraction_preserves_host_settings_and_rejects_bad_provider_settings() {
        let mut table: toml::Table = toml::from_str(
            "[general]\ntimezone='Europe/London'\n[axle]\nenabled=true\napi_key='test-key'",
        )
        .unwrap();
        let cfg = ProviderConfig::extract(&mut table).unwrap();
        cfg.validate().unwrap();
        assert!(table.contains_key("general"));
        assert!(!table.contains_key("axle"));
        for body in [
            "enabled=true",
            "unknown=true",
            "export_reward_p_per_kwh=-1",
            "import_reward_p_per_kwh=nan",
            "enabled=true\napi_key='key'\napi_url='http://api.axle.energy'",
        ] {
            let mut table = toml::from_str(&format!("[axle]\n{body}")).unwrap();
            assert!(
                ProviderConfig::extract(&mut table)
                    .and_then(|c| c.validate())
                    .is_err(),
                "{body}"
            );
        }
        for field in ["import_reward_p_per_kwh", "export_reward_p_per_kwh"] {
            for value in ["-1", "nan", "inf"] {
                let mut table = toml::from_str(&format!("[axle]\n{field}={value}")).unwrap();
                assert!(ProviderConfig::extract(&mut table)
                    .unwrap()
                    .validate()
                    .is_err());
            }
        }
        let mut table = toml::from_str("[axle]\nenabled=true\napi_key='key'\napi_url='http://127.0.0.1:3000'\nimport_reward_p_per_kwh=25").unwrap();
        ProviderConfig::extract(&mut table)
            .unwrap()
            .validate()
            .unwrap();
    }
    #[test]
    fn legacy_snapshot_flags_require_enabled_self_dispatch() {
        let now = Utc.with_ymd_and_hms(2026, 9, 10, 22, 0, 0).unwrap();
        let directory =
            std::env::temp_dir().join(format!("provider-snapshot-{}", std::process::id()));
        for (enabled, self_dispatch, expected) in [
            (false, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            let cfg = ProviderConfig {
                axle: AxleConfig {
                    enabled,
                    self_dispatch,
                    ..Default::default()
                },
            };
            let fields = cfg.snapshot_fields(&directory, now, None);
            assert_eq!(fields["axle_self_dispatch"], expected);
            assert!(fields["axle_event"].is_null());
            assert_eq!(fields["axle_rewards"], serde_json::json!([]));
        }
    }
}
