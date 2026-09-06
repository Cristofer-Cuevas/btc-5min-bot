# BTC 5-minute bot: audit and research build

**No profitable strategy has been established. Live taker entry now requires an explicit `TAKER_ENABLED=true`; the default collects data without placing orders.** This is a development safeguard, not evidence of an improved return. Existing running deployments are not changed by editing this checkout.

The September 5 database contains 214 live trades. Its 70 TWAP-enabled trades lost **$210.62 on $1,206.70** of recorded cost, before reconstructing actual exchange fees. Across all periods, recorded profit is $152.94, but one unresolved trade carries $148.50 cost and the historical ledger has no explicit fee reconciliation. A high win rate did not establish favorable entry prices.

Read the reproducible [profitability audit](reports/audit_profitability.md), [machine-readable results](reports/audit_profitability.json), and [fair-value validation](reports/fairvalue_validation.json). The source `t.db` and existing `fairvalue.json` were not rewritten by the audit.

## What changed

- Current nested CLOB `price_changes` events now update both price ladders, including cancellations. Disconnects invalidate books; fresh snapshots are required before using incremental data. Binance and RTDS validate timestamps and reset stale state. Taker entries reject stale or invalid data.
- Retry attempts run the full strategy again. An old direction or price cannot bypass the current gates. Tick rounding respects the actual market tick and the configured price cap.
- Live buys obtain per-market fee metadata and enforce an all-in cost cap. New trade costs include a conservative `fee_estimate_usdc`, explicitly distinguished from a reconciled debit. Only consistent, positive matched acknowledgements count as fills. Delayed or ambiguous responses block further live entry.
- A SQLite `order_attempts` journal reserves exposure before submission. Unknown orders survive restarts. Pre-trade checks include open positions, daily remaining loss budget, consecutive losses, stale unresolved positions and duplicate windows. Default caps are $5 per trade and $10 open cost; paper trades are excluded from live statistics. `/resume` cannot erase durable exposure or loss limits. Live `/testorder` was disabled because it bypassed the position ledger.
- Maker crossings are no longer called passive fills. Quotes that cross the contemporaneous book are rejected from post-only diagnostics. Both outcome tokens are measured independently of taker pause state and use the model's declared input feature.
- Fair-value fitting defaults to unconditional observations. Repeated observations from one five-minute outcome do not manufacture sample confidence. The exported model uses only the chronological training prefix, with whole-window holdout, embargo and observed resolution-arrival checks when available. Thin cells and thin interpolation neighbors cause abstention.
- Optional raw CLOB capture stores snapshots, price changes and trade events with receipt times in `market_events`. A bounded writer queue avoids disk I/O on the feed task; explicit gap markers identify overflow or failed writes. This is input for future maker replay, not a fill simulator.

## Reproduce the research

No credentials or network access are needed for these commands:

```powershell
python tools/audit_profitability.py --self-test
python -m unittest discover -s tools -p "test_*.py" -v
python tools/audit_profitability.py --db t.db --out reports/audit_profitability
python tools/build_fairvalue.py --db t.db --out reports/fairvalue_candidate.json --validation-out reports/fairvalue_validation.json --report
cargo test
```

The frozen eight-candidate experiment found no positive candidate with sufficient validation trades. The best supported validation candidate lost on the later test as well: **-$0.9693 across 15 hypothetical one-share trades**, assuming the stated fee scenario and one cent slippage. Those are synthetic results, not executable fills or actual wallet returns. Only 167 labeled unconditional windows were available, and about 65% of their rows lacked at least one book field.

The separate fair-value model had Brier loss **0.2191**, versus **0.1439** for the market midpoint on the same held-out observations; lower is better. Improving on a 50/50 forecast is insufficient when the competing market price is more informative. Neither a maker rebate nor a wide quote fixes a mispriced probability model.

Validation completed: **132 Rust tests passed, including the two normally ignored integration checks**, eight Python unit tests passed, and the audit self-tests passed. The migration was run against an isolated SQLite backup, and every original trade field was compared with the source. See [test output](reports/rust-tests.txt) and [verification details](reports/verification.json).

On this Windows machine, the pre-existing MinGW/Rust pthread mismatch required a local test linker workaround; plain `cargo test` did not pass unchanged. The workaround used a copied pthread library under ignored `target/test-native` and a 16 MiB executable stack. The verification file records it; no global toolchain configuration was changed.

## Collect useful forward data

Keep existing credentials private. Merge the settings in [.env.example](.env.example) into the runtime environment rather than replacing a configured credential file. The main collection settings are:

```dotenv
TAKER_ENABLED=false
DRY_RUN=true
CAPTURE_MARKET_EVENTS=true
MAKER_MODE=off
DB_PATH=trades.db
```

The bot still needs its configured Telegram connection and public feeds when run. `MAKER_MODE=shadow` additionally loads `FAIRVALUE_PATH`; use the generated candidate only as a diagnostic model. `MAKER_MODE=live` remains unsupported. `TAKER_ENABLED=true` with `DRY_RUN=true` enables experimental paper taker entries, charged at the configured limit plus an estimated current crypto fee. Paper execution has no measured queue or network latency and is not a live-profitability test.

Use a fresh collection database or a backup of an existing operational database; preserve `t.db` as the audit snapshot. Raw tape can grow quickly: monitor disk space and archive it explicitly. Exclude disconnects, capture gaps and the unflushed tail after a crash from replay until a new full snapshot is available. Public depth and trade data still cannot reveal exact private queue position.

## Work required before another live strategy

1. Reconcile trade 143 (`window_ts=1785941700`, cost $148.50), historical fees and payouts against actual wallet/exchange activity. The public Gamma lookup from this environment returned HTTP 403, so its outcome was not guessed. Confirmed market resolution alone does not prove a wallet credit or redemption.
2. Collect several market regimes with corrected book updates and full event tape. Evaluate a model of residual value relative to the market price, using only information available at each decision; preserve an untouched future test period after choosing parameters.
3. Implement stateful maker replay with post-only arrival checks, conservative queue depletion by actual opposing trades, measured placement/cancel delay, partial fills, owned inventory, executable exits, and markouts after fills. Treat queue assumptions as uncertainty bounds. Do not count a same-tick crossing as a maker fill or sell inventory the simulation never acquired.
4. Require positive out-of-sample net returns with an adequate number of independent trades, a positive block confidence bound, and robustness to worse fees/slippage/latency. Repeatedly tuning on the supplied September 5 test converts it into training data.
5. Implement exchange reconciliation for the durable order journal before unattended live deployment. Rows left `submitted` or `unknown` deliberately block entry; resolve them only using verified order/trade/balance evidence, not a restart or a blind SQL deletion. Current acknowledgements and conservative fees are not final on-chain accounting.

The risk caps bound intended spending; they do not create an edge. No live orders, wallet transactions, bot deployment, or credential changes were performed during this audit.

## Exchange references checked

- [Polymarket fees](https://docs.polymarket.com/trading/fees): query per-market fee parameters; current crypto schedule uses `shares * 0.07 * p * (1-p)`.
- [Order lifecycle](https://docs.polymarket.com/concepts/order-lifecycle) and [place orders](https://docs.polymarket.com/trading/place-orders): post-only crossing rejection and acknowledgement states.
- [Real-time market data](https://docs.polymarket.com/market-data/realtime-data): current incremental book schema and application heartbeat.
- [V2 exchange settlement implementation](https://github.com/Polymarket/ctf-exchange-v2/blob/main/src/exchange/mixins/Trading.sol): buy fees are additional collateral debits. Historical rows are not retroactively assumed to use this exact implementation.
