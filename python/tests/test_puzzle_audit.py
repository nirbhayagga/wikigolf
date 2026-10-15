"""Regression checks for the export auditor's claims about route legality."""
import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tools'))
from audit_puzzles import build_report, route_errors, summarize  # noqa: E402


class PuzzleAuditTests(unittest.TestCase):
    def test_fixture_routes_and_duplicate_accounting(self):
        report = build_report(ROOT / 'e2e/fixtures')
        self.assertTrue(report['review'])
        self.assertTrue(all(i['route_verified'] for i in report['review']))
        row = report['review'][0]['puzzle']
        summary = summarize([row, row])
        self.assertEqual(summary['races'] - summary['unique_pairs'], 1)
        bad = dict(row, par=row['par'] + 1)
        self.assertIn('Baked route length differs from par', route_errors(bad))

    def test_missing_link_and_disambiguation_are_detected(self):
        with tempfile.TemporaryDirectory() as tmp:
            site = Path(tmp) / 'site'
            shutil.copytree(ROOT / 'e2e/fixtures', site)
            shard = site / 'shards/0.json'
            data = json.loads(shard.read_text())
            start = next(a for a in data['a'] if a[0] == 0)
            start[10] = []
            start[4] |= 1
            shard.write_text(json.dumps(data))
            report = build_report(site)
            self.assertTrue(all(not i['route_verified'] for i in report['review']))
            errors = [e for i in report['review'] for e in i['errors']]
            self.assertTrue(any('No exported link' in e for e in errors))
            self.assertTrue(any('Disambiguation endpoint' in e for e in errors))


if __name__ == '__main__':
    unittest.main()
