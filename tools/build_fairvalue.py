#!/usr/bin/env python3
"""Fit a diagnostic P(Up | TWAP delta, time) with a chronological holdout.

Default input is the unconditional delta_samples table. Each resolved window
has one vote per cell, regardless of logging cadence. The newest 25% of whole
windows are held out; one intervening window is embargoed. The exported model
is fitted ONLY on the training prefix, including smoothing and isotonic fit.
Repeated observations never cross the train/test boundary.

The holdout reports window-weighted probability losses and coverage, including
comparison with the contemporaneous Up book midpoint on the same observations.
It does not simulate executions or demonstrate profitability. A market maker
needs subsequent trade/depth data, arrival/cancel latency and inventory data.

Usage:
  python tools/build_fairvalue.py --db t.db --out reports/fairvalue_candidate.json
      --validation-out reports/fairvalue_validation.json --report
"""

from __future__ import annotations

import argparse
import datetime
import json
import math
from pathlib import Path
from collections import defaultdict
from contextlib import closing
import sqlite3
import time

# Signed delta bucket edges (percent). 17 interior edges -> 18 buckets, the
# outermost two unbounded.
DELTA_EDGES = [
    -0.30, -0.20, -0.15, -0.10, -0.07, -0.05, -0.03, -0.01, 0.0,
    0.01, 0.03, 0.05, 0.07, 0.10, 0.15, 0.20, 0.30,
]

# Seconds-remaining bucket edges. Half-open [lo, hi), except the last which is
# [240, 301) so the secs_left == 300 tick at window open is captured rather
# than silently dropped.
SECS_EDGES = [0, 30, 60, 90, 120, 180, 240, 301]

MIN_N = 20
REFERENCE_SIDE = "Up"

# The ORIGINAL source. Conditioned on the strategy's own gates: a row exists
# only where evaluate_entry got far enough to set a side and a decision delta,
# and (before SIGNAL_LOG_CADENCE_MS) only at moments the rejection reason
# changed. Fits from this see near-certainties and little else.
SIGNALS_QUERY = """
SELECT s.window_ts, s.timestamp_ms,
       s.decision_delta,
       s.secs_left,
       t.actual_resolution, NULL, NULL
FROM signals s
JOIN twap_observations t ON s.window_ts = t.window_ts
WHERE s.side IS NOT NULL
  AND t.actual_resolution IS NOT NULL
  AND s.decision_delta IS NOT NULL
  AND s.timestamp_ms >= ?
"""

# The UNCONDITIONAL source. One row every DELTA_SAMPLE_INTERVAL_MS for every
# window, with no threshold, trend, side, pause or resolution filtering, and
# actual_resolution backfilled for every settled window including the ones the
# bot never traded. This is the source a pricing curve should be fitted on.
#
# No join is needed: the sampler backfills the outcome onto the row itself.
DELTA_SAMPLES_QUERY = """
SELECT window_ts, timestamp_ms,
       twap_delta_pct,
       secs_left,
       actual_resolution, up_bid, up_ask
FROM delta_samples
WHERE actual_resolution IS NOT NULL
  AND twap_delta_pct IS NOT NULL
  AND timestamp_ms >= ?
"""

SOURCES = {
    "signals": SIGNALS_QUERY,
    "delta_samples": DELTA_SAMPLES_QUERY,
}


def parse_since(value):
    """Epoch ms for a --since argument: a YYYY-MM-DD date, or raw epoch ms."""
    if value is None:
        return 0
    value = value.strip()
    if value.isdigit():
        return int(value)
    try:
        dt = datetime.datetime.strptime(value, "%Y-%m-%d")
    except ValueError:
        raise SystemExit(
            "--since must be YYYY-MM-DD or epoch milliseconds, got {!r}".format(value)
        )
    return int(dt.replace(tzinfo=datetime.timezone.utc).timestamp() * 1000)


def delta_bucket(delta: float) -> int:
    """Index of the delta bucket containing `delta`. Buckets are [lo, hi)."""
    for i, edge in enumerate(DELTA_EDGES):
        if delta < edge:
            return i
    return len(DELTA_EDGES)


def secs_bucket(secs: int):
    """Index of the secs_left bucket, or None if outside the modelled range."""
    for i in range(len(SECS_EDGES) - 1):
        if SECS_EDGES[i] <= secs < SECS_EDGES[i + 1]:
            return i
    return None


