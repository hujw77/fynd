---
icon: shield-check
---

# Price Guard

The price guard validates each successful quote's `amount_out` against external price providers
(Hyperliquid and Binance by default), catching mispriced quotes before they are encoded into
transactions and submitted on-chain. Providers are queried concurrently, and a quote passes if
**at least one** provider returns a price within tolerance. When validation fails, the quote's
status is set to `price_check_failed`; other orders in the same batch are unaffected.

Each provider runs a background worker that continuously populates an in-memory cache. On the
solve path, `get_expected_out()` reads from cache — no blocking API calls during solving.

## Enabling the price guard

The guard is disabled by default. Users enable it server-side with `--enable-price-guard`
(see [server configuration](server-configuration.md)); tolerances and fallback behavior can be
tuned via the matching `--price-guard-*` flags. When enabled without further overrides, the server
uses the defaults shown in the table below.

Clients can override the configuration per request through `encoding_options.price_guard`
(see [encoding options](encoding-options.md)). When present, the request config **replaces** the
server config entirely — any field omitted from the request falls back to the default in the table
below, not to the server's configured value. Set every field whose server default you do not want.

## Fields

| Field                            | Type      | Default | Description                                                                                                                   |
|----------------------------------|-----------|---------|-------------------------------------------------------------------------------------------------------------------------------|
| `enabled`                        | `boolean` | `false` | Turns the guard on or off. When off, all other fields are ignored and every quote passes through unchecked.                   |
| `lower_tolerance_bps`            | `integer` | `300`   | Max allowed deviation in basis points when the quote's `amount_out` is below the provider's expected amount out.              |
| `upper_tolerance_bps`            | `integer` | `10000` | Max allowed deviation in basis points when the quote's `amount_out` is above the provider's expected amount out.              |
| `fail_on_provider_error`         | `boolean` | `false` | See [fallback behavior](#fallback-behavior).                                                                                  |
| `fail_on_token_price_not_found`  | `boolean` | `false` | See [fallback behavior](#fallback-behavior).                                                                                  |

## Tolerance

The quote's `amount_out` is compared to each provider's expected amount out and the check
short-circuits as soon as one provider validates within tolerance — the remaining providers are
not consulted.

For both directions, deviation is computed the same way:

```
deviation_bps = abs(expected - actual) * 10000 / expected
```

- `amount_out < expected` → reject if deviation exceeds `lower_tolerance_bps`
- `amount_out >= expected` → reject if deviation exceeds `upper_tolerance_bps`

By default the lower bound is much stricter (`300` bps = 3%) than the upper bound (`10000` bps =
100%). The lower bound catches under-delivery — the user getting less than expected. The upper
bound catches cases where Fynd's price is significantly better than the external price, which may
indicate a pricing bug that would fail to land on-chain. The default of `10000` allows `amount_out`
up to twice the expected amount; lower it to catch suspicious outputs.

## Fallback behavior

The fallback flags apply only when every provider response is an error or "not found" — in other
words, no provider returned a price at all. If any provider returns an in-tolerance price, the
quote passes regardless of these flags; if any provider returns a price but none are within
tolerance, the quote is rejected regardless of these flags.

- **`fail_on_provider_error`** — controls behavior when all providers fail with an infrastructure
  error (network issue, API down, rate-limited). `false` (default) lets the quote pass; `true`
  rejects it.
- **`fail_on_token_price_not_found`** — controls behavior when every provider was reached but no
  provider lists the token (it is not traded on that venue). `false` (default) lets the quote
  pass; `true` rejects it.

When responses are mixed — some `price_not_found`, some infrastructure errors — the guard treats
the pair as potentially supported but unreachable and falls back to `fail_on_provider_error`.

## Custom providers

The `PriceProvider` trait, `ExternalPrice`, and `PriceProviderError` are public and re-exported
from `fynd-core`. Implement the trait to add your own price source and register it via
`FyndBuilder::register_price_provider()`:

```rust
let solver = FyndBuilder::new(chain, tycho_url, rpc_url, protocols, min_tvl)
    .register_price_provider(Box::new(MyCustomProvider::new()))
    .price_guard_config(
        PriceGuardConfig::default().with_enabled(true),
    )
    .build()
    .await?;
```

Providers follow a worker+cache pattern: `start()` spawns a background task that populates an
in-memory cache, and `get_expected_out()` reads from that cache synchronously without blocking or
making network calls.

If no providers are registered before `build()`, the two built-in providers (Hyperliquid, Binance)
are added automatically. Calling `register_price_provider()` before `build()` skips the defaults —
register only what you need.

## Example

Enable the price guard with a tight lower bound and fail-closed on unknown tokens:

```json
{
  "orders": [
    {
      "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
      "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "amount": "1000000000000000000",
      "side": "sell",
      "sender": "0x0000000000000000000000000000000000000001"
    }
  ],
  "options": {
    "encoding_options": {
      "price_guard": {
        "enabled": true,
        "lower_tolerance_bps": 100,
        "upper_tolerance_bps": 5000,
        "fail_on_token_price_not_found": true
      }
    }
  }
}
```

To disable the price guard on a server that has it enabled, set `price_guard` inside
`encoding_options`:

```json
"encoding_options": {
  "price_guard": { "enabled": false }
}
```
