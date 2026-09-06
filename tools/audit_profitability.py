#!/usr/bin/env python3
"""Read-only audit and deliberately small chronological strategy experiment.

No third-party packages, orders, model deployment, or database writes. Run:
  python tools/audit_profitability.py --db t.db --out reports/audit_profitability
  python tools/audit_profitability.py --self-test

The eight candidate definitions below are fixed in code. All rows from a window
remain in one chronological partition. Parameters use training outcomes only;
validation selects one candidate; the final test is reported without refitting.
The snapshots cannot establish executable performance or true maker fills.
"""
from __future__ import annotations

import argparse
import bisect
import collections
import datetime as dt
import hashlib
import json
import math
from pathlib import Path
import random
import sqlite3
import statistics


DELTA_EDGES = [-0.10, -0.05, -0.02, 0.0, 0.02, 0.05, 0.10]
HORIZONS = (120, 60)
FEATURES = ("twap_delta_pct", "spot_delta_pct")
MIN_CELL_WINDOWS = 10
MIN_VALIDATION_TRADES = 20
MIN_TEST_TRADES = 100


def utc(seconds):
    return dt.datetime.fromtimestamp(seconds, dt.timezone.utc).isoformat()


def mean(values):
    return statistics.mean(values) if values else None


def quantile(values, fraction):
    if not values:
        return None
    values = sorted(values)
    position = (len(values) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(values) - 1)
    return values[lower] + (values[upper] - values[lower]) * (position - lower)


def wilson(wins, count):
    if not count:
        return None
    z = 1.959963984540054
    p = wins / count
    denominator = 1 + z * z / count
    middle = (p + z * z / (2 * count)) / denominator
    radius = z * math.sqrt(p * (1 - p) / count + z * z / (4 * count * count)) / denominator
    return [middle - radius, middle + radius]


def block_mean_interval(values, block_size=12, replicates=2000):
    """Circular moving-block bootstrap; default block is one hour of windows.

    The unit is a chronological window, including no-trade windows, not ticks.
    This quantifies sampling noise only and does not certify future returns.
    """
    if len(values) < 2:
        return None
    rng = random.Random(20260905)
    size = len(values)
    length = min(block_size, size)
    draws = []
    for _ in range(replicates):
        sample = []
        while len(sample) < size:
            start = rng.randrange(size)
            sample.extend(values[(start + j) % size] for j in range(length))
        draws.append(statistics.mean(sample[:size]))
    return [quantile(draws, 0.025), quantile(draws, 0.975)]


def trade_summary(rows):
    resolved = [r for r in rows if r["profit"] is not None]
    equity = peak = drawdown = 0.0
    for row in resolved:
        equity += row["profit"]
        peak = max(peak, equity)
        drawdown = max(drawdown, peak - equity)
    cost = sum(r["cost_usdc"] for r in resolved)
    wins = sum(r["won"] == 1 for r in resolved)
    return {
        "trades": len(rows), "resolved": len(resolved), "wins": wins,
        "win_rate": wins / len(resolved) if resolved else None,
        "win_rate_wilson_95": wilson(wins, len(resolved)),
        "resolved_cost_usdc": cost,
        "unresolved_cost_usdc": sum(r["cost_usdc"] for r in rows if r["profit"] is None),
        "recorded_pnl_usdc": equity,
        "recorded_roi": equity / cost if cost else None,
        "equal_dollar_mean_return": mean([r["profit"] / r["cost_usdc"] for r in resolved if r["cost_usdc"] > 0]),
        "recorded_max_drawdown_usdc": drawdown,
        "mean_entry_price": mean([r["entry_price"] for r in resolved]),
        "first_utc": utc(rows[0]["timestamp"]) if rows else None,
        "last_utc": utc(rows[-1]["timestamp"]) if rows else None,
    }


def grouped_trades(rows, key):
    groups = collections.defaultdict(list)
    for row in rows:
        groups[str(key(row))].append(row)
    return {name: trade_summary(group) for name, group in sorted(groups.items())}


def snapshot(rows, horizon):
    """First scheduled sample after the horizon, within ten seconds.

    Missing features/books on that sample cause abstention. Never search later
    ticks for a favorable ask or choose a tick using the eventual outcome.
    """
    return next((r for r in rows if horizon - 10 <= r["secs_left"] <= horizon), None)


