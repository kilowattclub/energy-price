# energy-price

Electricity import and export price slots in pence/kWh. Includes Octopus tariff
clients and a read-only Axle event client. No inverter, optimiser, Brain settings,
or private dependencies are required.

```rust,no_run
use chrono::{Duration, Utc};
use energy_price::{get_import, get_export, MarketConfig, OctopusImports, OctopusExports};
let settings = MarketConfig::default(); // set your import and export product/tariff
let start = Utc::now();
let horizon = start..start + Duration::days(2);
let imports = get_import(&OctopusImports::new(settings.clone()), horizon.clone())?;
let exports = get_export(&OctopusExports::new(settings), horizon, None)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Pass `Some(&AxleForecast)` to `get_export` to include an expected export-event
reward. `AxleEvents::get_event` reads the current event; reward rates are supplied
by the caller because Axle's event endpoint does not provide them. Import events
do not add export rewards. The returned `ExportSlot` exposes the supplier tariff,
the additional reward, and their total. Partial-slot rewards use time overlap
assuming constant export power. Exact event-window planners should use the raw
tariff and event separately to avoid counting rewards twice. These are estimates,
not settled revenue.

Slots are timestamped in UTC and sorted. Unpublished slots remain absent; missing
prices are never synthesized. Supplier calls have five-second timeouts; Axle calls
have a 500 ms timeout. `with_source` supports other providers and offline tests.
`standard::StandardTariff` also provides the cached regional Flexible Octopus
comparison rate. No API keys, member settings or recorded household data ship here.

## Development and release

Run `cargo test --locked`, `cargo clippy --locked --all-targets -- --deny warnings`,
and `cargo package --locked`. The package is prepared for crates.io; it has not
yet been published. Consumers currently pin a Git revision. When ready, review the
package, publish with `cargo publish --locked`, and tag the released version.
The CI workflow checks the package without publishing it. MIT licensed.
