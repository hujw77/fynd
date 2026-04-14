---
icon: code
---

# Encoding Options

When you request a quote, you can include `encoding_options` to have Fynd encode the swap into a
ready-to-submit transaction. Without encoding options, you get a quote only (price, route, gas
estimate) but no transaction.

For full details on how the TychoRouter contract works, see the
[Tycho execution docs](https://docs.propellerheads.xyz/tycho/for-solvers/execution).

## Fields

| Field               | Type              | Required | Default           | Description                                                                                                     |
|---------------------|-------------------|----------|-------------------|-----------------------------------------------------------------------------------------------------------------|
| `slippage`          | `string`          | yes      | —                 | Slippage tolerance as a decimal string (e.g. `"0.005"` = 0.5%). Applied to the quoted output to compute `minAmountOut`. |
| `transfer_type`     | `string`          | no       | `"transfer_from"` | How the router receives your input tokens. See [transfer types](#transfer-types).                               |
| `permit`            | `PermitSingle`    | no       | —                 | Permit2 authorization. Required when `transfer_type` is `"transfer_from_permit2"`.                              |
| `permit2_signature` | `string`          | no       | —                 | Hex-encoded 65-byte signature over the permit. Required when `permit` is set.                                   |
| `client_fee_params` | `ClientFeeParams` | no       | —                 | Optional integrator fee. See the [client fees guide](client-fees.md).                                           |
| `price_guard`       | `PriceGuardConfig`| no       | —                 | Per-request overrides for price-guard validation. See [price guard](#price-guard).                              |

## Transfer types

The `transfer_type` field controls how the TychoRouter contract receives your input tokens. For a
deeper explanation see the
[Tycho execution docs](https://docs.propellerheads.xyz/tycho/for-solvers/execution).

### `transfer_from` (default)

Standard ERC-20 approval flow. Before submitting the transaction, the sender must have called
`approve()` on the input token granting the TychoRouter contract a sufficient allowance.

### `transfer_from_permit2`

Uses Uniswap's [Permit2](https://docs.propellerheads.xyz/tycho/for-solvers/execution) contract for
gasless approvals. The sender signs a `PermitSingle` off-chain and passes it along with the
signature in the quote request. No on-chain `approve()` needed (but the token must be approved to
the Permit2 contract).

When using this transfer type, both `permit` and `permit2_signature` are required.

### `use_vaults_funds`

Draws tokens from the sender's vault balance in the TychoRouter contract (ERC-6909). No approval or
permit needed — tokens must have been deposited into the vault beforehand. See the
[vault mechanism](https://docs.propellerheads.xyz/tycho/for-solvers/execution) in the Tycho docs.

## Price guard

The price guard validates each successful quote's `amount_out` against external price providers
(Hyperliquid and Binance by default). It runs after solving and before the response is returned.
Providers are queried concurrently, and a quote passes if **at least one** provider returns a price
within tolerance. When validation fails, the quote's status is set to `price_check_failed`; other
orders in the same batch are unaffected.

The guard is disabled by default. Operators enable it server-side with `--enable-price-guard`
(see [server configuration](server-configuration.md)); tolerances and fallback behavior can be
tuned via the matching `--price-guard-*` flags. When enabled without further overrides, the server
uses the defaults shown in the table below.

Clients can override the configuration per request through `encoding_options.price_guard`. When
present, the request config **replaces** the server config entirely — any field omitted from the
request falls back to the default in the table below, not to the server's configured value. Send
every field whose server default you do not want.

### Fields

| Field                            | Type      | Default | Description                                                                                                                   |
|----------------------------------|-----------|---------|-------------------------------------------------------------------------------------------------------------------------------|
| `enabled`                        | `boolean` | `false` | Turns the guard on or off. When off, all other fields are ignored and every quote passes through unchecked.                   |
| `lower_tolerance_bps`            | `integer` | `300`   | Max allowed deviation in basis points when the quote's `amount_out` is below the provider's expected amount out.              |
| `upper_tolerance_bps`            | `integer` | `10000` | Max allowed deviation in basis points when the quote's `amount_out` is above the provider's expected amount out.              |
| `fail_on_provider_error`         | `boolean` | `false` | See [fallback behavior](#fallback-behavior).                                                                                  |
| `fail_on_token_price_not_found`  | `boolean` | `false` | See [fallback behavior](#fallback-behavior).                                                                                  |

### Tolerance

The quote's `amount_out` is compared to each provider's expected amount out and the check
short-circuits as soon as one provider validates within tolerance — the remaining providers are
not consulted. If `amount_out` is below the expected amount, the deviation is checked against
`lower_tolerance_bps`; otherwise against `upper_tolerance_bps`. The two bounds are separate so
operators can be strict on under-delivery while leaving headroom for over-delivery that may
indicate a pricing error in Fynd rather than a genuine arbitrage. The default `upper_tolerance_bps`
of `10000` (100%) allows `amount_out` up to twice the expected amount before rejecting; lower it
to catch suspicious outputs.

### Fallback behavior

The fallback flags apply only when every provider response is an error or "not found" — in other
words, no provider returned a price at all. If any provider returns an in-tolerance price, the
quote passes regardless of these flags; if any provider returns an out-of-tolerance price and
none passes, the quote is rejected regardless of these flags.

- `fail_on_provider_error`: all providers failed with an infrastructure error (network issue, API
  down, rate-limited). `false` (default) lets the quote pass; `true` rejects it.
- `fail_on_token_price_not_found`: every provider was reached but no provider lists the token
  (it is not traded on that venue). `false` (default) lets the quote pass; `true` rejects it.

When responses are mixed — some `price_not_found`, some infrastructure errors — the guard treats
the pair as potentially supported but unreachable and falls back to `fail_on_provider_error`.

### Example

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

## Slippage

The `slippage` value is a decimal fraction:

| Value     | Meaning |
|-----------|---------|
| `"0.001"` | 0.1%    |
| `"0.005"` | 0.5%    |
| `"0.01"`  | 1%      |

Fynd computes `minAmountOut = quotedAmountOut * (1 - slippage)` and encodes it into the transaction.
If on-chain execution produces less than `minAmountOut`, the transaction reverts.

Typical values are `0.005` (0.5%) for stablecoin pairs and `0.01` (1%) for volatile pairs.

## The response transaction

When encoding options are present and the quote succeeds, the response includes a `transaction`
object:

| Field   | Type     | Description                                                                                                                                 |
|---------|----------|---------------------------------------------------------------------------------------------------------------------------------------------|
| `to`    | `string` | The TychoRouter contract address. See [contract addresses](https://docs.propellerheads.xyz/tycho/for-solvers/execution/contract-addresses). |
| `value` | `string` | Native token value (wei). Non-zero only when the input token is the native token.                                                           |
| `data`  | `string` | Hex-encoded calldata. Submit this as the `data` field of your Ethereum transaction.                                                         |

Use `to`, `value`, and `data` directly in your transaction. Set `from` to the sender address from
your order, choose a gas limit (the quote's `gas_estimate` is a good starting point), and submit.