def isotonic(values, weights):
    blocks = []
    for value, weight in zip(values, weights):
        blocks.append([value, weight, 1])
        while len(blocks) > 1 and blocks[-2][0] > blocks[-1][0]:
            b = blocks.pop()
            a = blocks.pop()
            weight = a[1] + b[1]
            blocks.append([(a[0] * a[1] + b[0] * b[1]) / weight, weight, a[2] + b[2]])
    return [value for value, _, length in blocks for _ in range(length)]


def fit_model(windows, rows_by_window, resolutions, horizon, feature, available_before_ms):
    counts = [0] * (len(DELTA_EDGES) + 1)
    wins = [0] * len(counts)
    usable = []
    for window in windows:
        row = snapshot(rows_by_window[window], horizon)
        resolution = resolutions.get(window)
        if row is None or row[feature] is None or resolution is None:
            continue
        # A training label must have actually arrived before validation begins.
        if resolution["resolution_event_ms"] is None or resolution["resolution_event_ms"] >= available_before_ms:
            continue
        if resolution["actual_resolution"] != row["actual_resolution"]:
            continue
        index = bisect.bisect_right(DELTA_EDGES, row[feature])
        counts[index] += 1
        wins[index] += row["actual_resolution"] == "Up"
        usable.append(window)
    raw = [(win + 1) / (count + 2) for win, count in zip(wins, counts)]
    return {"counts": counts, "wins": wins, "p": isotonic(raw, [n + 2 for n in counts]),
            "training_windows": usable, "available_before_ms": available_before_ms}


def model_probability(model, value):
    if value is None:
        return None
    index = bisect.bisect_right(DELTA_EDGES, value)
    if model["counts"][index] < MIN_CELL_WINDOWS:
        return None
    return model["p"][index]


def valid_ask(value):
    return value is not None and math.isfinite(value) and 0.05 <= value <= 0.95


def fee_per_share(price, rate, exponent):
    # Assumed scenario, not a reconstruction of historic charged fees.
    return rate * (price * (1 - price)) ** exponent


def candidate_definitions():
    return [{"name": f"{kind}_{feature.removesuffix('_delta_pct')}_{horizon}s",
             "kind": kind, "horizon": horizon, "feature": feature}
            for horizon in HORIZONS for feature in FEATURES for kind in ("direction", "calibrated")]


def proposed_trade(candidate, row, model, rate, exponent, slippage):
    if row is None or row[candidate["feature"]] is None:
        return None
    delta = row[candidate["feature"]]
    if candidate["kind"] == "direction":
        if abs(delta) < 0.05:
            return None
        side = "Up" if delta >= 0 else "Down"
    else:
        p_up = model_probability(model, delta)
        if p_up is None:
            return None
        choices = []
        for side, probability in (("Up", p_up), ("Down", 1 - p_up)):
            ask = row[side.lower() + "_ask"]
            if valid_ask(ask) and ask + slippage < 1:
                edge = probability - ask - slippage - fee_per_share(ask + slippage, rate, exponent)
                choices.append((edge, side))
        if not choices or max(choices)[0] < 0.03:
            return None
        side = max(choices)[1]
    ask = row[side.lower() + "_ask"]
    if not valid_ask(ask) or ask + slippage >= 1:
        return None
    return {"window_ts": row["window_ts"], "timestamp_ms": row["timestamp_ms"],
            "side": side, "ask": ask, "won": int(side == row["actual_resolution"])}


def evaluate(trades, windows, rate, exponent, slippage, bootstrap=False):
    by_window = {trade["window_ts"]: trade for trade in trades}
    costs, returns, pnl = [], [], []
    for window in windows:
        trade = by_window.get(window)
        if trade is None:
            returns.append(0.0)
            continue
        price = min(0.9999, trade["ask"] + slippage)
        cost = price + fee_per_share(price, rate, exponent)
        costs.append(cost)
        pnl.append(trade["won"] - cost)
        returns.append(trade["won"] - cost)
    wins = sum(trade["won"] for trade in trades)
    return {"windows": len(windows), "hypothetical_trades": len(trades), "wins": wins,
            "one_share_cost_usdc": sum(costs), "one_share_pnl_usdc": sum(pnl),
            "roi": sum(pnl) / sum(costs) if costs else None,
            "mean_pnl_per_window": mean(returns),
            "mean_pnl_per_window_block_95": block_mean_interval(returns) if bootstrap else None,
            "win_rate": wins / len(trades) if trades else None,
            "win_rate_wilson_95": wilson(wins, len(trades))}


