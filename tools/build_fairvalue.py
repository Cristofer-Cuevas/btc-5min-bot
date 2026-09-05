#!/usr/bin/env python3
"""Build the empirical fair-value curve from the bot's own labelled history.

Standalone analysis tool. NOT part of the bot binary and never imported by it;
the only contract between the two is the JSON file this writes.

WHAT IT ESTIMATES
-----------------
    P(the window resolves "Up" | signed decision delta, seconds remaining)

"Up" is the fixed REFERENCE SIDE. Every probability in the output file is the
probability that Up wins; the Down price is its complement (1 - p). Fixing one
reference side is what makes the monotonicity constraint meaningful: P(Up wins)
must be non-decreasing in the signed delta. Scoring "did the signalled side
win?" instead would produce a U-shape (both large negative and large positive
deltas look like wins) and monotonicity would be nonsense.

DATA SOURCE
-----------
`signals` joined to `twap_observations` on window_ts. `signals.decision_delta`
is the delta that actually drove the threshold and side selection at that
instant (the TWAP delta under USE_TWAP_STRIKE=true, the spot delta otherwise) --
i.e. the same quantity the bot will have in hand when it quotes.
`twap_observations.actual_resolution` is the settled outcome.

SAMPLING -- read before trusting the numbers
--------------------------------------------
There are TWO sampling regimes in the data, split by when the bot was running
with SIGNAL_LOG_CADENCE_MS enabled.

BEFORE the cadence (change-only logging): `signals` rows are NOT a uniform time
sample. The bot wrote a row only when the rejection reason CHANGED, giving
roughly 2.5 rows per window, all at transition moments. The curve fitted on
that data is conditioned on "a moment where the evaluation outcome changed",
while the bot uses it on every tick. Worse, it leaves whole regions unobserved:
a delta that sits quietly below the entry threshold is logged once and then
never again, which is why the near-zero delta buckets come out empty.

AFTER the cadence: a row is also written every SIGNAL_LOG_CADENCE_MS (default
5s) whenever the evaluation produced a decision delta, giving ~60 rows per
window on a uniform grid. This is the regime the curve should be fitted on.

Use --since to fit on the cadence era only once enough of it has accumulated;
mixing the two regimes weights transition moments far too heavily.

In both regimes cell counts (`n`) double-count windows: `w` (distinct windows)
is reported alongside and is the honest independent-sample count.

METHOD
------
1. Bucket by (signed delta, secs_left) on the edges below.
2. Per cell: n rows, w distinct windows, k = rows resolving Up.
3. Laplace smoothing: p = (k + 1) / (n + 2). Thin cells fall back toward 0.5
   instead of producing a hard 0.0 or 1.0 off two observations.
4. Isotonic regression (pool-adjacent-violators) across the delta axis within
   each secs_left bucket, weighted by (n + 2) -- the effective observation count
   of the smoothed estimate, so empty cells sit at 0.5 with weight 2 and get
   overridden by any real data rather than dragging it.
5. Flag cells with n < MIN_N as low confidence. The bot refuses to quote from
   them.

Usage:
    python tools/build_fairvalue.py [--db trades.db] [--out fairvalue.json]
                                    [--min-n 20] [--since YYYY-MM-DD]
                                    [--source signals|delta_samples] [--report]
"""

from __future__ import annotations

import argparse
import datetime
import json
import sqlite3
import sys
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
SELECT s.window_ts,
       s.decision_delta,
       s.secs_left,
       t.actual_resolution
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
SELECT window_ts,
       twap_delta_pct,
       secs_left,
       actual_resolution
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