def delta_centers() -> list:
    """Representative x for each delta bucket, used for linear interpolation.

    Interior buckets use their midpoint. The two unbounded buckets are given a
    finite centre one half-width beyond their finite edge, so the curve keeps a
    defined slope out to the tails instead of jumping.
    """
    centers = []
    first_w = DELTA_EDGES[1] - DELTA_EDGES[0]
    centers.append(DELTA_EDGES[0] - first_w / 2.0)
    for lo, hi in zip(DELTA_EDGES, DELTA_EDGES[1:]):
        centers.append((lo + hi) / 2.0)
    last_w = DELTA_EDGES[-1] - DELTA_EDGES[-2]
    centers.append(DELTA_EDGES[-1] + last_w / 2.0)
    return centers


def pava(values: list, weights: list) -> list:
    """Weighted pool-adjacent-violators. Returns the non-decreasing
    least-squares fit to `values`. Adjacent blocks that violate monotonicity
    are merged into their weighted mean until the sequence is ordered."""
    stack = []  # [mean, weight, span]
    for v, w in zip(values, weights):
        stack.append([v, w, 1.0])
        while len(stack) > 1 and stack[-2][0] > stack[-1][0] + 1e-12:
            m2, w2, c2 = stack.pop()
            m1, w1, c1 = stack.pop()
            tw = w1 + w2
            mean = (m1 * w1 + m2 * w2) / tw if tw > 0 else (m1 + m2) / 2.0
            stack.append([mean, tw, c1 + c2])
    out = []
    for mean, _w, span in stack:
        out.extend([mean] * int(span))
    return out


def read_samples(db_path: str, source: str, since_ms: int) -> tuple[list, dict]:
    uri = Path(db_path).resolve().as_uri() + "?mode=ro"
    with closing(sqlite3.connect(uri, uri=True)) as conn:
        rows = conn.execute(SOURCES[source], (since_ms,)).fetchall()
        columns = {r[1] for r in conn.execute("PRAGMA table_info(twap_observations)")}
        arrivals = dict(conn.execute("SELECT window_ts, resolution_event_ms FROM twap_observations")) if "resolution_event_ms" in columns else None
    samples = []
    invalid = 0
    resolutions = {}
    for window, timestamp, delta, secs, outcome, bid, ask in rows:
        if (outcome not in ("Up", "Down") or not math.isfinite(float(delta))
                or secs_bucket(int(secs)) is None
                or not window * 1000 <= timestamp <= (window + 300) * 1000):
            invalid += 1
            continue
        if window in resolutions and resolutions[window] != outcome:
            raise ValueError("conflicting resolution labels in window {}".format(window))
        resolutions[window] = outcome
        midpoint = None
        if (bid is not None and ask is not None
                and math.isfinite(bid) and math.isfinite(ask)
                and 0 < bid < ask < 1):
            midpoint = (bid + ask) / 2
        samples.append((window, timestamp, float(delta), int(secs), outcome, midpoint))
    samples.sort(key=lambda row: (row[0], row[1]))
    return samples, {"rows_total": len(rows), "rows_invalid": invalid, "label_arrivals": arrivals}


def split_windows(samples: list, holdout_fraction: float, embargo_windows: int, label_arrivals=None):
    if not 0 < holdout_fraction < 0.5:
        raise ValueError("holdout_fraction must lie strictly between 0 and 0.5")
    if embargo_windows < 1:
        raise ValueError("embargo_windows must be at least one")
    windows = sorted({r[0] for r in samples})
    test_n = max(1, math.ceil(len(windows) * holdout_fraction))
    cut = len(windows) - test_n
    if cut <= embargo_windows:
        raise ValueError("not enough resolved windows for training, embargo and holdout")
    train_windows = set(windows[:cut - embargo_windows])
    test_windows = set(windows[cut:])
    train = [r for r in samples if r[0] in train_windows]
    test = [r for r in samples if r[0] in test_windows]
    # Windows are 300 seconds long. Never train on an outcome whose window
    # overlaps the first held-out observation, even with malformed schedules.
    first_test_ms = min(r[1] for r in test)
    train = [r for r in train if (r[0] + 300) * 1000 < first_test_ms]
    if label_arrivals is not None:
        boundary = min(test_windows) * 1000
        train = [r for r in train if label_arrivals.get(r[0]) is not None and label_arrivals[r[0]] < boundary]
    if not train:
        raise ValueError("no training windows remain after temporal purge")
    return train, test


