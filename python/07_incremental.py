"""
07_incremental.py — Detect edge list changes and manage cache invalidation.
Usage:
    python python/07_incremental.py          # Check if edges.csv changed
    python python/07_incremental.py --reset  # Delete all caches
    python python/07_incremental.py --status # Show cache status
"""

import os
import argparse

DATA_DIR = "data"

CACHES = {
    "Phase 0 (String Mapping)": [
        "cache_edges_int.parquet", "cache_mapping.parquet", "cache_edges_hash.txt"
    ],
    "Phase 1 (GPU Directed)": [
        "cache_directed_done", "cache_pagerank.parquet", "cache_in_degree.parquet"
    ],
    "Phase 2 (GPU Layout + Communities)": [
        "cache_layout_done", "cache_layout.parquet"
    ],
    "Final Output": [
        "nodes.parquet", "edges.parquet"
    ],
}


def file_sig(path):
    """Get file size as a simple change indicator."""
    if not os.path.exists(path):
        return None
    return os.path.getsize(path)


def check_changes():
    """Check if edges.csv has changed since last run."""
    csv_path = os.path.join(DATA_DIR, "edges.csv")
    hash_path = os.path.join(DATA_DIR, "cache_edges_hash.txt")

    if not os.path.exists(csv_path):
        print("No edges.csv found. Run the Rust parser first.")
        return False

    current_sig = str(file_sig(csv_path))

    if not os.path.exists(hash_path):
        print("No previous run detected. All phases will run fresh.")
        return True

    with open(hash_path) as f:
        cached_sig = f.read().strip()

    if current_sig != cached_sig:
        print(f"edges.csv has CHANGED (size: {cached_sig} → {current_sig})")
        print("Caches are stale. Run with --reset then re-run 01_graph_compute.py")
        return True
    else:
        print("edges.csv has NOT changed. Caches are valid.")
        return False


def show_status():
    """Show which cache files exist."""
    print(f"\nCache Status ({DATA_DIR}/):\n")
    for phase, files in CACHES.items():
        print(f"  {phase}:")
        for fname in files:
            path = os.path.join(DATA_DIR, fname)
            if os.path.exists(path):
                size_mb = os.path.getsize(path) / (1024 * 1024)
                print(f"    ✓ {fname:40s} {size_mb:>8.1f} MB")
            else:
                print(f"    ✗ {fname:40s} (missing)")
        print()


def reset_caches():
    """Delete all cache files."""
    count = 0
    for _, files in CACHES.items():
        for fname in files:
            path = os.path.join(DATA_DIR, fname)
            if os.path.exists(path):
                os.remove(path)
                print(f"   Deleted {path}")
                count += 1
    print(f"\n   Cleared {count} cache files.")


def main():
    parser = argparse.ArgumentParser(description="Wiki-Graph Cache Manager")
    parser.add_argument('--reset', action='store_true', help='Delete all caches')
    parser.add_argument('--status', action='store_true', help='Show cache status')
    args = parser.parse_args()

    if args.reset:
        reset_caches()
    elif args.status:
        show_status()
    else:
        check_changes()
        print()
        show_status()


if __name__ == "__main__":
    main()