def experiment(samples, resolutions, rate, exponent, slippage):
    by_window = collections.defaultdict(list)
    for row in samples:
        if row["actual_resolution"] in ("Up", "Down"):
            by_window[row["window_ts"]].append(row)
    windows = sorted(by_window)
    if len(windows) < 15:
        return {"status": "insufficient_windows", "labeled_windows": len(windows)}
    first, second = int(0.6 * len(windows)), int(0.8 * len(windows))
    partitions = {"train": windows[:first], "validation": windows[first:second], "test": windows[second:]}
    first_validation_ms = partitions["validation"][0] * 1000
    first_test_ms = partitions["test"][0] * 1000
    # Selection may see only validation labels received before the test starts.
    validation = [w for w in partitions["validation"] if resolutions.get(w, {}).get("resolution_event_ms") is not None
                  and resolutions[w]["resolution_event_ms"] < first_test_ms]
    partitions["validation"] = validation
    candidates = candidate_definitions()
    results = []
    models = {}
    trades_by_candidate = {}
    for candidate in candidates:
        model = fit_model(partitions["train"], by_window, resolutions, candidate["horizon"],
                          candidate["feature"], first_validation_ms)
        models[candidate["name"]] = model
        candidate_trades = {}
        for partition, partition_windows in partitions.items():
            candidate_trades[partition] = [trade for window in partition_windows
                if (trade := proposed_trade(candidate, snapshot(by_window[window], candidate["horizon"]),
                                            model, rate, exponent, slippage)) is not None]
        trades_by_candidate[candidate["name"]] = candidate_trades
        results.append({"candidate": candidate, "fit_independent_windows": len(model["training_windows"]),
                        "fit_cell_windows": model["counts"],
                        "train_in_sample": evaluate(candidate_trades["train"], partitions["train"], rate, exponent, slippage),
                        "validation": evaluate(candidate_trades["validation"], validation, rate, exponent, slippage)})
    # Test outcomes are not accessed for selection. Max favors return/window,
    # then deterministic name order, and does not optimize using test metrics.
    supported_validation = [r for r in results if r["validation"]["hypothetical_trades"] >= MIN_VALIDATION_TRADES]
    eligible = [r for r in supported_validation if r["validation"]["mean_pnl_per_window"] > 0]
    ranked = sorted(results, key=lambda r: (-r["validation"]["mean_pnl_per_window"], r["candidate"]["name"]))
    selected = sorted(eligible, key=lambda r: (-r["validation"]["mean_pnl_per_window"], r["candidate"]["name"]))[0] if eligible else None
    # An under-supported leader is shown for diagnosis, explicitly not selected.
    diagnostic = selected or (sorted(supported_validation, key=lambda r: (-r["validation"]["mean_pnl_per_window"], r["candidate"]["name"]))[0] if supported_validation else ranked[0])
    name = diagnostic["candidate"]["name"]
    test_trades = trades_by_candidate[name]["test"]
    test_result = evaluate(test_trades, partitions["test"], rate, exponent, slippage, True)
    stress = {f"slippage_{stress_slippage:.3f}": evaluate(test_trades, partitions["test"], rate, exponent, stress_slippage, True)
              for stress_slippage in sorted({0.0, slippage, 0.02})}
    calibration = []
    for horizon in HORIZONS:
        for feature in FEATURES:
            candidate_name = f"calibrated_{feature.removesuffix('_delta_pct')}_{horizon}s"
            model = models[candidate_name]
            model_errors, market_errors = [], []
            for window in partitions["test"]:
                row = snapshot(by_window[window], horizon)
                if row is None:
                    continue
                probability = model_probability(model, row[feature])
                if probability is None or row["up_ask"] is None or row["up_bid"] is None:
                    continue
                if not (0 <= row["up_bid"] <= row["up_ask"] <= 1):
                    continue
                market = (row["up_bid"] + row["up_ask"]) / 2
                outcome = int(row["actual_resolution"] == "Up")
                model_errors.append((probability - outcome) ** 2)
                market_errors.append((market - outcome) ** 2)
            calibration.append({"candidate": candidate_name, "common_test_windows": len(model_errors),
                                "model_brier": mean(model_errors), "market_mid_brier": mean(market_errors)})
    lower = (test_result["mean_pnl_per_window_block_95"] or [-1])[0]
    supported = bool(selected and test_result["hypothetical_trades"] >= MIN_TEST_TRADES and lower > 0)
    return {"status": "offline_only_execution_unverified", "candidate_count": len(candidates),
            "partition": {k: {"windows": len(v), "first_utc": utc(v[0]) if v else None,
                               "last_utc": utc(v[-1]) if v else None} for k, v in partitions.items()},
            "fee_scenario": {"formula": "rate * (price * (1-price)) ** exponent, per share",
                             "rate": rate, "exponent": exponent, "slippage_per_share": slippage,
                             "historical_fees_reconstructed": False},
            "minimum_validation_trades": MIN_VALIDATION_TRADES,
            "minimum_test_trades": MIN_TEST_TRADES,
            "minimum_training_windows_per_cell": MIN_CELL_WINDOWS,
            "selected_candidate": selected["candidate"]["name"] if selected else None,
            "diagnostic_validation_leader": name,
            "validation_candidates": results,
            "frozen_candidate_test": test_result,
            "same_test_entries_cost_stress": stress,
            "test_calibration_comparison": calibration,
            "statistical_gate_passed": supported,
            "live_promotion": False,
            "reasons_no_promotion": ["Only a short contiguous market regime is represented.",
                "Observed asks have no recorded order age, executable depth, latency or actual fills.",
                "Historical fee charges and wallet balances are not reconciled.",
                "Selection among eight candidates introduces selection bias; this small holdout cannot certify an edge.",
                "Maker snapshots contain no resting-order queue or post-only execution evidence."]}


