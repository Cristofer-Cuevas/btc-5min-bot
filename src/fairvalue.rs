//! Empirical fair-value curve: P(a side resolves winner | signed decision
//! delta, seconds remaining).
//!
//! The curve is FITTED OFFLINE by `tools/build_fairvalue.py` from this bot's
//! own labelled history and loaded here from JSON. There is deliberately no
//! hardcoded fallback curve: a wrong pricing curve is worse than no quoting at
//! all, so a missing or malformed file refuses to start maker mode rather than
//! substituting a guess.
//!
//! The stored probabilities are all for the REFERENCE SIDE "Up". The Down
//! probability is the complement. That asymmetry is what makes the
//! monotonicity invariant (p non-decreasing in signed delta) meaningful, and
//! it is re-verified on load — a non-monotone curve is a data artifact that
//! would produce arbitrageable quotes.

use std::sync::OnceLock;

use serde::Deserialize;
use tracing::info;

/// Which outcome token a probability refers to. Distinct from the SDK's
/// `Side` (Buy/Sell), which describes an order, not an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Up,
    Down,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Up => "Up",
            Side::Down => "Down",
        }
    }

    /// Parse the "Up"/"Down" strings the rest of the bot passes around.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Up" => Some(Side::Up),
            "Down" => Some(Side::Down),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Cell {
    /// Number of observation rows, retained for diagnostics, not confidence.
    pub n: i64,
    /// Distinct resolved windows contributing to this cell. Repeated rows
    /// within one outcome must never manufacture independent evidence.
    #[serde(default)]
    pub w: i64,
    /// Isotonic-fitted P(Up resolves) for this cell.
    pub p: f64,
    pub low_conf: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FairValueModel {
    pub version: u32,
    pub reference_side: String,
    /// Interior delta bucket edges. `delta_edges.len() + 1` buckets.
    pub delta_edges: Vec<f64>,
    /// Representative x for each bucket, used for interpolation. One per
    /// bucket, strictly increasing.
    pub delta_centers: Vec<f64>,
    /// Half-open secs_left bucket edges; `secs_edges.len() - 1` buckets.
    pub secs_edges: Vec<i64>,
    pub min_n: i64,
    /// Minimum independent windows. Legacy v1 files use min_n for this gate.
    #[serde(default)]
    pub min_windows: Option<i64>,
    #[serde(default)]
    pub feature: Option<String>,
    /// `grid[secs_bucket][delta_bucket]`.
    pub grid: Vec<Vec<Cell>>,
}

/// Process-wide model, set once at startup by [`init`].
static MODEL: OnceLock<FairValueModel> = OnceLock::new();

/// Use the feature the model was fitted on, regardless of taker mode.
pub fn input_delta(spot: Option<f64>, twap: Option<f64>, use_twap: bool) -> Option<f64> {
    match MODEL.get()?.feature.as_deref() {
        Some("twap_delta_pct") => twap,
        Some("spot_delta_pct") => spot,
        Some("decision_delta") | None => if use_twap { twap } else { spot },
        _ => None,
    }
}

/// Load, validate and install the process-wide fair-value model.
///
/// Returns an error rather than installing anything questionable: callers are
/// expected to abort startup on failure.
pub fn init(path: &str) -> Result<(), String> {
    let model = load(path)?;
    let c = model.coverage();
    info!(
        "Fair-value model loaded from {}: {} secs buckets x {} delta buckets, \
         reference_side={} min_n={}",
        path,
        model.grid.len(),
        model.delta_centers.len(),
        model.reference_side,
        model.min_n,
    );
    // Coverage is logged because it bounds how often the bot can quote at all:
    // an input landing in any of the (total - usable) cells yields None.
    info!(
        "Fair-value coverage: {}/{} cells usable, fitted on {} rows over {} \
         cell-window observations",
        c.usable_cells, c.total_cells, c.total_rows, c.total_windows,
    );
    MODEL
        .set(model)
        .map_err(|_| "fair-value model already initialised".to_string())
}

/// How much of the grid can actually be priced against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    pub usable_cells: usize,
    pub total_cells: usize,
    /// Total observation rows behind the whole grid.
    pub total_rows: i64,
    /// Total distinct-window counts summed per cell. Windows contributing to
    /// several cells are counted once per cell, so this is an upper bound on
    /// independent samples, not a window count.
    pub total_windows: i64,
}