def fit_samples(samples: list, min_n: int, min_windows: int) -> dict:
    if min_n < 1 or min_windows < 1:
        raise ValueError("minimum counts must be positive")
    n_secs, n_delta = len(SECS_EDGES) - 1, len(DELTA_EDGES) + 1
    counts = [[0] * n_delta for _ in range(n_secs)]
    row_wins = [[0] * n_delta for _ in range(n_secs)]
    windows = [[{} for _ in range(n_delta)] for _ in range(n_secs)]
    for window, _timestamp, delta, secs, outcome, _mid in samples:
        si, di = secs_bucket(secs), delta_bucket(delta)
        counts[si][di] += 1
        row_wins[si][di] += outcome == "Up"
        windows[si][di][window] = outcome == "Up"
    grid = []
    for si in range(n_secs):
        ws = [len(c) for c in windows[si]]
        ks = [sum(c.values()) for c in windows[si]]
        raw = [(k + 1) / (w + 2) for k, w in zip(ks, ws)]
        fitted = pava(raw, [w + 2 for w in ws])
        grid.append([{
            "n": counts[si][di], "w": ws[di], "k": row_wins[si][di],
            "kw": ks[di], "p_raw": ks[di] / ws[di] if ws[di] else None,
            "p_smoothed": raw[di], "p": min(0.99, max(0.01, fitted[di])),
            "low_conf": counts[si][di] < min_n or ws[di] < min_windows,
        } for di in range(n_delta)])
    return {
        "version": 2, "reference_side": "Up", "delta_edges": DELTA_EDGES,
        "delta_centers": delta_centers(), "secs_edges": SECS_EDGES,
        "min_n": min_n, "min_windows": min_windows,
        "weighting": "one_vote_per_window_per_cell", "grid": grid,
    }


def predict(model: dict, delta: float, secs: int):
    """Match Rust's confidence-gated interpolation, including thin neighbours."""
    if not math.isfinite(delta):
        return None
    si = next((i for i, (a, b) in enumerate(zip(
        model["secs_edges"], model["secs_edges"][1:])) if a <= secs < b), None)
    if si is None:
        return None
    di = sum(delta >= edge for edge in model["delta_edges"])
    row, centers = model["grid"][si], model["delta_centers"]
    def usable(c):
        return (not c["low_conf"] and c["n"] >= model["min_n"]
                and c["w"] >= model["min_windows"])
    if not usable(row[di]):
        return None
    if abs(delta - centers[di]) < 1e-12:
        return row[di]["p"]
    lo, hi = (di, di + 1) if delta >= centers[di] else (di - 1, di)
    if lo < 0 or hi >= len(centers):
        return row[di]["p"]
    if not usable(row[lo]) or not usable(row[hi]):
        return None
    t = max(0.0, min(1.0, (delta - centers[lo]) / (centers[hi] - centers[lo])))
    return row[lo]["p"] + t * (row[hi]["p"] - row[lo]["p"])


def probability_scores(observations: list) -> dict:
    """Equal weight per window; rows within a window share one vote."""
    by_window = defaultdict(list)
    for window, p, y in observations:
        p = min(1 - 1e-12, max(1e-12, p))
        by_window[window].append(((p - y) ** 2, -y * math.log(p) - (1-y) * math.log(1-p)))
    if not by_window:
        return {"rows": 0, "windows": 0, "brier": None, "log_loss": None}
    losses = [(sum(a for a, _ in v) / len(v), sum(b for _, b in v) / len(v))
              for v in by_window.values()]
    return {"rows": len(observations), "windows": len(by_window),
            "brier": sum(a for a, _ in losses) / len(losses),
            "log_loss": sum(b for _, b in losses) / len(losses)}