def maker_summary(rows):
    labeled = [r for r in rows if r["actual_resolution"] in ("Up", "Down")]
    crossings = [r for r in labeled if r["would_bid_fill"] == 1]
    first_by_window = {}
    for row in crossings:
        first_by_window.setdefault(row["window_ts"], row)
    def summarize(group):
        return {"observations": len(group), "independent_windows": len({r["window_ts"] for r in group}),
                "winning_fraction": mean([int(r["side"] == r["actual_resolution"]) for r in group]),
                "mean_model_fair_value": mean([r["fair_value"] for r in group]),
                "mean_quoted_bid": mean([r["our_bid"] for r in group]),
                "mean_current_ask": mean([r["market_ask"] for r in group]),
                "synthetic_mean_outcome_minus_quoted_bid": mean([int(r["side"] == r["actual_resolution"]) - r["our_bid"] for r in group])}
    return {"rows": len(rows), "labeled_rows": len(labeled),
            "windows": len({r["window_ts"] for r in rows}),
            "missing_market_ask_rows": sum(r["market_ask"] is None for r in rows),
            "all_bid_crossing_rows_not_independent": summarize(crossings),
            "first_bid_crossing_per_window": summarize(list(first_by_window.values())),
            "real_maker_fills_proven": 0,
            "interpretation": "would_bid_fill compares this tick's proposed bid with this tick's ask. These are immediate crossings, not evidence of a previously resting post-only order filling. Repeated rows cannot be counted as independent trades. No ask-sale profit is claimed without owned inventory."}


