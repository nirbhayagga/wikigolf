"""
06_community_stats.py — Export per-community statistics
Generates data/community_stats.json and prints a summary table.
"""

import pandas as pd
import json
from tqdm import tqdm


def main():
    print("Generating community statistics...")
    nodes = pd.read_parquet("data/nodes.parquet")

    # Load labels if available
    try:
        with open("data/community_labels.json", "r") as f:
            labels = json.load(f)
    except FileNotFoundError:
        labels = {}

    # Vectorized aggregates — single pass over the DataFrame
    print("   Computing per-community aggregates...")
    agg = nodes.groupby('community').agg(
        size=('vertex', 'size'),
        avg_pagerank=('pagerank', 'mean'),
        max_pagerank=('pagerank', 'max'),
        avg_degree=('degree', 'mean'),
    )
    agg = agg.sort_values('size', ascending=False)

    # Get top 10 articles per community in a single pass (not per-community filter)
    print("   Finding top articles per community...")
    top_per_comm = (nodes.sort_values('pagerank', ascending=False)
                    .groupby('community')['vertex']
                    .apply(lambda x: x.head(10).tolist()))

    # Build stats list
    stats = []
    for comm_id in tqdm(agg.index, desc="Communities",
                        unit="community", total=len(agg)):
        if pd.isna(comm_id):
            continue
        row = agg.loc[comm_id]
        label = labels.get(str(int(comm_id)), f"Cluster {int(comm_id)}")
        top_articles = top_per_comm.get(comm_id, [])

        stats.append({
            "community_id": int(comm_id),
            "label": label,
            "size": int(row['size']),
            "pct_of_total": round(row['size'] / len(nodes) * 100, 2),
            "avg_pagerank": float(row['avg_pagerank']),
            "max_pagerank": float(row['max_pagerank']),
            "avg_degree": float(row['avg_degree']),
            "top_articles": top_articles,
        })

    # Save full stats
    with open("data/community_stats.json", "w") as f:
        json.dump(stats, f, indent=2)

    # Print summary table
    print(f"\n{'ID':>6}  {'Size':>10}  {'%':>6}  {'Avg PR':>10}  {'Label'}")
    print("-" * 70)
    for s in stats[:30]:
        print(f"{s['community_id']:>6}  {s['size']:>10,}  {s['pct_of_total']:>5.1f}%"
              f"  {s['avg_pagerank']:>10.2e}  {s['label']}")

    print(f"\nTotal communities: {len(stats)}")
    print(f"Saved → data/community_stats.json")


if __name__ == "__main__":
    main()
