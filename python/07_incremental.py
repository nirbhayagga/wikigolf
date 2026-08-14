"""
07_incremental.py — Cache status and invalidation.

Usage:
    python python/07_incremental.py            # status + staleness check
    python python/07_incremental.py --status   # status only
    python python/07_incremental.py --reset    # delete pipeline caches
"""

import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import Paths, fingerprint, load_config, reset_caches  # noqa: E402


def show_status(paths):
    groups = {
        "Parser output": [
            ("titles.parquet", paths.titles),
            ("redirects.parquet", paths.redirects),
            ("edges.parquet", paths.edges),
        ],
        "Pipeline caches": [
            ("manifest.json", paths.manifest),
            ("cache_metrics.parquet", paths.cache_metrics),
            ("cache_layout.parquet", paths.cache_layout),
        ],
        "Final output": [
            ("nodes.parquet", paths.nodes),
            ("community_labels.json", paths.community_labels),
            ("community_stats.json", paths.community_stats),
        ],
    }
    print(f"\nStatus ({paths.data_dir}/):\n")
    for group, files in groups.items():
        print(f"  {group}:")
        for name, path in files:
            if os.path.exists(path):
                mb = os.path.getsize(path) / (1024 * 1024)
                print(f"    ✓ {name:<26} {mb:>9.1f} MB")
            else:
                print(f"    ✗ {name:<26}   (missing)")
        print()


def check_staleness(paths, cfg):
    """Compare the recorded input fingerprint against the inputs on disk."""
    if not os.path.exists(paths.edges) or not os.path.exists(paths.titles):
        print("No parser output found. Run wiki-parser first.")
        return
    if not os.path.exists(paths.manifest):
        print("No manifest — the pipeline has not run against these inputs yet.")
        return

    with open(paths.manifest) as f:
        stored = json.load(f)
    current = fingerprint(paths, stored.get("sample_ratio", 1.0), cfg)

    diff = [k for k in current if stored.get(k) != current[k]]
    if diff:
        print("Inputs have CHANGED since the caches were built:")
        for k in diff:
            print(f"   {k}: {stored.get(k)} → {current[k]}")
        print("\nCaches are stale. Run: python python/01_graph_compute.py --reset")
    else:
        print("Inputs unchanged. Caches are valid.")


def main():
    ap = argparse.ArgumentParser(description="Wiki-Graph cache manager")
    ap.add_argument("--reset", action="store_true", help="delete pipeline caches")
    ap.add_argument("--status", action="store_true", help="show status only")
    ap.add_argument("--data-dir", default=None)
    args = ap.parse_args()

    cfg = load_config()
    paths = Paths(args.data_dir or cfg["pipeline"]["data_dir"])

    if args.reset:
        reset_caches(paths)
    elif args.status:
        show_status(paths)
    else:
        check_staleness(paths, cfg)
        show_status(paths)


if __name__ == "__main__":
    main()