/// Read and validate a model file without installing it.
pub fn load(path: &str) -> Result<FairValueModel, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read fair-value file '{}': {}", path, e))?;
    let model: FairValueModel = serde_json::from_str(&raw)
        .map_err(|e| format!("cannot parse fair-value file '{}': {}", path, e))?;
    model.validate().map_err(|e| format!("{}: {}", path, e))?;
    Ok(model)
}

impl FairValueModel {
    /// Structural and semantic checks. Every one of these failing means the
    /// curve cannot be priced against safely.
    pub fn validate(&self) -> Result<(), String> {
        if self.feature.as_deref().is_some_and(|f| !matches!(f, "twap_delta_pct" | "spot_delta_pct" | "decision_delta")) {
            return Err("unsupported model feature".into());
        }
        if self.version != 1 && self.version != 2 {
            return Err(format!("unsupported version {}", self.version));
        }
        if self.reference_side != "Up" {
            return Err(format!(
                "reference_side must be \"Up\", got \"{}\"",
                self.reference_side
            ));
        }
        if self.min_n <= 0 || self.min_windows.is_some_and(|w| w <= 0) {
            return Err("minimum sample/window counts must be positive".into());
        }
        if self.version == 2 && self.min_windows.is_none() {
            return Err("version 2 requires min_windows".into());
        }
        if self
            .delta_edges
            .iter()
            .chain(&self.delta_centers)
            .any(|v| !v.is_finite())
        {
            return Err("delta edges and centers must be finite".into());
        }
        if self.delta_edges.windows(2).any(|w| w[1] <= w[0]) {
            return Err("delta_edges must be strictly increasing".into());
        }
        let n_delta = self.delta_edges.len() + 1;
        if self.delta_centers.len() != n_delta {
            return Err(format!(
                "delta_centers has {} entries, expected {} (delta_edges + 1)",
                self.delta_centers.len(),
                n_delta
            ));
        }
        if self.secs_edges.len() < 2 {
            return Err("secs_edges needs at least 2 entries".into());
        }
        let n_secs = self.secs_edges.len() - 1;
        if self.grid.len() != n_secs {
            return Err(format!(
                "grid has {} rows, expected {} (secs_edges - 1)",
                self.grid.len(),
                n_secs
            ));
        }

        for (i, w) in self.delta_centers.windows(2).enumerate() {
            if w[1] <= w[0] {
                return Err(format!(
                    "delta_centers not strictly increasing at index {}: {} -> {}",
                    i, w[0], w[1]
                ));
            }
        }
        for (i, center) in self.delta_centers.iter().enumerate() {
            if (i > 0 && *center < self.delta_edges[i - 1])
                || (i < self.delta_edges.len() && *center >= self.delta_edges[i])
            {
                return Err(format!("delta center {} is outside its bucket", i));
            }
        }
        for (i, w) in self.secs_edges.windows(2).enumerate() {
            if w[1] <= w[0] {
                return Err(format!(
                    "secs_edges not strictly increasing at index {}: {} -> {}",
                    i, w[0], w[1]
                ));
            }
        }

        for (si, row) in self.grid.iter().enumerate() {
            if row.len() != n_delta {
                return Err(format!(
                    "grid row {} has {} cells, expected {}",
                    si,
                    row.len(),
                    n_delta
                ));
            }
            for (di, c) in row.iter().enumerate() {
                if !c.p.is_finite() || !(0.0..=1.0).contains(&c.p) {
                    return Err(format!(
                        "grid[{}][{}].p = {} is not a probability",
                        si, di, c.p
                    ));
                }
                if c.n < 0 {
                    return Err(format!("grid[{}][{}].n = {} is negative", si, di, c.n));
                }
                if c.w < 0 || c.w > c.n {
                    return Err(format!("grid[{}][{}].w must lie in [0, n]", si, di));
                }
            }
            // The monotonicity invariant the offline fit is supposed to
            // enforce. Re-checked here so a hand-edited or stale file cannot
            // introduce an arbitrageable curve.
            for w in row.windows(2) {
                if w[1].p < w[0].p - 1e-9 {
                    return Err(format!(
                        "grid row {} is not monotone in delta: {} -> {}",
                        si, w[0].p, w[1].p
                    ));
                }
            }
        }
        Ok(())
    }

