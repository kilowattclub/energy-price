# energy-price

Electricity import and export price slots in pence/kWh. Includes Octopus tariff
clients, a read-only Axle event client and provider-owned dispatch policy. No
inverter, optimiser, Brain settings or private dependencies are required.

```rust,no_run
use chrono::{Duration, Utc};
use energy_price::{get_export, get_import, MarketConfig, OctopusExports, OctopusImports};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = MarketConfig::default(); // set your import and export product/tariff
    let start = Utc::now();
    let horizon = start..start + Duration::days(2);
    let imports = get_import(&OctopusImports::new(settings.clone()), horizon.clone())?;
    let exports = get_export(&OctopusExports::new(settings), horizon, None)?;
    println!("{} import and {} export slots", imports.len(), exports.len());
    Ok(())
}
```

Pass `Some(&AxleForecast)` to `get_export` to include an expected export-event
reward. `AxleEvents::get_event` reads the current event; reward rates are supplied
by the caller because Axle's event endpoint does not provide them. Import events
do not add export rewards. The returned `ExportSlot` exposes the supplier tariff,
the additional reward, and their total. Partial-slot rewards use time overlap
assuming constant export power. Exact event-window planners should use the raw
tariff and event separately to avoid counting rewards twice. These are estimates,
not settled revenue.

Export rates are expanded over the half-hours they cover. Agile Outgoing
publishes one rate per half-hour; flat products such as Outgoing Octopus
(`OUTGOING-VAR-24-10-26`) publish one rate per price, open-ended while current,
so every half-hour in the window gets the flat price and rate changes are
followed automatically. Optionally, `export_fixed_p` replaces the configured
export tariff's published rates with one flat price, without API calls. Use it
for offline runs or a known flat rate during API outages:

```toml
[market]
export_product = "OUTGOING-VAR-24-10-26"
export_tariff = "E-1R-OUTGOING-VAR-24-10-26-F"
export_fixed_p = 12.0 # optional; omit to follow Octopus
```

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

`dispatch::DispatchService` owns Axle event polling and measured reward
accounting. Applications use the provider-neutral `DispatchProvider` contract.
`poll` returns whether the known events changed, so the host plans again;
`next_boundary` gives the next event start or end, where the host plans again
and lets its current command expire. There is no handover and no provider
override: a dispatch event is an export like any other, priced by the planner
with the reward on top of the tariff. The host owns its clock, inverter
connection, command application and shutdown. No hardware commands are sent by
this library.

`forecasts` supplies event windows and configured reward assumptions separately
from supplier tariffs. `active_event` supplies provider metadata for reporting.
`Reading` takes signed grid/battery power, SOC and timestamp freshness; rewards
use measured net grid energy with gaps preserved. A feed outage keeps the last
known event until it ends.

`ProviderConfig::extract` consumes provider sections from a host TOML table and
leaves other sections for the host to validate. Call `validate` before use.
Add this section to the host configuration only when using Axle. Select
Self-Dispatch Mode in the Axle portal first: the member's own controller exports
through events, and Axle only announces and pays them. The former
`self_dispatch` setting is gone; remove it from existing configurations.

```toml
[axle]
enabled = true
api_url = "https://api.axle.energy"
api_key = "YOUR_MEMBER_API_KEY"
# Assumptions, additional to supplier tariffs; not API rates or settled payments.
export_reward_p_per_kwh = 100.0
import_reward_p_per_kwh = 0.0
```

The member API key comes from the Home Assistant section of `vpp.axle.energy`.
Disabled providers require no credentials. Provider settings, legacy dashboard
field names and the `axle-rewards.json` state format are owned here.
`snapshot_fields` returns those reporting fields for the host to include without
knowing their schema. Existing state directories continue to work; the old
`axle-handover.json` and `axle-self-dispatch.json` files are no longer read.

Provider tests live alongside their implementation and cover event changes,
feed outages, restarts and signed reward energy.
