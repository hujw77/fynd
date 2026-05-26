---
icon: coins
---

# Client Fees


Every swap through the TychoRouter incurs two fees, deducted from the swap output:

* **Router fee**: 10 bps (0.1%) on the swap output, always applied.
* **Client fee**: optional integrator fee via `ClientFeeParams`. The router takes a 20% share; the integrator keeps 80%.

When you request encoding, the quote response includes a `fee_breakdown` with the exact amounts.

## Fee breakdown

`amount_out` in the quote response is the raw pre-fee swap output — the amount produced by the route before any fees are deducted. It is **not** what the user receives. The amount the user receives on-chain is at least `fee_breakdown.min_amount_received`.

The on-chain `FeeCalculator` deducts fees from `amount_out`. Fynd mirrors this calculation (identical integer arithmetic) to set `minAmountOut` in the encoded transaction.

Given `amount_out` (raw swap output), `client_fee_bps` (0 if none), and `slippage`:

```
1. client_fee        = amount_out * client_fee_bps / 10,000
2. router_share      = amount_out * client_fee_bps * 2,000 / 100,000,000
3. client_portion    = client_fee - router_share
4. router_fee_output = amount_out * 10 / 10,000
5. router_fee        = router_share + router_fee_output
6. amount_after_fees = amount_out - client_portion - router_fee
7. max_slippage      = amount_after_fees * slippage
8. min_amount_received = amount_after_fees - max_slippage
```

The response fields (all absolute values in output token units):

| Field                 | Description                                                   |
| --------------------- | ------------------------------------------------------------- |
| `router_fee`          | Router's total take (output fee + 20% of client fee)          |
| `client_fee`          | Integrator's portion (80% of the client fee)                  |
| `max_slippage`        | Slippage allowance on the post-fee amount                     |
| `min_amount_received` | On-chain minimum the user receives (`minAmountOut` in the tx) |

Invariant: `amount_out = router_fee + client_fee + max_slippage + min_amount_received`

### Example

1,000,000 USDC output, 50 bps client fee, 1% slippage:

```
client_fee (total)   = 1,000,000 * 50 / 10,000         = 5,000
router_share         = 1,000,000 * 50 * 2,000 / 1e8    = 1,000
client_portion       = 5,000 - 1,000                    = 4,000
router_fee_output    = 1,000,000 * 10 / 10,000          = 1,000
router_fee           = 1,000 + 1,000                     = 2,000
amount_after_fees    = 1,000,000 - 4,000 - 2,000        = 994,000
max_slippage         = 994,000 * 0.01                    = 9,940
min_amount_received  = 994,000 - 9,940                   = 984,060
```

## Setting up client fees

1. Choose a fee in basis points (e.g. `50` = 0.5%), a receiver address, and a `maxClientContribution`.
2. The fee receiver signs an EIP-712 `ClientFee` message authorizing these parameters.
3. Attach the signed params to `EncodingOptions.clientFeeParams`.
4. The router verifies the signature on-chain and deducts the fee. Fees go to the receiver's vault balance.

No `ClientFeeParams`? No client fee. The 10 bps router fee still applies.

### maxClientContribution

The maximum amount (in output token units) the client will subsidize from their vault balance if slippage pushes the output below `minAmountOut`. If the shortfall exceeds this limit, the transaction reverts.

Set to `0` to collect fees without covering slippage losses. This is the common case.

See [Tycho encoding docs](https://docs.propellerheads.xyz/tycho/for-solvers/execution/encoding#encode) for vault details.

## EIP-712 signing

The fee receiver signs a typed data hash:

| Field                   | Type      | Description                       |
| ----------------------- | --------- | --------------------------------- |
| `clientFeeBps`          | `uint16`  | Fee in basis points (0-10,000)    |
| `clientFeeReceiver`     | `address` | Address receiving the fee         |
| `maxClientContribution` | `uint256` | Maximum subsidy from client vault |
| `deadline`              | `uint256` | Signature expiry (Unix timestamp) |

**EIP-712 domain:**

| Field               | Value                        |
| ------------------- | ---------------------------- |
| `name`              | `TychoRouter`                |
| `version`           | `1`                          |
| `chainId`           | Target chain ID              |
| `verifyingContract` | TychoRouter contract address |

## Code examples

{% tabs %}
{% tab title="TypeScript" %}
```typescript
// Build fee params (without signature).
const feeParams: ClientFeeParams = {
    bps: 50,              // 0.5% fee
    receiver: feeReceiver,
    maxContribution: 0n,  // no vault subsidy
    deadline: 1893456000, // Unix timestamp
};

// Compute the EIP-712 hash and sign with the fee receiver's wallet.
const hash = clientFeeSigningHash(feeParams, 1, routerAddress);
const signature = await account.signMessage({message: {raw: hash}});

// Attach signature and wire into encoding options.
const opts = withClientFee(encodingOptions(0.005), {...feeParams, signature});
```
{% endtab %}

{% tab title="Rust" %}
```rust
    // Step 1: request a quote using unsigned client fee params.
    // The server encodes the full calldata and returns `swaps_hash` +
    // `signature_offset` in the fee breakdown so the client can patch
    // the real signature in.
    let fee = ClientFeeParams::new(
        FEE_BPS,
        Bytes::copy_from_slice(fee_receiver.as_slice()),
        BigUint::ZERO,
        u64::MAX,
    );
    let order = Order::new(
        Bytes::copy_from_slice(sell_token.as_slice()),
        Bytes::copy_from_slice(buy_token.as_slice()),
        BigUint::from(SELL_AMOUNT),
        OrderSide::Sell,
        Bytes::copy_from_slice(sender.as_slice()),
        None,
    );
    let quote = client
        .quote(QuoteParams::new(
            order,
            QuoteOptions::default()
                .with_timeout_ms(5_000)
                .with_encoding_options(EncodingOptions::new(SLIPPAGE).with_client_fee(fee.clone())),
        ))
        .await?;

    let fee_breakdown = quote
        .fee_breakdown()
        .ok_or("no fee breakdown in quote")?;
    let swaps_hash = fee_breakdown
        .swaps_hash()
        .ok_or("no swaps_hash — server must support client fee signing")?;

    // Step 2: sign the full 10-field EIP-712 ClientFee hash.
    // receiver defaults to sender when the order has no explicit receiver.
    let hash = fee.eip712_signing_hash(
        chain_id,
        &router_address,
        quote.amount_in(),
        &Bytes::copy_from_slice(sell_token.as_slice()),
        &Bytes::copy_from_slice(buy_token.as_slice()),
        fee_breakdown.min_amount_received(),
        &Bytes::copy_from_slice(sender.as_slice()),
        swaps_hash,
    )?;
    let sig = fee_signer
        .sign_hash(&B256::from(hash))
        .await?;

    // Step 3: patch the real signature into the calldata.
    let quote = quote.with_client_fee_signature(&sig.as_bytes()[..])?;
```

See the full working example: [`clients/rust/examples/swap_client_fee.rs`](../../clients/rust/examples/swap_client_fee.rs)
{% endtab %}
{% endtabs %}