    /// Cells the bot can quote from, and the sample sizes behind the grid.
    pub fn coverage(&self) -> Coverage {
        let mut c = Coverage {
            usable_cells: 0,
            total_cells: 0,
            total_rows: 0,
            total_windows: 0,
        };
        for row in &self.grid {
            for cell in row {
                c.total_cells += 1;
                c.total_rows += cell.n;
                c.total_windows += cell.w;
                if self.usable(cell) {
                    c.usable_cells += 1;
                }
            }
        }
        c
    }

    fn usable(&self, cell: &Cell) -> bool {
        !cell.low_conf && cell.n >= self.min_n && cell.w >= self.min_windows.unwrap_or(self.min_n)
    }

    /// Index of the delta bucket containing `delta`. Buckets are [lo, hi).
    fn delta_bucket(&self, delta: f64) -> usize {
        for (i, edge) in self.delta_edges.iter().enumerate() {
            if delta < *edge {
                return i;
            }
        }
        self.delta_edges.len()
    }

    /// Index of the secs_left bucket, or None outside the modelled range.
    fn secs_bucket(&self, secs_left: i64) -> Option<usize> {
        (0..self.secs_edges.len() - 1)
            .find(|&i| secs_left >= self.secs_edges[i] && secs_left < self.secs_edges[i + 1])
    }

    /// See the free function [`fair_value`]; this is the testable form.
    pub fn fair_value(&self, side: Side, delta: f64, secs_left: i64) -> Option<f64> {
        if !delta.is_finite() {
            return None;
        }
        let si = self.secs_bucket(secs_left)?;
        let row = self.grid.get(si)?;
        let di = self.delta_bucket(delta);

        // Monotonicity alone does not make thin cells reliable. Every cell
        // contributing nonzero interpolation weight must pass confidence.
        if !self.usable(row.get(di)?) {
            return None;
        }

        let p_up = self.interpolate(row, di, delta)?;
        Some(match side {
            Side::Up => p_up,
            Side::Down => 1.0 - p_up,
        })
    }

    /// Linear interpolation between the centres of adjacent delta buckets.
    /// Flat outside the first and last centre, where the isotonic fit has
    /// already saturated.
    fn interpolate(&self, row: &[Cell], di: usize, delta: f64) -> Option<f64> {
        let centers = &self.delta_centers;
        let c = centers[di];
        if (delta - c).abs() < 1e-12 {
            return Some(row[di].p);
        }
        // Pick the neighbour on the side the input actually lies toward.
        let (lo, hi) = if delta >= c {
            if di + 1 >= centers.len() {
                return Some(row[di].p);
            }
            (di, di + 1)
        } else {
            if di == 0 {
                return Some(row[di].p);
            }
            (di - 1, di)
        };
        if !self.usable(&row[lo]) || !self.usable(&row[hi]) {
            return None;
        }
        let (x0, x1) = (centers[lo], centers[hi]);
        let (y0, y1) = (row[lo].p, row[hi].p);
        if (x1 - x0).abs() < f64::EPSILON {
            return Some(y0);
        }
        let t = ((delta - x0) / (x1 - x0)).clamp(0.0, 1.0);
        Some(y0 + t * (y1 - y0))
    }
}