def build(db_path: str, min_n: int, since_ms: int = 0,
          source: str = "signals") -> dict:
    query = SOURCES[source]
    conn = sqlite3.connect("file:{}?mode=ro".format(db_path), uri=True)
    try:
        rows = conn.execute(query, (since_ms,)).fetchall()
    except sqlite3.OperationalError as exc:
        conn.close()
        raise SystemExit(
            "cannot read '{}' from {}: {}\n"
            "If the table does not exist yet, the bot has not run with this "
            "build long enough to create it.".format(source, db_path, exc)
        )
    finally:
        try:
            conn.close()
        except Exception:
            pass

    n_secs = len(SECS_EDGES) - 1
    n_delta = len(DELTA_EDGES) + 1

    counts = [[0] * n_delta for _ in range(n_secs)]
    wins = [[0] * n_delta for _ in range(n_secs)]
    windows = [[set() for _ in range(n_delta)] for _ in range(n_secs)]

    used = 0
    skipped_secs = 0
    for window_ts, delta, secs_left, resolution in rows:
        si = secs_bucket(int(secs_left))
        if si is None:
            skipped_secs += 1
            continue
        di = delta_bucket(float(delta))
        counts[si][di] += 1
        windows[si][di].add(window_ts)
        if resolution == REFERENCE_SIDE:
            wins[si][di] += 1
        used += 1

    # Laplace smoothing, then isotonic regression along the delta axis.
    grid = []
    for si in range(n_secs):
        raw = [
            (wins[si][di] + 1) / (counts[si][di] + 2)
            for di in range(n_delta)
        ]
        weights = [float(counts[si][di] + 2) for di in range(n_delta)]
        fitted = pava(raw, weights)
        row = []
        for di in range(n_delta):
            n = counts[si][di]
            row.append({
                "n": n,
                "w": len(windows[si][di]),
                "k": wins[si][di],
                "p_raw": round(wins[si][di] / n, 6) if n else None,
                "p_smoothed": round(raw[di], 6),
                "p": round(min(0.99, max(0.01, fitted[di])), 6),
                "low_conf": n < min_n,
            })
        grid.append(row)

    return {
        "version": 1,
        "generated_at_ms": int(time.time() * 1000),
        "source_db": db_path,
        "source_table": source,
        "since_ms": since_ms,
        "reference_side": REFERENCE_SIDE,
        "delta_edges": DELTA_EDGES,
        "delta_centers": [round(c, 6) for c in delta_centers()],
        "secs_edges": SECS_EDGES,
        "min_n": min_n,
        "rows_total": len(rows),
        "rows_used": used,
        "rows_skipped_secs_out_of_range": skipped_secs,
        "grid": grid,
    }


def delta_label(di: int) -> str:
    if di == 0:
        return "(-inf,{:+.2f})".format(DELTA_EDGES[0])
    if di == len(DELTA_EDGES):
        return "[{:+.2f},+inf)".format(DELTA_EDGES[-1])
    return "[{:+.2f},{:+.2f})".format(DELTA_EDGES[di - 1], DELTA_EDGES[di])


def secs_label(si: int) -> str:
    return "{}-{}s".format(SECS_EDGES[si], SECS_EDGES[si + 1])


