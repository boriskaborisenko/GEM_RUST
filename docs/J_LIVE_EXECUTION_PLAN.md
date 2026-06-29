# Strategy J Live Execution Plan

Status: in progress. The default runtime remains `paper`; live orders require explicit config and secrets.

## Product Understanding

Strategy J already produces explicit paper signals:

- operation: `BUY` or `SELL`
- order type: `Market` or `Limit`
- side: `UP` or `DOWN`
- amount: USD for buys, shares for sells
- price: hard limit / diagnostic price

The live executor must submit those signals to Polymarket without changing Strategy J's decision contour.

## Current Decision

Use the Polymarket V2 deposit-wallet path:

- CLOB host: `https://clob-v2.polymarket.com`
- collateral: pUSD
- signature type: `POLY_1271`
- funder: the deposit wallet that holds pUSD
- signer: owner/session private key used to sign CLOB orders
- relayer: future gasless deploy/approve/redeem worker, not required for normal CLOB order posting

Important: relayer API keys do not replace CLOB order signing. A signer private key is still required for orders.

## MVP Boundary

Phase 1 does:

- load live secrets from ignored `.env.live`
- build live CLOB intents from existing `OrderSignal`
- support `Market -> FOK/FAK` with signal price as hard cap
- support `Limit -> GTC/GTD` with optional post-only
- stale-order guard before submit
- dry-run mode by default
- confirmed matched response parsing

Phase 1 does not:

- change Strategy J logic
- auto-enable live mode
- do relayer redeem automatically
- update paper portfolio from unconfirmed live responses
- implement full open-order heartbeat/cancel lifecycle for long-lived maker orders

## Architecture Plan

```text
strategy_j::process_live_tick()
  -> Vec<OrderSignal>
  -> paper executor when execution.mode = "paper"
  -> live_executor when execution.mode = "live"
      -> LiveOrderIntent
      -> CLOB SDK sign/post
      -> LiveExecutionResult
      -> confirmed-fill accounting only
```

The executor is a separate module. Strategy J must remain the single decision source.

## Execution Config

`config.json` gets an `execution` block with safe defaults:

```json
{
  "execution": {
    "mode": "paper",
    "dryRun": true,
    "secretsFile": ".env.live",
    "clobHost": "https://clob-v2.polymarket.com",
    "signatureType": "poly1271",
    "marketOrderType": "fok",
    "limitOrderType": "gtd",
    "limitPostOnly": false,
    "limitTtlMs": 25000,
    "maxOrderAgeMs": 3000,
    "minOrderUsd": 1.0
  }
}
```

`mode=live` is necessary but not sufficient for real money if `dryRun=true`.

## Secrets Plan

Tracked:

- `.env.live.example`

Ignored:

- `.env`
- `.env.*`
- `secrets/`

Required live values:

- `POLYMARKET_PRIVATE_KEY`: signer private key for CLOB order signatures
- `POLYMARKET_DEPOSIT_WALLET_ADDRESS`: deposit wallet that holds pUSD

Optional future relayer values:

- `POLY_RELAYER_API_KEY`
- `POLY_RELAYER_ADDRESS`

## Backend Plan

1. Add `ExecutionConfig` to `config.rs`.
2. Add `live_executor.rs`.
3. Add dry-run intent planning tests.
4. Add CLOB SDK dependency.
5. Keep live submit isolated until accounting is verified.
6. Add runtime integration only through explicit `execution.mode=live`.

## Testing And Verification

Required before any real order:

- `cargo fmt`
- `cargo test -q`
- dry-run intent build for BUY/SELL and Market/Limit
- auth/balance check using `.env.live`
- one tiny live FOK with `startingBank`/window caps still limiting risk

## Immediate Next Task

Implement the safe live executor module and config/secrets scaffolding. Then wire runtime accounting only after matched-fill parsing is validated.
