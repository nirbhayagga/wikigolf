"""
03_name_clusters.py — LLM Semantic Community Naming via Gemini API
Reads config.yaml for model and rate limit settings.
Saves progress after every call (crash-resilient).
"""

import os
import time
import json
import yaml
import pandas as pd
from tqdm import tqdm
from google import genai


def load_config():
    defaults = {"model": "gemini-2.5-flash", "rate_limit_sleep": 5}
    try:
        with open("config.yaml") as f:
            user = yaml.safe_load(f)
        if "gemini" in user:
            defaults.update(user["gemini"])
        if "community" in user:
            defaults["top_n"] = user["community"].get("top_n", 20)
        else:
            defaults["top_n"] = 20
    except FileNotFoundError:
        defaults["top_n"] = 20
    return defaults


def main():
    t0 = time.time()

    if "GEMINI_API_KEY" not in os.environ:
        print("Skipping: GEMINI_API_KEY not set.")
        print("  export GEMINI_API_KEY='your_key'")
        return

    config = load_config()
    print(f"Generating labels for top {config['top_n']} communities...")
    print(f"  Model: {config['model']}, Rate limit: {config['rate_limit_sleep']}s\n")

    nodes_df = pd.read_parquet("data/nodes.parquet")
    top_communities = nodes_df['community'].value_counts().head(config['top_n']).index

    client = genai.Client()

    labels_path = "data/community_labels.json"
    if os.path.exists(labels_path):
        with open(labels_path, "r") as f:
            labels = json.load(f)
        print(f"  Resuming from {len(labels)} cached labels")
    else:
        labels = {}

    pbar = tqdm(top_communities, desc="Labeling communities",
                unit="community", total=len(top_communities))
    for comm_id in pbar:
        key = str(comm_id)
        if key in labels:
            pbar.set_postfix_str(f"{labels[key]} (cached)")
            continue

        cluster = nodes_df[nodes_df['community'] == comm_id]
        top_articles = cluster.nlargest(20, 'pagerank')['vertex'].tolist()

        prompt = (
            "Identify a 2-to-4 word categorical description summarizing "
            "this cluster of related Wikipedia pages. Return ONLY the "
            f"title phrase itself, nothing else: {top_articles}"
        )

        try:
            response = client.models.generate_content(
                model=config['model'], contents=prompt,
            )
            label = response.text.strip().replace('"', '')
            pbar.set_postfix_str(label)
            labels[key] = label
        except Exception as e:
            pbar.set_postfix_str(f"FAILED: {e}")
            labels[key] = f"Cluster {comm_id}"

        with open(labels_path, "w") as f:
            json.dump(labels, f, indent=2)

        time.sleep(config['rate_limit_sleep'])

    secs = time.time() - t0
    print(f"\nLabeled {len(labels)} communities in {secs:.0f}s -> {labels_path}")


if __name__ == "__main__":
    main()