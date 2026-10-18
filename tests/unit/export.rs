use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use chrono::{Duration, TimeZone};
use serde_json::json;

use super::*;

/// Serve a recorded Octopus response through the same row expansion as HTTP.
struct Recorded {
    payload: Result<serde_json::Value, String>,
    calls: Arc<AtomicUsize>,
}

impl ExportPriceSource for Recorded {
    fn fetch(
        &self,
        _product: &str,
        _tariff: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ExportPriceSlot>, ExportPriceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.payload
            .as_ref()
            .map(|payload| rate_slots(payload, from, to))
            .map_err(|error| ExportPriceError::Http(error.clone()))
    }
}

fn client(cfg: MarketConfig, payload: Result<serde_json::Value, String>) -> OctopusExports {
    OctopusExports::with_source(
        MarketConfig {
            export_product: "OUTGOING-VAR-24-10-26".into(),
            export_tariff: "E-1R-OUTGOING-VAR-24-10-26-F".into(),
            ..cfg
        },
        Box::new(Recorded {
            payload,
            calls: Arc::default(),
        }),
    )
}

fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, hour, minute, 0)
        .unwrap()
}

fn rate(price: f64, from: &str, to: Option<&str>) -> serde_json::Value {
    json!({
        "value_exc_vat": price,
        "value_inc_vat": price,
        "valid_from": from,
        "valid_to": to,
        "payment_method": null,
    })
}

fn agile_outgoing(from: DateTime<Utc>, count: i64) -> serde_json::Value {
    // Octopus lists the newest slot first.
    let rows = (0..count)
        .rev()
        .map(|i| {
            let start = from + Duration::minutes(30 * i);
            let end = start + Duration::minutes(30);
            rate(
                5.0 + i as f64 * 0.5,
                &start.to_rfc3339(),
                Some(&end.to_rfc3339()),
            )
        })
        .collect::<Vec<_>>();
    json!({ "count": count, "next": null, "previous": null, "results": rows })
}

#[test]
fn one_open_ended_flat_rate_prices_every_half_hour_in_the_window() {
    // Recorded from OUTGOING-VAR-24-10-26, region F, on 18 October 2026.
    let payload = json!({
        "count": 1, "next": null, "previous": null,
        "results": [rate(12.0, "2026-03-01T00:00:00Z", None)],
    });
    let start = at(18, 0, 0);
    let slots = client(MarketConfig::default(), Ok(payload))
        .get_slots(start, start + Duration::days(2))
        .unwrap();
    assert_eq!(slots.len(), 96);
    for (i, slot) in slots.iter().enumerate() {
        assert_eq!(slot.start, start + Duration::minutes(30 * i as i64));
        assert_eq!(slot.price_p_per_kwh, 12.0);
    }
}

#[test]
fn flat_rate_starts_at_the_first_half_hour_boundary_after_an_unaligned_request() {
    let payload = json!({ "results": [rate(12.0, "2026-03-01T00:00:00Z", None)] });
    let slots = client(MarketConfig::default(), Ok(payload))
        .get_slots(at(18, 12, 10), at(18, 14, 0))
        .unwrap();
    let starts = slots.iter().map(|s| s.start).collect::<Vec<_>>();
    assert_eq!(starts, [at(18, 12, 30), at(18, 13, 0), at(18, 13, 30)]);
}

#[test]
fn a_rate_change_inside_the_window_uses_each_rate_for_its_own_half_hours() {
    let payload = json!({
        "results": [
            rate(12.0, "2026-03-01T00:00:00Z", None),
            rate(15.0, "2024-10-25T23:00:00Z", Some("2026-03-01T00:00:00Z")),
        ],
    });
    let change = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
    let slots = client(MarketConfig::default(), Ok(payload))
        .get_slots(change - Duration::hours(12), change + Duration::hours(12))
        .unwrap();
    assert_eq!(slots.len(), 48);
    assert!(slots[..24]
        .iter()
        .all(|s| s.start < change && s.price_p_per_kwh == 15.0));
    assert!(slots[24..]
        .iter()
        .all(|s| s.start >= change && s.price_p_per_kwh == 12.0));
    assert!(slots
        .windows(2)
        .all(|w| w[1].start - w[0].start == Duration::minutes(30)));
}

