"""Regressions for independent samples, temporal leakage and thin-cell pricing."""
import sqlite3
import tempfile
import unittest
from pathlib import Path
from contextlib import closing

import build_fairvalue as fv


def sample(window, outcome="Up", delta=0.005, secs=90, midpoint=0.5):
    return (window, (window + 300 - secs) * 1000, delta, secs, outcome, midpoint)


class FairValueTests(unittest.TestCase):
    def test_repeating_one_window_never_manufactures_confidence(self):
        model = fv.fit_samples([sample(300)] * 1000, 20, 20)
        cell = model["grid"][fv.secs_bucket(90)][fv.delta_bucket(0.005)]
        self.assertEqual(cell["n"], 1000)
        self.assertEqual(cell["w"], 1)
        self.assertTrue(cell["low_conf"])
        self.assertIsNone(fv.predict(model, 0.005, 90))

    def test_logging_cadence_cannot_change_fitted_probabilities(self):
        samples = [sample(300, "Up"), sample(600, "Down")]
        once = fv.fit_samples(samples, 1, 1)
        duplicated = fv.fit_samples(samples + [samples[0]] * 1000, 1, 1)
        self.assertEqual([[c["p"] for c in r] for r in once["grid"]],
                         [[c["p"] for c in r] for r in duplicated["grid"]])

    def test_holdout_label_changes_cannot_affect_training_model(self):
        samples = [sample(i * 300) for i in range(1, 17)]
        train, test = fv.split_windows(samples, 0.25, 1)
        last_train, first_test = max(r[0] for r in train), min(r[0] for r in test)
        self.assertLess(last_train + 300, first_test)
        self.assertFalse({r[0] for r in train} & {r[0] for r in test})
        flipped = [r[:4] + ("Down", r[5]) if r[0] >= first_test else r
                   for r in samples]
        new_train, new_test = fv.split_windows(flipped, 0.25, 1)
        self.assertEqual(fv.fit_samples(train, 1, 1), fv.fit_samples(new_train, 1, 1))
        self.assertNotEqual(test, new_test)

    def test_delayed_training_resolution_is_excluded(self):
        samples = [sample(i * 300) for i in range(1, 17)]
        arrivals = {r[0]: (r[0] + 450) * 1000 for r in samples}
        arrivals[300] = 999999999
        train, test = fv.split_windows(samples, 0.25, 1, arrivals)
        self.assertNotIn(300, {r[0] for r in train})
        self.assertTrue(all(arrivals[r[0]] < min(t[0] for t in test) * 1000 for r in train))

    def test_interpolation_requires_both_contributing_cells(self):
        model = fv.fit_samples([sample(300)], 1, 1)
        self.assertIsNotNone(fv.predict(model, 0.005, 90))
        self.assertIsNone(fv.predict(model, 0.004, 90))
        self.assertIsNone(fv.predict(model, float("nan"), 90))

    def test_scores_weight_windows_equally(self):
        one_good = (300, 0.99, 1)
        one_bad = (600, 0.01, 1)
        once = fv.probability_scores([one_good, one_bad])
        repeated = fv.probability_scores([one_good] * 100 + [one_bad])
        self.assertAlmostEqual(once["brier"], repeated["brier"])
        self.assertAlmostEqual(once["log_loss"], repeated["log_loss"])

    def test_conflicting_window_resolutions_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sample.db"
            with closing(sqlite3.connect(path)) as conn:
                conn.execute("CREATE TABLE delta_samples (window_ts INTEGER, timestamp_ms INTEGER, "
                             "twap_delta_pct REAL, secs_left INTEGER, actual_resolution TEXT, "
                             "up_bid REAL, up_ask REAL)")
                conn.executemany("INSERT INTO delta_samples VALUES (?, ?, ?, ?, ?, ?, ?)", [
                    (300, 510000, 0.005, 90, "Up", 0.4, 0.6),
                    (300, 511000, 0.005, 89, "Down", 0.4, 0.6),
                ])
                conn.commit()
            with self.assertRaisesRegex(ValueError, "conflicting resolution"):
                fv.read_samples(str(path), "delta_samples", 0)

    def test_default_builder_uses_unconditional_data_and_preserves_holdout(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sample.db"
            with closing(sqlite3.connect(path)) as conn:
                conn.execute("CREATE TABLE delta_samples (window_ts INTEGER, timestamp_ms INTEGER, "
                             "twap_delta_pct REAL, secs_left INTEGER, actual_resolution TEXT, "
                             "up_bid REAL, up_ask REAL)")
                conn.executemany("INSERT INTO delta_samples VALUES (?, ?, ?, ?, ?, ?, ?)", [
                    (i * 300, (i * 300 + 210) * 1000, 0.005, 90,
                     "Up" if i % 2 else "Down", 0.4, 0.6)
                    for i in range(1, 17)
                ])
                conn.commit()
            model = fv.build(str(path), min_n=1, min_windows=1)
            self.assertEqual(model["source_table"], "delta_samples")
            self.assertEqual(model["validation"]["training_windows"], 11)
            self.assertEqual(model["validation"]["holdout_windows"], 4)
            cell = model["grid"][fv.secs_bucket(90)][fv.delta_bucket(0.005)]
            self.assertEqual(cell["w"], 11)
            self.assertFalse(model["validation"]["profitability_demonstrated"])


if __name__ == "__main__":
    unittest.main()
