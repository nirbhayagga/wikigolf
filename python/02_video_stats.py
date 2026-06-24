"""
02_video_stats.py — Orphans, Dead Ends, and Dead-End Orphans
Uses PyArrow C++ for memory-efficient edge scanning.
"""

import time
import pandas as pd
import pyarrow.parquet as pq
import pyarrow.compute as pc
from tqdm import tqdm


def main():
    t0 = time.time()
    print("Analyzing Graph Topology...\n")

    with tqdm(total=5, desc="Overall", unit="step") as overall:
        nodes_df = pd.read_parquet("data/nodes.parquet")
        print(f"Total articles: {len(nodes_df):,}")
        overall.update(1)

        # Warn if data looks sampled (edges much fewer than expected for node count)
        edge_count = pq.read_metadata("data/edges.parquet").num_rows
        ratio = edge_count / max(len(nodes_df), 1)
        if ratio < 5.0 and len(nodes_df) > 100_000:
            print("   ⚠ WARNING: edges.parquet appears to be from a sampled run.")
            print(f"   Edge/node ratio = {ratio:.1f} (expected ~50 for full Wikipedia).")
            print("   Dead-end / orphan analysis may be inaccurate.\n")

        # Orphans: zero inbound links
        orphans = nodes_df[nodes_df['degree'] == 0]
        print(f"  Orphans (no inbound links):          {len(orphans):>10,}")
        overall.update(1)

        # Dead Ends: never appear as Source
        print("Scanning edge list for unique sources (PyArrow C++)...")
        arrow_table = pq.read_table("data/edges.parquet", columns=['Source'])
        sources = set(pc.unique(arrow_table['Source']).to_pylist())
        overall.update(1)

        all_vertices = set(nodes_df['vertex'].unique())
        dead_ends = all_vertices - sources
        print(f"  Dead Ends (no outbound links):       {len(dead_ends):>10,}")

        dead_end_orphans = set(orphans['vertex']).intersection(dead_ends)
        print(f"  Dead-End Orphans (fully isolated):   {len(dead_end_orphans):>10,}")
        overall.update(1)

        # Top orphans
        print(f"\nTop orphans by PageRank:")
        for _, row in orphans.nlargest(10, 'pagerank').iterrows():
            print(f"   {row['vertex']:50s} PR={row['pagerank']:.2e}")
        overall.update(1)

    secs = time.time() - t0
    print(f"\nCompleted in {secs:.1f}s")


if __name__ == "__main__":
    main()