def report(model: dict) -> None:
    grid = model["grid"]
    n_delta = len(model["delta_centers"])
    min_n = model["min_n"]

    if model.get("since_ms"):
        print("since             : {} ({})".format(
            datetime.datetime.fromtimestamp(
                model["since_ms"] / 1000, datetime.timezone.utc
            ).strftime("%Y-%m-%d %H:%M UTC"),
            model["since_ms"],
        ))
    print("source table      : {}".format(model.get("source_table", "signals")))
    print("rows joined       : {}".format(model["rows_total"]))
    print("rows used         : {}".format(model["rows_used"]))
    print("skipped (secs oob): {}".format(
        model["rows_skipped_secs_out_of_range"]))
    print("reference side    : {}  (p = P(Up resolves))".format(
        model["reference_side"]))
    print("low-confidence    : n < {}".format(min_n))
    print()

    for si, row in enumerate(grid):
        total_n = sum(c["n"] for c in row)
        print("=== secs_left {}   (n={}) ===".format(secs_label(si), total_n))
        print("{:>20} {:>6} {:>6} {:>8} {:>8} {:>8}  flag".format(
            "delta bucket", "n", "wins", "p_raw", "p_smth", "p_fit"))
        for di in range(n_delta):
            c = row[di]
            raw = "{:.4f}".format(c["p_raw"]) if c["p_raw"] is not None else "-"
            flag = "LOW" if c["low_conf"] else ""
            print("{:>20} {:>6} {:>6} {:>8} {:>8.4f} {:>8.4f}  {}".format(
                delta_label(di), c["n"], c["k"], raw,
                c["p_smoothed"], c["p"], flag))
        print()

    print("=== EXTREME CELLS (p_fit > 0.95 or < 0.05) ===")
    print("{:>10} {:>20} {:>6} {:>6} {:>8}  flag".format(
        "secs", "delta bucket", "n", "wins", "p_fit"))
    found = False
    for si, row in enumerate(grid):
        for di, c in enumerate(row):
            if c["p"] > 0.95 or c["p"] < 0.05:
                found = True
                flag = "LOW" if c["low_conf"] else ""
                print("{:>10} {:>20} {:>6} {:>6} {:>8.4f}  {}".format(
                    secs_label(si), delta_label(di), c["n"], c["k"],
                    c["p"], flag))
    if not found:
        print("  none")
    print()

    # ---- COVERAGE: the deliverable ----
    #
    # A curve that can only price near-certainties is useless for making a
    # market. These two numbers say whether this fit can do better than that.
    min_n = model["min_n"]
    populated = 0
    total = 0
    for row in grid:
        for c in row:
            total += 1
            if c["n"] >= min_n:
                populated += 1

    # The uncertain middle band: delta buckets whose whole range lies inside
    # (-0.05, +0.05). Those are the ones the signals-based fit had none of.
    mid_idx = [
        di for di in range(len(model["delta_centers"]))
        if abs(model["delta_centers"][di]) < 0.05
    ]

    print("=== COVERAGE ===")
    print("cells with n >= {}: {} of {}".format(min_n, populated, total))
    print()
    print("uncertain band (delta buckets inside +/-0.05):")
    print("{:>10} {:>20} {:>7} {:>7} {:>9}  flag".format(
        "secs", "delta bucket", "n", "windows", "p_fit"))
    mid_populated = 0
    mid_total = 0
    for si, row in enumerate(grid):
        for di in mid_idx:
            c = row[di]
            mid_total += 1
            if c["n"] >= min_n:
                mid_populated += 1
            flag = "LOW" if c["low_conf"] else ""
            print("{:>10} {:>20} {:>7} {:>7} {:>9.4f}  {}".format(
                secs_label(si), delta_label(di), c["n"], c["w"], c["p"], flag))
    print()
    print("uncertain-band cells with n >= {}: {} of {}".format(
        min_n, mid_populated, mid_total))
    if mid_populated == 0:
        print()
        print("*** THE UNCERTAIN BAND IS STILL EMPTY. ***")
        print("Something is still filtering the samples. Do NOT deploy this")
        print("curve: it can only price near-certainties, which is the wrong")
        print("half of the distribution for making a market. Check that the")
        print("bot is running the unconditional sampler and that")
        print("delta_samples has rows with |twap_delta_pct| < 0.05 and a")
        print("non-NULL actual_resolution.")
    print()

    # Monotonicity assertion -- the fit is worthless if this fails.
    for si, row in enumerate(grid):
        ps = [c["p"] for c in row]
        for a, b in zip(ps, ps[1:]):
            if b < a - 1e-9:
                print("MONOTONICITY VIOLATED in {}: {} -> {}".format(
                    secs_label(si), a, b))
                sys.exit(1)
    print("monotonicity: OK (p non-decreasing in delta within every secs bucket)")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default="trades.db")
    ap.add_argument("--out", default="fairvalue.json")
    ap.add_argument("--min-n", type=int, default=MIN_N)
    ap.add_argument(
        "--source",
        choices=sorted(SOURCES),
        default="signals",
        help="which table to fit from. 'signals' (default) preserves the "
             "original behaviour but is filtered by the strategy's own gates. "
             "'delta_samples' is the unconditional sampler and is the correct "
             "source for a market-making curve.",
    )
    ap.add_argument(
        "--since",
        default=None,
        help="only use signals at or after this time (YYYY-MM-DD or epoch ms). "
             "Use it to fit on the SIGNAL_LOG_CADENCE_MS era only, once enough "
             "of it exists -- mixing the two sampling regimes over-weights "
             "reason-transition moments.",
    )
    ap.add_argument("--report", action="store_true",
                    help="print the full grid to stdout")
    args = ap.parse_args()

    model = build(args.db, args.min_n, parse_since(args.since), args.source)
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(model, fh, indent=1)
    print("wrote {} ({} rows over {}x{} cells)".format(
        args.out, model["rows_used"], len(model["grid"]),
        len(model["delta_centers"])))
    if args.report:
        print()
        report(model)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
