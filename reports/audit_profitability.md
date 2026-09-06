# Profitability audit

This audit found no defensible basis to promote a strategy to live trading. It reads SQLite in read-only mode and places no orders.

The database records 214 live trades, 213 resolved, with $152.94 recorded PnL. This is not a wallet-reconciled net result. One or more pending trades carry $148.50 cost. Recorded peak-to-trough drawdown is $227.10.

The TWAP-enabled period has 70 trades and $-210.62 recorded PnL on $1206.70 cost. Changes in sizing and market regime confound comparisons; the earlier period is not evidence that reverting will profit.

## Data and maker evidence

The unconditional sample contains 9,875 rows but only 167 labeled five-minute windows, from 2026-09-05T05:15:00+00:00 to 2026-09-05T19:15:00+00:00. 6,387 rows lack at least one book field; 4,893 have neither ask.

There are 4,830 labeled bid-crossing rows. Their winning fraction is 42.13%, versus a mean model probability of 69.23% and a mean quoted bid of $0.6475. Outcome minus quoted bid averages $-0.2261 per synthetic share before costs. This is an adverse-selection diagnostic, not realized or executable maker PnL.

would_bid_fill compares this tick's proposed bid with this tick's ask. These are immediate crossings, not evidence of a previously resting post-only order filling. Repeated rows cannot be counted as independent trades. No ask-sale profit is claimed without owned inventory.

## Chronological experiment

Eight fixed candidate rules combine 120/60 seconds remaining, TWAP/spot delta, and either an absolute 0.05% direction threshold or a train-only calibrated probability with at least 0.03 estimated edge. The calibrated models use Laplace smoothing, monotonic pooling and at least ten independent training windows in the input cell. Training scores are explicitly in-sample.

The first scheduled sample within ten seconds after each horizon is the only decision. Missing data causes abstention. Buy prices are the selected side's observed ask plus the stated slippage and fee scenario. There is one hypothetical share and at most one trade per window. These rules do not replay the live strategy's full historical configuration.

The chronological split is 60% training, 20% validation and 20% test by complete window. Training labels must have arrived before validation starts. Validation labels not known at the test boundary are embargoed. Validation selects by average PnL per window, requiring twenty trades and positive validation PnL. Test outcomes never choose the candidate or fit the parameters. Under-supported validation leaders are diagnostic only.

| Partition | Windows | First UTC | Last UTC |
|---|---:|---|---|
| train | 100 | 2026-09-05T05:15:00+00:00 | 2026-09-05T13:30:00+00:00 |
| validation | 32 | 2026-09-05T13:35:00+00:00 | 2026-09-05T16:10:00+00:00 |
| test | 34 | 2026-09-05T16:20:00+00:00 | 2026-09-05T19:05:00+00:00 |

| Candidate | Train trades | Train PnL* | Validation trades | Validation PnL* |
|---|---:|---:|---:|---:|
| direction_twap_120s | 0 | $0.0000 | 0 | $0.0000 |
| calibrated_twap_120s | 65 | $-0.7720 | 20 | $-1.5780 |
| direction_spot_120s | 0 | $0.0000 | 0 | $0.0000 |
| calibrated_spot_120s | 60 | $1.4823 | 18 | $-3.2324 |
| direction_twap_60s | 0 | $0.0000 | 0 | $0.0000 |
| calibrated_twap_60s | 31 | $1.6326 | 10 | $-0.0850 |
| direction_spot_60s | 0 | $0.0000 | 1 | $0.6746 |
| calibrated_spot_60s | 28 | $-1.3259 | 9 | $0.0187 |

*Synthetic one-share results; assumed per-share fee = 0.072 × [p(1−p)]^1.0, plus $0.010 slippage. This scenario does not reconstruct actual historic fees or claim a quote could be filled. No maker rebates are assumed.

Selected candidate: **none — insufficient validation trades**. Diagnostic validation leader: `calibrated_twap_120s`. Its frozen test has 15 trades across 34 windows and $-0.9741 synthetic PnL. Its 95% moving-block bootstrap interval for PnL per window is [-0.07667535647058824, 0.01869101117647058]. Blocks contain twelve consecutive windows, with no-trade windows retained; the interval measures sampling variation in this short period only.

| Same frozen test entries | Synthetic PnL | Synthetic ROI |
|---|---:|---:|
| slippage_0.000 | $-0.8201 | -17.01% |
| slippage_0.010 | $-0.9741 | -19.58% |
| slippage_0.020 | $-1.1279 | -22.00% |

All stress rows use the same entries; the rule is not reselected when costs change. At least 100 test trades and a positive lower confidence bound would be needed even for the statistical gate. Execution verification and multiple future market regimes remain necessary regardless of that gate.

## Reproduce and interpret

```powershell
python tools/audit_profitability.py --db t.db --out reports/audit_profitability
python tools/audit_profitability.py --self-test
```

The adjacent JSON contains full metrics, monthly and version breakdowns, unresolved trade identifiers, data coverage, model sample counts, and calibration comparisons. No historical winning subset is promoted. New test periods must be collected after freezing a candidate; repeatedly tuning on this report turns its test into training data.
