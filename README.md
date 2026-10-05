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
reward. `AxleEvents::get_event` reads the current event. Axle's event endpoint
does not supply rates, so `axle::reward_p_per_kwh` gives the published
Self-Dispatch rate: 100p/kWh for export events and nothing for import events. The returned `ExportSlot` exposes the supplier tariff,
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

`dispatch::DispatchService` owns Axle event polling, self-dispatch policy, retry
timing and measured reward accounting. Axle events are always self-dispatched:
the host keeps control and applies the export or import itself; control is never
handed to Axle. Applications use the provider-neutral `DispatchProvider`
contract. `poll` returns schedule updates; `control` returns `Override` or
`Tariff`. The host owns its clock, inverter connection, command application and
shutdown. Pass actual write outcomes to `record_attempt`; use `permits_shutdown`
before sending a shutdown reset. No hardware commands are sent by this library.

`forecasts` supplies event windows and reward assumptions separately from
supplier tariffs. `active_event` supplies provider metadata for reporting.
`Reading` takes signed grid/battery power, SOC and timestamp freshness; rewards
use measured net grid energy with gaps preserved. Short leases, feed-failure
limits and SOC latches retain the original policy.

`ProviderConfig::extract` consumes provider sections from a host TOML table and
leaves other sections for the host to validate. Call `validate` before use.
Add this section to the host configuration only when using Axle, after selecting
Self-Dispatch Mode in the Axle portal:

```toml
[axle]
enabled = true
api_key = "YOUR_MEMBER_API_KEY"
```

The member API key comes from the Home Assistant section of `vpp.axle.energy`.
Disabled providers require no credentials. Older `api_url`, `self_dispatch` and
reward-rate settings have been removed; delete them from existing configurations.
Provider settings, legacy dashboard field names and the `axle-self-dispatch.json`
and `axle-rewards.json` state formats are owned here. An old `axle-handover.json` is
no longer read. `snapshot_fields` returns those reporting fields for the host to
include without knowing their schema. Existing state directories continue to
work without moving or resetting their files.

Provider tests live alongside their implementation and cover exact boundaries,
restarts, cancellations, expired feeds, bounded retries and signed reward energy.
