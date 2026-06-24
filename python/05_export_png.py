"""
05_export_png.py — Export static high-res PNG of the Wikipedia graph
Renders directly via Datashader (no browser needed).
Usage: python python/05_export_png.py [--width 4096] [--height 2160]
"""

import argparse
import time
import pandas as pd
import json
import numpy as np
import yaml
import datashader as ds
import datashader.transfer_functions as tf
import colorcet as cc
from tqdm import tqdm
from PIL import Image


def load_config():
    defaults = {"width": 4096, "height": 2160}
    try:
        with open("config.yaml") as f:
            user = yaml.safe_load(f)
        if "export" in user:
            defaults.update(user["export"])
    except FileNotFoundError:
        pass
    return defaults


def main():
    config = load_config()
    parser = argparse.ArgumentParser(description="Export Wikipedia Graph as PNG")
    parser.add_argument('--width', type=int, default=config['width'])
    parser.add_argument('--height', type=int, default=config['height'])
    parser.add_argument('--output', default='data/wikipedia_graph.png')
    args = parser.parse_args()

    t0 = time.time()
    print(f"Rendering {args.width}x{args.height} PNG...")

    steps = tqdm(total=7, desc="Export PNG", unit="step")

    nodes = pd.read_parquet("data/nodes.parquet")
    steps.set_postfix_str("loaded nodes")
    steps.update(1)

    # Load community labels
    try:
        with open("data/community_labels.json", "r") as f:
            labels = json.load(f)
    except FileNotFoundError:
        labels = {}

    nodes['label'] = nodes['community'].astype(str).map(labels).fillna("Other")
    nodes['community'] = nodes['community'].astype(str).astype('category')
    steps.set_postfix_str("mapped communities")
    steps.update(1)

    print(f"   {len(nodes):,} points, {nodes['community'].nunique()} communities")

    # Render via Datashader
    cvs = ds.Canvas(plot_width=args.width, plot_height=args.height)
    agg = cvs.points(nodes, 'x', 'y', agg=ds.count_cat('community'))
    steps.set_postfix_str("aggregated")
    steps.update(1)

    # Build color key from community categories
    cats = list(nodes['community'].cat.categories)
    palette = cc.glasbey_dark
    color_key = {cat: palette[i % len(palette)] for i, cat in enumerate(cats)}

    img = tf.shade(agg, color_key=color_key, min_alpha=100)
    img = tf.set_background(img, "black")
    steps.set_postfix_str("shaded")
    steps.update(1)

    # Convert to PIL and save
    pil_img = img.to_pil()
    steps.set_postfix_str("converted to PIL")
    steps.update(1)

    pil_img.save(args.output, "PNG")
    steps.set_postfix_str(f"saved {args.output}")
    steps.update(1)
    print(f"   Saved → {args.output} ({args.width}x{args.height})")

    # Also export a smaller thumbnail
    thumb_path = args.output.replace('.png', '_thumb.png')
    thumb = pil_img.resize((args.width // 4, args.height // 4), Image.LANCZOS)
    thumb.save(thumb_path, "PNG")
    steps.set_postfix_str(f"saved thumbnail")
    steps.update(1)
    steps.close()
    print(f"   Thumbnail → {thumb_path}")
    print(f"   Export complete in {time.time() - t0:.1f}s")


if __name__ == "__main__":
    main()