/// Fair probability that `side` wins, given the current signed decision
/// delta and seconds remaining. Interpolates linearly between adjacent
/// delta buckets within the same secs_left bucket. Returns None if the
/// an input or an interpolation neighbour lacks enough distinct windows.
///
/// Also returns None when no model is installed, so callers cannot
/// accidentally price against an uninitialised curve.
pub fn fair_value(side: Side, delta: f64, secs_left: i64) -> Option<f64> {
    MODEL.get()?.fair_value(side, delta, secs_left)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three secs buckets x four delta buckets, monotone, with one thin cell.
    fn model_json() -> String {
        r#"{
          "version": 1,
          "reference_side": "Up",
          "delta_edges": [-0.10, 0.0, 0.10],
          "delta_centers": [-0.15, -0.05, 0.05, 0.15],
          "secs_edges": [0, 60, 120, 301],
          "min_n": 20,
          "grid": [
            [{"n":100,"w":50,"p":0.10,"low_conf":false},
             {"n":100,"w":50,"p":0.30,"low_conf":false},
             {"n":100,"w":50,"p":0.70,"low_conf":false},
             {"n":100,"w":50,"p":0.90,"low_conf":false}],
            [{"n":100,"w":50,"p":0.20,"low_conf":false},
             {"n":  5,"w": 3,"p":0.40,"low_conf":true},
             {"n":100,"w":50,"p":0.60,"low_conf":false},
             {"n":100,"w":50,"p":0.80,"low_conf":false}],
            [{"n":100,"w":50,"p":0.45,"low_conf":false},
             {"n":100,"w":50,"p":0.48,"low_conf":false},
             {"n":100,"w":50,"p":0.52,"low_conf":false},
             {"n":100,"w":50,"p":0.55,"low_conf":false}]
          ]
        }"#
        .to_string()
    }

    fn model() -> FairValueModel {
        let m: FairValueModel = serde_json::from_str(&model_json()).expect("parse");
        m.validate().expect("validate");
        m
    }

    #[test]
    fn up_and_down_are_complements() {
        let m = model();
        let up = m.fair_value(Side::Up, 0.05, 30).expect("up");
        let down = m.fair_value(Side::Down, 0.05, 30).expect("down");
        assert!((up + down - 1.0).abs() < 1e-12, "{} + {} != 1", up, down);
    }

    #[test]
    fn exact_center_returns_cell_value() {
        let m = model();
        assert!((m.fair_value(Side::Up, 0.05, 30).unwrap() - 0.70).abs() < 1e-12);
        assert!((m.fair_value(Side::Up, -0.15, 30).unwrap() - 0.10).abs() < 1e-12);
    }

    #[test]
    fn interpolates_between_adjacent_centers() {
        let m = model();
        // Midway between centres -0.05 (p=0.30) and 0.05 (p=0.70).
        let p = m.fair_value(Side::Up, 0.0, 30).unwrap();
        assert!((p - 0.50).abs() < 1e-12, "expected 0.50, got {}", p);
        // A quarter of the way up that same segment.
        let p = m.fair_value(Side::Up, -0.025, 30).unwrap();
        assert!((p - 0.40).abs() < 1e-12, "expected 0.40, got {}", p);
    }

    #[test]
    fn flat_beyond_outermost_centers() {
        let m = model();
        assert!((m.fair_value(Side::Up, -5.0, 30).unwrap() - 0.10).abs() < 1e-12);
        assert!((m.fair_value(Side::Up, 5.0, 30).unwrap() - 0.90).abs() < 1e-12);
    }

    #[test]
    fn low_confidence_cell_returns_none() {
        let m = model();
        // secs bucket 1, delta bucket 1 has n = 5.
        assert_eq!(m.fair_value(Side::Up, -0.05, 90), None);
        // Its neighbours are still priceable.
        assert!(m.fair_value(Side::Up, 0.05, 90).is_some());
    }

    #[test]
    fn repeated_rows_cannot_pass_independent_window_gate() {
        let mut m = model();
        m.grid[0][2].n = 10_000;
        m.grid[0][2].w = 1;
        m.grid[0][2].low_conf = false;
        assert_eq!(m.fair_value(Side::Up, 0.05, 30), None);
        assert_eq!(m.coverage().usable_cells, 10);
    }

    #[test]
    fn interpolation_never_borrows_probability_from_a_thin_neighbor() {
        let m = model();
        assert_eq!(m.fair_value(Side::Up, 0.025, 90), None);
        assert_eq!(m.fair_value(Side::Up, -0.11, 90), None);
        // Exact centers use no weight from the neighboring cell.
        assert_eq!(m.fair_value(Side::Up, 0.05, 90), Some(0.60));
    }

    #[test]
    fn malformed_axes_and_window_counts_are_rejected() {
        let mut m = model();
        m.delta_edges[0] = f64::NAN;
        assert!(m.validate().is_err());
        let mut m = model();
        m.delta_edges.swap(0, 1);
        assert!(m.validate().is_err());
        let mut m = model();
        m.delta_centers[0] = 0.0;
        assert!(m.validate().is_err());
        let mut m = model();
        m.grid[0][0].w = 101;
        assert!(m.validate().is_err());
        let mut m = model();
        m.min_n = 0;
        assert!(m.validate().is_err());
    }

    #[test]
    fn v2_requires_and_uses_independent_window_threshold() {
        let mut m = model();
        m.version = 2;
        assert!(m.validate().is_err());
        m.min_windows = Some(51);
        m.validate().unwrap();
        assert_eq!(m.fair_value(Side::Up, 0.05, 30), None);
        assert_eq!(m.coverage().usable_cells, 0);
    }

    #[test]
    fn secs_left_outside_range_returns_none() {
        let m = model();
        assert_eq!(m.fair_value(Side::Up, 0.05, -1), None);
        assert_eq!(m.fair_value(Side::Up, 0.05, 301), None);
        assert_eq!(m.fair_value(Side::Up, 0.05, 9999), None);
        // The last bucket is [240, 301), so 300 is inside.
        assert!(m.fair_value(Side::Up, 0.05, 300).is_some());
    }

    #[test]
    fn non_finite_delta_returns_none() {
        let m = model();
        assert_eq!(m.fair_value(Side::Up, f64::NAN, 30), None);
        assert_eq!(m.fair_value(Side::Up, f64::INFINITY, 30), None);
    }

    #[test]
    fn monotone_in_delta_for_up_and_reversed_for_down() {
        let m = model();
        let mut prev_up = f64::NEG_INFINITY;
        let mut prev_down = f64::INFINITY;
        let mut d = -0.30;
        while d <= 0.30 {
            if let Some(up) = m.fair_value(Side::Up, d, 30) {
                assert!(up >= prev_up - 1e-12, "Up not monotone at {}", d);
                prev_up = up;
            }
            if let Some(down) = m.fair_value(Side::Down, d, 30) {
                assert!(down <= prev_down + 1e-12, "Down not anti-monotone at {}", d);
                prev_down = down;
            }
            d += 0.005;
        }
    }

    #[test]
    fn validate_rejects_non_monotone_grid() {
        let bad = model_json().replace(
            r#"{"n":100,"w":50,"p":0.70,"low_conf":false}"#,
            r#"{"n":100,"w":50,"p":0.20,"low_conf":false}"#,
        );
        let m: FairValueModel = serde_json::from_str(&bad).expect("parse");
        let err = m.validate().expect_err("must reject non-monotone grid");
        assert!(err.contains("not monotone"), "unexpected error: {}", err);
    }

    #[test]
    fn validate_rejects_wrong_reference_side() {
        let bad = model_json().replace(r#""reference_side": "Up""#, r#""reference_side": "Down""#);
        let m: FairValueModel = serde_json::from_str(&bad).expect("parse");
        assert!(m.validate().is_err());
    }

    #[test]
    fn validate_rejects_shape_mismatch() {
        let bad = model_json().replace(
            r#""delta_centers": [-0.15, -0.05, 0.05, 0.15]"#,
            r#""delta_centers": [-0.15, -0.05, 0.05]"#,
        );
        let m: FairValueModel = serde_json::from_str(&bad).expect("parse");
        assert!(m.validate().is_err());
    }

    #[test]
    fn validate_rejects_out_of_range_probability() {
        let bad = model_json().replace(
            r#"{"n":100,"w":50,"p":0.90,"low_conf":false}"#,
            r#"{"n":100,"w":50,"p":1.40,"low_conf":false}"#,
        );
        let m: FairValueModel = serde_json::from_str(&bad).expect("parse");
        assert!(m.validate().is_err());
    }

    #[test]
    fn load_missing_file_is_an_error_not_a_fallback() {
        let err =
            load("definitely-not-a-real-fairvalue-file.json").expect_err("missing file must fail");
        assert!(err.contains("cannot read"), "unexpected error: {}", err);
    }

    #[test]
    fn side_parse_roundtrip() {
        assert_eq!(Side::parse("Up"), Some(Side::Up));
        assert_eq!(Side::parse("Down"), Some(Side::Down));
        assert_eq!(Side::parse("up"), None);
        assert_eq!(Side::Up.as_str(), "Up");
        assert_eq!(Side::Down.as_str(), "Down");
    }
}

