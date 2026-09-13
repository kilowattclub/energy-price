use std::sync::{Arc, Mutex};

use chrono::{Duration, TimeZone};

use super::*;

type RequestedRange = Arc<Mutex<Option<(DateTime<Utc>, DateTime<Utc>)>>>;

struct MockSource(RequestedRange);

impl ImportSource for MockSource {
    fn fetch(
        &self,
        _product: &str,
        _tariff: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ImportSlot>, ImportError> {
        self.0.lock().unwrap().replace((from, to));
        let mut slots = (0..48)
            .rev()
            .map(|index| ImportSlot {
                start: from + Duration::minutes(index * 30),
                price_p_per_kwh: index as f64,
            })
            .collect::<Vec<_>>();
        slots.push(slots[0]);
        Ok(slots)
    }
}

#[test]
fn returns_the_exact_requested_utc_day_sorted_and_deduplicated() {
    let requested = Arc::new(Mutex::new(None));
    let client = OctopusImports {
        cfg: MarketConfig::default(),
        source: Box::new(MockSource(Arc::clone(&requested))),
    };
    let start = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
    let end = start + Duration::hours(24);

    let slots = client.get_slots(start, end).unwrap();

    assert_eq!(*requested.lock().unwrap(), Some((start, end)));
    assert_eq!(slots.len(), 48);
    assert_eq!(slots.first().unwrap().start, start);
    assert_eq!(slots.last().unwrap().start, end - Duration::minutes(30));
}
