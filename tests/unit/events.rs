use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use chrono::TimeZone;

use super::*;

fn at(hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 26, hour, minute, 0).unwrap()
}

#[test]
fn nullable_response_means_no_event() {
    let response: EventResponse = serde_json::from_value(serde_json::json!({
        "start_time": null,
        "end_time": null,
        "import_export": null
    }))
    .unwrap();
    assert_eq!(response.into_event().unwrap(), None);
}

#[test]
fn opted_out_events_cannot_authorize_dispatch() {
    let response: EventResponse = serde_json::from_value(serde_json::json!({
        "start_time": "2026-08-26T12:00:00Z",
        "end_time": "2026-08-26T13:00:00Z",
        "import_export": "export",
        "opted_out": true
    }))
    .unwrap();
    assert_eq!(response.into_event().unwrap(), None);
    let response: EventResponse = serde_json::from_value(serde_json::json!({
        "start_time": "2026-08-26T12:00:00Z",
        "end_time": "2026-08-28T13:00:00Z",
        "import_export": "export"
    }))
    .unwrap();
    assert!(response.into_event().is_err());
}

#[test]
fn timezone_offsets_are_normalised_to_utc() {
    let response: EventResponse = serde_json::from_value(serde_json::json!({
        "start_time": "2026-08-26T13:00:00+01:00",
        "end_time": "2026-08-26T13:30:00+01:00",
        "import_export": "export"
    }))
    .unwrap();
    let event = response.into_event().unwrap().unwrap();
    assert_eq!(event.start_time, at(12, 0));
    assert_eq!(event.end_time, at(12, 30));
}

#[test]
fn partial_and_backwards_events_are_rejected() {
    for value in [
        serde_json::json!({
            "start_time": "2026-08-26T12:00:00Z",
            "end_time": null,
            "import_export": "export"
        }),
        serde_json::json!({
            "start_time": "2026-08-26T13:00:00Z",
            "end_time": "2026-08-26T12:00:00Z",
            "import_export": "export"
        }),
    ] {
        let response: EventResponse = serde_json::from_value(value).unwrap();
        assert!(response.into_event().is_err());
    }
}

struct CountingSource(Arc<AtomicUsize>);

impl AxleSource for CountingSource {
    fn fetch(&self, _url: &str, _api_key: &str) -> Result<Option<AxleEvent>, AxleError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

#[test]
fn every_call_reaches_the_provider() {
    let count = Arc::new(AtomicUsize::new(0));
    let client = AxleEvents {
        cfg: AxleConfig {
            enabled: true,
            api_url: "https://api.axle.energy".into(),
            api_key: "member-key".into(),
            ..AxleConfig::default()
        },
        source: Box::new(CountingSource(Arc::clone(&count))),
    };
    client.get_event().unwrap();
    client.get_event().unwrap();
    client.get_event().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[test]
fn disabled_axle_does_not_query_the_provider() {
    let count = Arc::new(AtomicUsize::new(0));
    let client = AxleEvents {
        cfg: AxleConfig::default(),
        source: Box::new(CountingSource(Arc::clone(&count))),
    };
    assert_eq!(client.get_event().unwrap(), None);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
fn request_timeout_fits_inside_the_one_second_control_tick() {
    assert!(REQUEST_TIMEOUT < Duration::from_secs(1));
}
