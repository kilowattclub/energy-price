use chrono::{Duration, TimeZone, Utc};
use energy_price::{
    export::{ExportPriceError, ExportPriceSource},
    import::{ImportError, ImportSource},
    *,
};
struct Import;
impl ImportSource for Import {
    fn fetch(
        &self,
        _: &str,
        _: &str,
        from: chrono::DateTime<Utc>,
        _: chrono::DateTime<Utc>,
    ) -> Result<Vec<ImportSlot>, ImportError> {
        Ok(vec![
            ImportSlot {
                start: from + Duration::minutes(30),
                price_p_per_kwh: 30.0,
            },
            ImportSlot {
                start: from,
                price_p_per_kwh: -2.0,
            },
        ])
    }
}
struct Export;
impl ExportPriceSource for Export {
    fn fetch(
        &self,
        _: &str,
        _: &str,
        from: chrono::DateTime<Utc>,
        _: chrono::DateTime<Utc>,
    ) -> Result<Vec<ExportPriceSlot>, ExportPriceError> {
        Ok((0..3)
            .map(|i| ExportPriceSlot {
                start: from + Duration::minutes(i * 30),
                price_p_per_kwh: 15.0,
            })
            .collect())
    }
}
fn client() -> OctopusExports {
    OctopusExports::with_source(
        MarketConfig {
            export_product: "test".into(),
            export_tariff: "test".into(),
            ..MarketConfig::default()
        },
        Box::new(Export),
    )
}
#[test]
fn public_import_api_sorts_and_preserves_negative_and_unpublished_prices() {
    let start = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
    let result = get_import(
        &OctopusImports::with_source(MarketConfig::default(), Box::new(Import)),
        start..start + Duration::days(1),
    )
    .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].start, start);
    assert_eq!(result[0].price_p_per_kwh, -2.0);
}
#[test]
fn export_reward_is_optional_additional_and_limited_to_the_event_overlap() {
    let start = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
    let horizon = start..start + Duration::minutes(90);
    let plain = get_export(&client(), horizon.clone(), None).unwrap();
    assert!(plain
        .iter()
        .all(|s| s.price_p_per_kwh == 15.0 && s.axle_reward_p_per_kwh == 0.0));
    let mut axle = AxleForecast {
        event: AxleEvent {
            start_time: start + Duration::minutes(15),
            end_time: start + Duration::minutes(60),
            direction: AxleDirection::Export,
        },
        reward_p_per_kwh: 100.0,
        self_dispatch: true,
    };
    let enriched = get_export(&client(), horizon.clone(), Some(&axle)).unwrap();
    assert_eq!(
        enriched
            .iter()
            .map(|s| s.price_p_per_kwh)
            .collect::<Vec<_>>(),
        vec![65.0, 115.0, 15.0]
    );
    assert!(enriched.iter().all(|s| s.tariff_p_per_kwh == 15.0));
    axle.event.direction = AxleDirection::Import;
    assert_eq!(
        get_export(&client(), horizon.clone(), Some(&axle)).unwrap(),
        plain
    );
    axle.reward_p_per_kwh = f64::NAN;
    assert!(get_export(&client(), horizon.clone(), Some(&axle)).is_err());
    axle.reward_p_per_kwh = 100.0;
    axle.event.end_time = axle.event.start_time;
    assert!(get_export(&client(), horizon, Some(&axle)).is_err());
}
