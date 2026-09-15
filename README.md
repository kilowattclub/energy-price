# energy-price

Electricity import and export price slots in pence/kWh. Includes Octopus tariff
clients, a read-only Axle event client and provider-owned dispatch policy. No inverter, optimiser, Brain settings,
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

## Dispatch providers

`dispatch::DispatchService` owns Axle event polling, external handover protection,
opted-in self-dispatch policy, retry timing and measured reward accounting.
Applications use the provider-neutral `DispatchProvider` contract. `poll` returns
schedule updates; `control` returns `Unavailable`, `External`, `Override` or
`Tariff`. The host owns its clock, inverter connection, command application and
shutdown. Pass actual write outcomes to `record_attempt`; use `permits_shutdown`
before sending a shutdown reset. No hardware commands are sent by this library.

`forecasts` supplies event windows and configured reward assumptions separately
from supplier tariffs. `active_event` supplies provider metadata for reporting.
`Reading` takes signed grid/battery power, SOC and timestamp freshness; rewards
use measured net grid energy with gaps preserved. Short leases, feed-failure
limits, SOC latches and external ownership protection retain the original policy.

`ProviderConfig::extract` consumes provider sections from a host TOML table and
leaves other sections for the host to validate. Call `validate` before use.
Add this section to the host configuration only when using Axle:

```toml
[axle]
enabled = true
api_url = "https://api.axle.energy"
api_key = "YOUR_MEMBER_API_KEY"
# Enable only after selecting Self-Dispatch Mode in the Axle portal.
self_dispatch = false
# Assumptions, additional to supplier tariffs; not API rates or settled payments.
export_reward_p_per_kwh = 100.0
import_reward_p_per_kwh = 0.0
```

The member API key comes from the Home Assistant section of `vpp.axle.energy`.
Disabled providers require no credentials. Provider settings, legacy dashboard
field names and the `axle-handover.json`, `axle-self-dispatch.json` and
`axle-rewards.json` state formats are owned here. `snapshot_fields` returns those
reporting fields for the host to include without knowing their schema. Existing
state directories continue to work without moving or resetting their files.

Provider tests live alongside their implementation and cover exact boundaries,
restarts, cancellations, expired feeds, bounded retries and signed reward energy.