def validate_holdout(model: dict, train: list, test: list) -> dict:
    train_outcomes = {r[0]: r[4] == "Up" for r in train}
    baseline = (sum(train_outcomes.values()) + 1) / (len(train_outcomes) + 2)
    fitted, constant, midpoint, common_model = [], [], [], []
    for window, _timestamp, delta, secs, outcome, mid in test:
        p = predict(model, delta, secs)
        if p is None:
            continue
        y = float(outcome == "Up")
        fitted.append((window, p, y))
        constant.append((window, baseline, y))
        if mid is not None:
            midpoint.append((window, mid, y))
            common_model.append((window, p, y))
    return {
        "method": "chronological_whole_window_holdout_with_embargo",
        "training_windows": len(train_outcomes), "training_rows": len(train),
        "training_first_window": min(train_outcomes),
        "training_last_window": max(train_outcomes),
        "holdout_windows": len({r[0] for r in test}), "holdout_rows": len(test),
        "holdout_first_window": min(r[0] for r in test),
        "holdout_last_window": max(r[0] for r in test),
        "prediction_coverage_rows": len(fitted) / len(test),
        "model": probability_scores(fitted),
        "training_base_rate_on_model_coverage": probability_scores(constant),
        "model_on_market_coverage": probability_scores(common_model),
        "market_midpoint_on_same_coverage": probability_scores(midpoint),
        "profitability_demonstrated": False,
        "limitations": [
            "Probability scores are not execution P&L or a live-trading approval.",
            "Snapshots lack passive queue position, trade flow, quantities and latency.",
            "One short holdout does not establish robustness across market regimes.",
            "Outcome label availability times are absent; the embargo reduces but cannot prove elimination of label delay.",
            "Window weighting avoids duplicate outcomes; adjacent windows may still be dependent.",
        ],
    }


def build(db_path: str, min_n: int = MIN_N, since_ms: int = 0,
          source: str = "delta_samples", min_windows: int = MIN_N,
          holdout_fraction: float = 0.25, embargo_windows: int = 1) -> dict:
    samples, diagnostics = read_samples(db_path, source, since_ms)
    arrivals = diagnostics.pop("label_arrivals")
    train, test = split_windows(samples, holdout_fraction, embargo_windows, arrivals)
    model = fit_samples(train, min_n, min_windows)
    model.update({
        "generated_at_ms": int(time.time() * 1000), "source_db": db_path,
        "source_table": source, "since_ms": since_ms,
        "feature": "twap_delta_pct" if source == "delta_samples" else "decision_delta",
        "rows_used": len(train), "holdout_fraction": holdout_fraction,
        "embargo_windows": embargo_windows, **diagnostics,
        "validation": validate_holdout(model, train, test),
    })
    model["validation"]["label_availability_checked"] = arrivals is not None
    if arrivals is not None:
        model["validation"]["limitations"] = [s for s in model["validation"]["limitations"] if not s.startswith("Outcome label availability")]
    if source == "signals":
        model["validation"]["limitations"].append(
            "signals are filtered by strategy gates; estimates do not cover unconditional market making.")
    return model


def report(model: dict) -> None:
    validation = model["validation"]
    usable = sum(not c["low_conf"] for row in model["grid"] for c in row)
    print("Source: {}; training {} windows / {} rows; holdout {} windows / {} rows".format(
        model["source_table"], validation["training_windows"], model["rows_used"],
        validation["holdout_windows"], validation["holdout_rows"]))
    print("Independent-window minimum: {}; usable cells: {}; holdout row coverage: {:.1%}".format(
        model["min_windows"], usable, validation["prediction_coverage_rows"]))
    print(json.dumps(validation, indent=2))
    print("No maker fills or profitability are demonstrated by this report.")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default="trades.db")
    ap.add_argument("--out", default="fairvalue.json")
    ap.add_argument("--validation-out", help="optional standalone holdout report JSON")
    ap.add_argument("--min-n", type=int, default=MIN_N, help="minimum raw rows per cell")
    ap.add_argument("--min-windows", type=int, default=MIN_N,
                    help="minimum distinct resolved windows per cell")
    ap.add_argument("--source", choices=sorted(SOURCES), default="delta_samples",
                    help="delta_samples is unconditional; signals are strategy-filtered")
    ap.add_argument("--since", help="YYYY-MM-DD or epoch ms")
    ap.add_argument("--holdout-fraction", type=float, default=0.25)
    ap.add_argument("--embargo-windows", type=int, default=1)
    ap.add_argument("--report", action="store_true")
    args = ap.parse_args()
    try:
        model = build(args.db, args.min_n, parse_since(args.since), args.source,
                      args.min_windows, args.holdout_fraction, args.embargo_windows)
    except (sqlite3.Error, ValueError) as exc:
        ap.error(str(exc))
    for path, data in [(args.out, model), (args.validation_out, model["validation"])]:
        if path:
            dest = Path(path)
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(json.dumps(data, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    print("Wrote {} (training prefix only; holdout never refitted)".format(args.out))
    if args.report:
        report(model)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