def run_audit(db_path, rate, exponent, slippage):
    db_path = Path(db_path).resolve()
    connection = sqlite3.connect(db_path.as_uri() + "?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    connection.execute("PRAGMA query_only=ON")
    connection.execute("BEGIN")
    def query(sql):
        return [dict(row) for row in connection.execute(sql)]
    trades = query("SELECT * FROM trades ORDER BY timestamp,id")
    samples = query("SELECT * FROM delta_samples ORDER BY window_ts,timestamp_ms")
    makers = query("SELECT * FROM maker_shadow ORDER BY timestamp_ms,side")
    observations = query("SELECT window_ts,actual_resolution,resolution_event_ms FROM twap_observations")
    resolutions = {r["window_ts"]: r for r in observations}
    tables = {row["name"]: connection.execute('SELECT count(*) FROM "' + row["name"].replace('"', '""') + '"').fetchone()[0]
              for row in query("SELECT name FROM sqlite_master WHERE type='table' AND name!='sqlite_sequence'")}
    connection.rollback()
    connection.close()
    live = [r for r in trades if r["dry_run"] == 0]
    unresolved = [{k: r[k] for k in ("id", "window_ts", "timestamp", "side", "cost_usdc", "shares")}
                  for r in live if r["profit"] is None]
    sample_windows = sorted({r["window_ts"] for r in samples})
    labels_by_window = collections.defaultdict(set)
    for row in samples:
        if row["actual_resolution"] is not None:
            labels_by_window[row["window_ts"]].add(row["actual_resolution"])
    artifact_path = db_path.parent / "fairvalue.json"
    artifact = None
    if artifact_path.exists():
        raw = artifact_path.read_bytes()
        model = json.loads(raw)
        cells = [cell for row in model["grid"] for cell in row]
        accepted = [cell for cell in cells if not cell["low_conf"] and cell["n"] >= model["min_n"]]
        artifact = {"sha256": hashlib.sha256(raw).hexdigest(),
                    "source_table": model.get("source_table"), "generated_at_ms": model.get("generated_at_ms"),
                    "rows_used": model.get("rows_used"), "accepted_cells": len(accepted),
                    "accepted_cells_with_fewer_than_20_windows": sum(c.get("w", 0) < 20 for c in accepted),
                    "minimum_accepted_cell_windows": min((c.get("w", 0) for c in accepted), default=0)}
    return {"audit_version": 1, "db_file": db_path.name, "database_opened_read_only": True,
            "table_counts": tables,
            "accounting_note": "Recorded payout minus cost is internally consistent but is not wallet-reconciled net profit. There are no explicit historical fee or net-settled-share columns in the supplied database.",
            "live_trades": trade_summary(live), "paper_trades": trade_summary([r for r in trades if r["dry_run"] != 0]),
            "live_by_twap_mode": grouped_trades(live, lambda r: bool(r["used_twap_strike"])),
            "live_by_month": grouped_trades(live, lambda r: utc(r["timestamp"])[:7]),
            "live_by_version": grouped_trades(live, lambda r: r["bot_version"]),
            "unresolved_live_trades": unresolved,
            "accounting_mismatches": {
                "cost_vs_shares_times_price": sum(abs(r["shares"] * r["entry_price"] - r["cost_usdc"]) > 1e-6 for r in trades),
                "profit_vs_payout_minus_cost": sum(r["profit"] is not None and abs(r["payout"] - r["cost_usdc"] - r["profit"]) > 1e-6 for r in trades)},
            "unconditional_data": {"rows": len(samples), "windows": len(sample_windows),
                "labeled_windows": len(labels_by_window), "conflicting_labels": sum(len(v) > 1 for v in labels_by_window.values()),
                "first_utc": utc(sample_windows[0]) if sample_windows else None,
                "last_utc": utc(sample_windows[-1]) if sample_windows else None,
                "missing_twap_rows": sum(r["twap_delta_pct"] is None for r in samples),
                "missing_any_book_field_rows": sum(any(r[k] is None for k in ("up_bid", "up_ask", "down_bid", "down_ask")) for r in samples),
                "no_ask_on_either_side_rows": sum(r["up_ask"] is None and r["down_ask"] is None for r in samples)},
            "existing_fairvalue_artifact": artifact, "maker_shadow": maker_summary(makers),
            "chronological_experiment": experiment(samples, resolutions, rate, exponent, slippage)}


def write_markdown(report):
    live = report["live_trades"]
    twap = report["live_by_twap_mode"].get("True", {})
    data = report["unconditional_data"]
    maker = report["maker_shadow"]["all_bid_crossing_rows_not_independent"]
    experiment_report = report["chronological_experiment"]
    lines = ["# Profitability audit", "",
             "This audit found no defensible basis to promote a strategy to live trading. It reads SQLite in read-only mode and places no orders.", "",
             f"The database records {live['trades']} live trades, {live['resolved']} resolved, with ${live['recorded_pnl_usdc']:.2f} recorded PnL. This is not a wallet-reconciled net result. One or more pending trades carry ${live['unresolved_cost_usdc']:.2f} cost. Recorded peak-to-trough drawdown is ${live['recorded_max_drawdown_usdc']:.2f}.", "",
             f"The TWAP-enabled period has {twap.get('trades', 0)} trades and ${twap.get('recorded_pnl_usdc', 0):.2f} recorded PnL on ${twap.get('resolved_cost_usdc', 0):.2f} cost. Changes in sizing and market regime confound comparisons; the earlier period is not evidence that reverting will profit.", "",
             "## Data and maker evidence", "",
             f"The unconditional sample contains {data['rows']:,} rows but only {data['labeled_windows']} labeled five-minute windows, from {data['first_utc']} to {data['last_utc']}. {data['missing_any_book_field_rows']:,} rows lack at least one book field; {data['no_ask_on_either_side_rows']:,} have neither ask.", "",
             f"There are {maker['observations']:,} labeled bid-crossing rows. Their winning fraction is {maker['winning_fraction']:.2%}, versus a mean model probability of {maker['mean_model_fair_value']:.2%} and a mean quoted bid of ${maker['mean_quoted_bid']:.4f}. Outcome minus quoted bid averages ${maker['synthetic_mean_outcome_minus_quoted_bid']:.4f} per synthetic share before costs. This is an adverse-selection diagnostic, not realized or executable maker PnL.", "",
             report["maker_shadow"]["interpretation"], "",
             "## Chronological experiment", "",
             "Eight fixed candidate rules combine 120/60 seconds remaining, TWAP/spot delta, and either an absolute 0.05% direction threshold or a train-only calibrated probability with at least 0.03 estimated edge. The calibrated models use Laplace smoothing, monotonic pooling and at least ten independent training windows in the input cell. Training scores are explicitly in-sample.", "",
             "The first scheduled sample within ten seconds after each horizon is the only decision. Missing data causes abstention. Buy prices are the selected side's observed ask plus the stated slippage and fee scenario. There is one hypothetical share and at most one trade per window. These rules do not replay the live strategy's full historical configuration.", "",
             "The chronological split is 60% training, 20% validation and 20% test by complete window. Training labels must have arrived before validation starts. Validation labels not known at the test boundary are embargoed. Validation selects by average PnL per window, requiring twenty trades and positive validation PnL. Test outcomes never choose the candidate or fit the parameters. Under-supported validation leaders are diagnostic only.", ""]
    if "partition" not in experiment_report:
        lines.append("Too few labeled windows for the experiment.")
        return "\n".join(lines) + "\n"
    lines += ["| Partition | Windows | First UTC | Last UTC |", "|---|---:|---|---|"]
    for name, partition in experiment_report["partition"].items():
        lines.append(f"| {name} | {partition['windows']} | {partition['first_utc']} | {partition['last_utc']} |")
    lines += ["", "| Candidate | Train trades | Train PnL* | Validation trades | Validation PnL* |", "|---|---:|---:|---:|---:|"]
    for result in experiment_report["validation_candidates"]:
        train, validation = result["train_in_sample"], result["validation"]
        lines.append(f"| {result['candidate']['name']} | {train['hypothetical_trades']} | ${train['one_share_pnl_usdc']:.4f} | {validation['hypothetical_trades']} | ${validation['one_share_pnl_usdc']:.4f} |")
    fees = experiment_report["fee_scenario"]
    test = experiment_report["frozen_candidate_test"]
    lines += ["", f"*Synthetic one-share results; assumed per-share fee = {fees['rate']} × [p(1−p)]^{fees['exponent']}, plus ${fees['slippage_per_share']:.3f} slippage. This scenario does not reconstruct actual historic fees or claim a quote could be filled. No maker rebates are assumed.", "",
              f"Selected candidate: **{experiment_report['selected_candidate'] or 'none — insufficient validation trades'}**. Diagnostic validation leader: `{experiment_report['diagnostic_validation_leader']}`. Its frozen test has {test['hypothetical_trades']} trades across {test['windows']} windows and ${test['one_share_pnl_usdc']:.4f} synthetic PnL. Its 95% moving-block bootstrap interval for PnL per window is {test['mean_pnl_per_window_block_95']}. Blocks contain twelve consecutive windows, with no-trade windows retained; the interval measures sampling variation in this short period only.", "",
              "| Same frozen test entries | Synthetic PnL | Synthetic ROI |", "|---|---:|---:|"]
    for name, stress in experiment_report["same_test_entries_cost_stress"].items():
        roi = f"{stress['roi']:.2%}" if stress["roi"] is not None else "n/a"
        lines.append(f"| {name} | ${stress['one_share_pnl_usdc']:.4f} | {roi} |")
    lines += ["", "All stress rows use the same entries; the rule is not reselected when costs change. At least 100 test trades and a positive lower confidence bound would be needed even for the statistical gate. Execution verification and multiple future market regimes remain necessary regardless of that gate.", "",
              "## Reproduce and interpret", "", "```powershell", "python tools/audit_profitability.py --db t.db --out reports/audit_profitability", "python tools/audit_profitability.py --self-test", "```", "",
              "The adjacent JSON contains full metrics, monthly and version breakdowns, unresolved trade identifiers, data coverage, model sample counts, and calibration comparisons. No historical winning subset is promoted. New test periods must be collected after freezing a candidate; repeatedly tuning on this report turns its test into training data.", ""]
    return "\n".join(lines)


def self_test():
    base = {"window_ts": 300, "timestamp_ms": 480000, "secs_left": 120,
            "twap_delta_pct": 0.06, "spot_delta_pct": 0.06, "actual_resolution": "Up",
            "up_ask": 0.6, "down_ask": 0.41}
    assert snapshot([base, dict(base, timestamp_ms=485000, secs_left=115)], 120) is base
    candidate = {"kind": "direction", "feature": "twap_delta_pct"}
    trade = proposed_trade(candidate, base, {}, 0.072, 1.0, 0.01)
    assert trade is not None
    assert proposed_trade(candidate, dict(base, up_ask=None), {}, 0.072, 1.0, 0.01) is None
    assert evaluate([trade], [300], 0.072, 1, 0.02)["one_share_pnl_usdc"] < evaluate([trade], [300], 0.072, 1, 0)["one_share_pnl_usdc"]
    rows = {300: [base]}
    late = {300: {"actual_resolution": "Up", "resolution_event_ms": 900000}}
    assert sum(fit_model([300], rows, late, 120, "twap_delta_pct", 800000)["counts"]) == 0
    assert sum(fit_model([300], rows, late, 120, "twap_delta_pct", 1000000)["counts"]) == 1
    assert model_probability({"counts": [0] * 8, "p": [0.9] * 8}, 0.06) is None
    assert sum(isotonic([0.8, 0.2], [1, 1])) == 1.0
    assert block_mean_interval([0.0] * 20) == [0.0, 0.0]
    print("audit self-tests passed: sample selection, abstention, label availability, cost stress, thin cells, uncertainty")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", default="t.db")
    parser.add_argument("--out", default="reports/audit_profitability")
    parser.add_argument("--fee-rate", type=float, default=0.072)
    parser.add_argument("--fee-exponent", type=float, default=1.0)
    parser.add_argument("--slippage", type=float, default=0.01)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if not (0 <= args.fee_rate <= 1 and 0 < args.fee_exponent <= 5 and 0 <= args.slippage <= 0.1):
        parser.error("invalid fee or slippage scenario")
    report = run_audit(args.db, args.fee_rate, args.fee_exponent, args.slippage)
    output = Path(args.out)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.with_suffix(".json").write_text(json.dumps(report, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    output.with_suffix(".md").write_text(write_markdown(report), encoding="utf-8")
    print(f"Wrote {output.with_suffix('.md')} and {output.with_suffix('.json')}; live promotion: false")


if __name__ == "__main__":
    main()