#[test]
fn half_hourly_agile_outgoing_rows_are_unchanged() {
    let first = at(18, 0, 0);
    let payload = agile_outgoing(first, 96);
    // The previous parser kept each row whose valid_from fell inside the window.
    let previous = |from: DateTime<Utc>, to: DateTime<Utc>| {
        let mut slots = payload["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| ExportPriceSlot {
                start: DateTime::parse_from_rfc3339(row["valid_from"].as_str().unwrap())
                    .unwrap()
                    .with_timezone(&Utc),
                price_p_per_kwh: row["value_inc_vat"].as_f64().unwrap(),
            })
            .filter(|slot| from <= slot.start && slot.start < to)
            .collect::<Vec<_>>();
        slots.sort_by_key(|slot| slot.start);
        slots
    };
    for (from, to) in [
        (first, first + Duration::days(2)),
        (at(18, 12, 10), at(19, 6, 0)),
        (at(17, 22, 0), at(18, 1, 0)),
        (at(19, 23, 0), at(20, 3, 0)),
    ] {
        let slots = client(MarketConfig::default(), Ok(payload.clone()))
            .get_slots(from, to)
            .unwrap();
        assert_eq!(slots, previous(from, to), "{from}..{to}");
    }
}

#[test]
fn empty_or_failed_responses_leave_export_unpriced() {
    let start = at(18, 0, 0);
    let end = start + Duration::days(2);
    for payload in [
        json!({ "count": 0, "next": null, "previous": null, "results": [] }),
        json!({ "detail": "No tariff found" }),
        // A rate that ended before the window, or a row without a usable price.
        json!({ "results": [rate(15.0, "2024-10-25T23:00:00Z", Some("2026-03-01T00:00:00Z"))] }),
        json!({ "results": [{ "valid_from": "2026-03-01T00:00:00Z", "value_inc_vat": null }] }),
        json!({ "results": [{ "valid_from": "2026-03-01T00:00:00Z", "valid_to": "soon", "value_inc_vat": 12.0 }] }),
    ] {
        assert!(matches!(
            client(MarketConfig::default(), Ok(payload)).get_slots(start, end),
            Err(ExportPriceError::Unavailable(_))
        ));
    }
    assert!(matches!(
        client(MarketConfig::default(), Err("timed out".into())).get_slots(start, end),
        Err(ExportPriceError::Http(_))
    ));
    assert!(matches!(
        OctopusExports::with_source(
            MarketConfig::default(),
            Box::new(Recorded {
                payload: Ok(json!({})),
                calls: Arc::default(),
            }),
        )
        .get_slots(start, end),
        Err(ExportPriceError::Unavailable(_))
    ));
}

#[test]
fn a_configured_fixed_export_price_needs_no_api_and_covers_every_half_hour() {
    let calls = Arc::new(AtomicUsize::new(0));
    let fixed = |price| {
        OctopusExports::with_source(
            MarketConfig {
                export_product: "OUTGOING-VAR-24-10-26".into(),
                export_tariff: "E-1R-OUTGOING-VAR-24-10-26-F".into(),
                export_fixed_p: Some(price),
                ..MarketConfig::default()
            },
            Box::new(Recorded {
                payload: Err("offline".into()),
                calls: Arc::clone(&calls),
            }),
        )
    };
    let slots = fixed(12.0).get_slots(at(18, 0, 5), at(19, 0, 0)).unwrap();
    assert_eq!(slots.len(), 47);
    assert_eq!(slots[0].start, at(18, 0, 30));
    assert!(slots.iter().all(|s| s.price_p_per_kwh == 12.0));
    for invalid in [-1.0, f64::NAN, f64::INFINITY] {
        assert!(fixed(invalid)
            .get_slots(at(18, 0, 0), at(19, 0, 0))
            .is_err());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn fixed_export_prices_parse_from_host_toml() {
    let cfg: MarketConfig = toml::from_str(
        "export_product = \"OUTGOING-VAR-24-10-26\"\n\
         export_tariff = \"E-1R-OUTGOING-VAR-24-10-26-F\"\n\
         export_fixed_p = 12.0\n",
    )
    .unwrap();
    assert_eq!(cfg.export_fixed_p, Some(12.0));
    assert_eq!(MarketConfig::default().export_fixed_p, None);
}