#[cfg(test)]
mod shipped_curve_tests {
    use super::*;

    /// Loads the curve actually produced by `tools/build_fairvalue.py` and
    /// prints what the bot would price at a spread of realistic states.
    ///
    /// Ignored by default because it depends on a generated file. Run it after
    /// rebuilding the curve, before deploying it:
    ///
    ///   FAIRVALUE_FILE=fairvalue.json cargo test -- --ignored --nocapture
    #[test]
    #[ignore = "needs a generated fairvalue.json; run after rebuilding the curve"]
    fn shipped_curve_loads_and_prices() {
        let path = std::env::var("FAIRVALUE_FILE").unwrap_or_else(|_| "fairvalue.json".into());
        let m = load(&path).expect("the shipped curve must load and validate");

        let c = m.coverage();
        println!(
            "coverage: {}/{} cells usable, {} rows, {} cell-window obs",
            c.usable_cells, c.total_cells, c.total_rows, c.total_windows
        );
        assert!(
            c.usable_cells > 0,
            "no usable cells: the bot could never quote"
        );

        println!(
            "\n{:>6}  {:>8}  {:>10}  {:>10}",
            "secs", "delta", "P(Up)", "P(Down)"
        );
        let mut priced = 0;
        for secs in [15, 45, 75, 105, 150, 210, 270] {
            for delta in [-0.20, -0.12, -0.08, -0.06, 0.0, 0.06, 0.08, 0.12, 0.20] {
                match (
                    m.fair_value(Side::Up, delta, secs),
                    m.fair_value(Side::Down, delta, secs),
                ) {
                    (Some(up), Some(down)) => {
                        priced += 1;
                        assert!(
                            (up + down - 1.0).abs() < 1e-9,
                            "Up/Down must be complements"
                        );
                        println!("{:>6}  {:>+8.3}  {:>10.4}  {:>10.4}", secs, delta, up, down);
                    }
                    _ => {
                        println!("{:>6}  {:>+8.3}  {:>10}  {:>10}", secs, delta, "-", "-");
                    }
                }
            }
        }
        println!("\npriced {} of {} probe points", priced, 7 * 9);
        assert!(
            priced > 0,
            "the curve priced nothing at any realistic state"
        );

        // Monotonicity holds across the interpolated curve, not just the cells.
        for secs in [15, 45, 75, 105, 150, 210, 270] {
            let mut prev = f64::NEG_INFINITY;
            let mut d = -0.40;
            while d <= 0.40 {
                if let Some(p) = m.fair_value(Side::Up, d, secs) {
                    assert!(
                        p >= prev - 1e-9,
                        "interpolated curve is not monotone at secs={} delta={}",
                        secs,
                        d
                    );
                    prev = p;
                }
                d += 0.005;
            }
        }
    }
}
