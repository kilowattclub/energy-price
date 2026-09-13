use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use chrono::TimeZone;

use super::*;

#[derive(Clone)]
struct MockSource {
    products: Result<Vec<Product>, String>,
    rates: Result<Vec<RateQuote>, String>,
    calls: Arc<AtomicUsize>,
}

impl StandardTariffSource for MockSource {
    fn products(&self, _at: DateTime<Utc>) -> Result<Vec<Product>, StandardTariffError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.products
            .clone()
            .map_err(StandardTariffError::Unavailable)
    }

    fn rates(
        &self,
        _product: &str,
        _tariff: &str,
        _at: DateTime<Utc>,
    ) -> Result<Vec<RateQuote>, StandardTariffError> {
        self.rates.clone().map_err(StandardTariffError::Unavailable)
    }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 2, 12, 0, 0).unwrap()
}

fn product(code: &str, available_from: DateTime<Utc>) -> Product {
    Product {
        code: code.into(),
        display_name: FLEXIBLE_DISPLAY_NAME.into(),
        available_from,
    }
}

fn rate(payment_method: &str, price_p_per_kwh: f64) -> RateQuote {
    RateQuote {
        price_p_per_kwh,
        payment_method: payment_method.into(),
        valid_from: now() - Duration::days(30),
        valid_to: None,
    }
}

fn temp_path(label: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "kwc-standard-{label}-{}-{}.json",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn resolves_latest_flexible_product_for_import_region_and_payment_method() {
    let calls = Arc::new(AtomicUsize::new(0));
    let source = MockSource {
        products: Ok(vec![
            product("VAR-OLD", now() - Duration::days(100)),
            product("VAR-CURRENT", now() - Duration::days(10)),
            Product {
                code: "TRACKER".into(),
                display_name: "Octopus Tracker".into(),
                available_from: now(),
            },
        ]),
        rates: Ok(vec![
            rate("NON_DIRECT_DEBIT", 27.81),
            rate("DIRECT_DEBIT", 26.347335),
        ]),
        calls,
    };
    let client = OctopusStandardTariff {
        cfg: MarketConfig::default(),
        source: Box::new(source),
    };

    let resolved = client.resolve(now()).unwrap();

    assert_eq!(resolved.product, "VAR-CURRENT");
    assert_eq!(resolved.tariff, "E-1R-VAR-CURRENT-C");
    assert_eq!(resolved.payment_method, "DIRECT_DEBIT");
    assert_eq!(resolved.price_p_per_kwh, 26.347335);
}

#[test]
fn persists_api_rate_and_reuses_it_during_an_outage_after_restart() {
    let path = temp_path("restart");
    let calls = Arc::new(AtomicUsize::new(0));
    let cfg = MarketConfig::default();
    let good = MockSource {
        products: Ok(vec![product("VAR-CURRENT", now() - Duration::days(10))]),
        rates: Ok(vec![rate("DIRECT_DEBIT", 26.347335)]),
        calls: Arc::clone(&calls),
    };
    let client = OctopusStandardTariff {
        cfg: cfg.clone(),
        source: Box::new(good),
    };
    let mut tariff = StandardTariff::with_client(path.clone(), cfg.clone(), client, now());
    assert_eq!(tariff.price_p_per_kwh(now()), 26.347335);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let outage = MockSource {
        products: Err("offline".into()),
        rates: Ok(Vec::new()),
        calls,
    };
    let client = OctopusStandardTariff {
        cfg: cfg.clone(),
        source: Box::new(outage),
    };
    let mut restarted =
        StandardTariff::with_client(path.clone(), cfg, client, now() + Duration::minutes(1));

    assert_eq!(
        restarted.price_p_per_kwh(now() + Duration::minutes(1)),
        26.347335
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn uses_configured_fallback_on_first_boot_outage() {
    let path = temp_path("fallback");
    let cfg = MarketConfig::default();
    let source = MockSource {
        products: Err("offline".into()),
        rates: Ok(Vec::new()),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let client = OctopusStandardTariff {
        cfg: cfg.clone(),
        source: Box::new(source),
    };
    let mut tariff = StandardTariff::with_client(path, cfg.clone(), client, now());

    assert_eq!(tariff.price_p_per_kwh(now()), cfg.standard_tariff_p);
}
