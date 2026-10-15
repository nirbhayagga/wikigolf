import sys
import unittest
from pathlib import Path
import numpy as np
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from viewer_index import GridIndex


class NearestTests(unittest.TestCase):
    def test_matches_brute_force_at_cell_boundaries(self):
        rng = np.random.default_rng(42)
        x, y = rng.normal(size=(2, 300))
        index = GridIndex(x, y)
        for qx, qy in rng.uniform(-4, 4, size=(50, 2)):
            want = int(np.argmin((x-qx)**2+(y-qy)**2))
            self.assertEqual(index.nearest(qx, qy), want)

    def test_isolation_excludes_hidden_articles(self):
        index = GridIndex(np.array([0., 1., 2.]), np.array([0., 0., 0.]))
        self.assertEqual(index.nearest(0, 0, np.array([False, True, True])), 1)
        self.assertIsNone(index.nearest(0, 0, np.zeros(3, dtype=bool)))

    def test_empty_and_nonfinite_coordinates(self):
        index = GridIndex(np.array([np.nan]), np.array([np.nan]))
        self.assertIsNone(index.nearest(0, 0))
