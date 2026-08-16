"""
05_export_png.py — Export a high-resolution PNG of the graph.

Renders directly through Datashader; no browser involved.
Usage: python python/05_export_png.py [--width 4096] [--height 2160]
"""

import argparse
import json
import os
import sys
import time

import numpy as np
import pandas as pd

import colorcet as cc
import datashader as ds
import datashader.transfer_functions as tf
from PIL import Image

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import Paths, load_config  # noqa: E402

# See 04_app.py: the categorical aggregate costs width * height * categories
# * 4 bytes. At 4096x2160 that is ~35 MB per category, so an unbounded
# category count (Leiden finds thousands) would ask for hundreds of GB.
MAX_CATEGORIES = 24
OTHER = "Other"


def _transfer(total, occupied, how):
    """Map counts to 0..1 brightness, the way tf.shade's `how` does."""
    t = np.zeros_like(total)
    v = total[occupied]
    if how == "eq_hist":
        order = v.argsort()
        ranks = np.empty(len(v))
        ranks[order] = np.arange(len(v))
        t[occupied] = (ranks + 1) / len(v)
    elif how == "log":
        ref = np.log1p(np.percentile(v, 99.5))
        t[occupied] = np.clip(np.log1p(v) / max(ref, 1e-9), 0, 1)
    elif how == "cbrt":
        ref = np.percentile(v, 99.5) ** (1 / 3)
        t[occupied] = np.clip(v ** (1 / 3) / max(ref, 1e-9), 0, 1)
    else:
        ref = np.percentile(v, 99.5)
        t[occupied] = np.clip(v / max(ref, 1e-9), 0, 1)
    return t


def _shade_winner(agg, cats, palette, total, occupied, args):
    """Colour each pixel by the community that dominates it.

    datashader's `tf.shade` with a `color_key` mixes the categories present in
    a pixel in proportion to their counts. That is right for a handful of
    categories and wrong for a hundred: at enwiki density a pixel holds
    articles from many communities at once, and the count-weighted mean of many
    hues is grey. Raising `community.resolution` to get legible regions made
    the render *worse* for exactly this reason.

    Taking the dominant category instead keeps every pixel at full saturation
    and shows where one community actually owns the ground. Measured on the
    resolution-6 layout, the winner holds 57% of its pixel on average, so this
    is a real majority rather than an arbitrary pick — but neighbouring regions
    do interpenetrate, which is a fact about the layout, not the render.
    """
    from PIL import Image as PILImage

    counts = agg.data                       # (h, w, ncat)
    who = counts.argmax(axis=2)
    best = counts.max(axis=2)

    rgb = np.array(
        [[int(palette[i % len(palette)].lstrip("#")[j:j + 2], 16) for j in (0, 2, 4)]
         for i in range(len(cats))],
        dtype=np.float32,
    )

    share = np.zeros_like(total)
    share[occupied] = best[occupied] / total[occupied]
    print(f"   winner-take-all: dominant community holds {share[occupied].mean():.2f} "
          f"of its pixel on average, clear majority in "
          f"{100 * (share[occupied] > 0.5).mean():.1f}% of them")

    t = _transfer(total, occupied, args.how)
    if args.min_alpha:
        # Same role as tf.shade's min_alpha: keep the faintest occupied pixel
        # visible rather than letting it fade to the background.
        floor = args.min_alpha / 255.0
        t[occupied] = floor + (1 - floor) * t[occupied]

    img = (rgb[who] * t[..., None]).clip(0, 255).astype(np.uint8)
    img[~occupied] = 0
    # dynspread works on a datashader Image, and is pointless here anyway:
    # spreading exists for sparse canvases, and this one is ~70% occupied.
    return PILImage.fromarray(img)


def main():
    cfg = load_config()
    exp = cfg["export"]

    ap = argparse.ArgumentParser(description="Export the Wikipedia graph as a PNG")
    ap.add_argument("--width", type=int, default=exp["width"])
    ap.add_argument("--height", type=int, default=exp["height"])
    ap.add_argument("--max-categories", type=int,
                    default=exp.get("max_categories", MAX_CATEGORIES))
    ap.add_argument("--output", default=None)
    ap.add_argument("--data-dir", default=None, help="override pipeline.data_dir")
    ap.add_argument("--percentile", type=float, default=0.4,
                    help="frame between this and (100-this) percentile of coords, "
                         "so outliers and the fragment ring cannot dominate")
    # Defaults are for a *dense* map. eq_hist rank-equalizes the occupied
    # pixels, which is right when most of the canvas is empty and wrong here:
    # on enwiki 70% of pixels hold at least one article, so equalization drags
    # the median pixel to mid-brightness and the whole frame washes out to a
    # flat smear. log keeps the dynamic range the density actually has.
    ap.add_argument("--how", default="log",
                    choices=["eq_hist", "log", "cbrt", "linear"])
    # Likewise the alpha floor: at 180 every occupied pixel renders at 70%
    # opacity, so empty space and dense cores look nearly the same.
    ap.add_argument("--min-alpha", type=int, default=40)
    ap.add_argument("--max-px", type=int, default=3, help="dynspread max radius")
    ap.add_argument("--no-spread", action="store_true")
    ap.add_argument("--color", default="winner", choices=["winner", "blend"],
                    help="winner: each pixel takes the colour of the community "
                         "holding the most articles in it. blend: datashader's "
                         "count-weighted mean, which greys out as categories rise")
    args = ap.parse_args()

    paths = Paths(args.data_dir or cfg["pipeline"]["data_dir"])
    output = args.output or os.path.join(paths.data_dir, "wikipedia_graph.png")

    if not os.path.exists(paths.nodes):
        raise SystemExit(
            f"{paths.nodes} not found.\n"
            "  Run the pipeline first: python python/01_graph_compute.py"
        )

    t0 = time.time()
    # This is the count_cat aggregate alone. Real peak RSS is several times
    # larger: tf.shade's eq_hist equalization builds float intermediates over
    # the whole category stack. Measured at 4096x2160 — 24 categories
    # (budget 0.9 GB) renders fine, 80 categories (budget 2.9 GB) was
    # OOM-killed on a 15 GB machine with ~7 GB free. Treat the budget as a
    # lower bound and keep roughly 4x it available.
    budget = args.width * args.height * (args.max_categories + 1) * 4 / 1e9
    print(f"Rendering {args.width}x{args.height} (aggregate ~{budget:.1f} GB, "
          f"expect ~{4 * budget:.1f} GB peak)...")

    nodes = pd.read_parquet(paths.nodes)
    print(f"   {len(nodes):,} points")

    try:
        with open(paths.community_labels) as f:
            labels = json.load(f)
    except FileNotFoundError:
        labels = {}

    sizes = nodes["community"].value_counts()
    keep = set(sizes.head(args.max_categories).index)
    name_of = lambda c: labels.get(str(c), f"Cluster {c}")
    cats = [name_of(c) for c in sizes.head(args.max_categories).index] + [OTHER]
    nodes["label"] = pd.Categorical(
        [name_of(c) if c in keep else OTHER for c in nodes["community"]],
        categories=cats,
    )
    print(f"   {len(sizes):,} communities → {len(cats)} categories")

    # Frame on a percentile, not on min/max.
    #
    # The layout deliberately parks disconnected fragments on an outer ring, and
    # a few weakly-attached articles sit far out too. Letting those set the
    # bounds spent ~96% of the frame on 0.5% of the articles and rendered the
    # actual map as a smudge. Percentile bounds are also generically safer: any
    # future layout with outliers gets framed sensibly without special-casing.
    x, y = nodes["x"].to_numpy(), nodes["y"].to_numpy()
    lo, hi = args.percentile, 100 - args.percentile
    xr = (np.percentile(x, lo), np.percentile(x, hi))
    yr = (np.percentile(y, lo), np.percentile(y, hi))
    pad = 0.02 * max(xr[1] - xr[0], yr[1] - yr[0])
    xr = (xr[0] - pad, xr[1] + pad)
    yr = (yr[0] - pad, yr[1] + pad)
    shown = ((x >= xr[0]) & (x <= xr[1]) & (y >= yr[0]) & (y <= yr[1])).sum()
    print(f"   framing {shown:,} of {len(nodes):,} points "
          f"({100 * shown / len(nodes):.1f}%) at the {lo}-{hi} percentile")

    cvs = ds.Canvas(plot_width=args.width, plot_height=args.height,
                    x_range=xr, y_range=yr)
    agg = cvs.points(nodes, "x", "y", agg=ds.count_cat("label"))

    # Report what the layout actually produced, because a bad layout and a bad
    # render look identical in the output PNG. A force layout with real
    # structure has a heavy-tailed density: the busiest cell holds tens or
    # hundreds of times the mean and the median cell is well below it. Uniform
    # random points score max/mean ~2 and median/mean ~1 — if these numbers
    # land there, the graph was too dense to unfold and no render setting will
    # rescue it.
    total = agg.data.sum(axis=2).astype(np.float64)
    occupied = total > 0
    mean = total.mean()
    print(f"   density: {100 * occupied.mean():.1f}% of pixels occupied, "
          f"max/mean {total.max() / mean:.1f}x, "
          f"median/mean {np.median(total) / mean:.2f}")
    if args.how == "eq_hist" and occupied.mean() > 0.25:
        print("   WARNING: eq_hist on a canvas this full flattens the image; "
              "try --how log")

    # glasbey_light, not glasbey_dark: these are dark colours on a black
    # background, which is why the first renders looked washed out.
    palette = cc.glasbey_light
    color_key = {c: palette[i % len(palette)] for i, c in enumerate(cats)}

    if args.color == "winner":
        pil = _shade_winner(agg, cats, palette, total, occupied, args)
    else:
        img = tf.shade(agg, color_key=color_key, how=args.how,
                       min_alpha=args.min_alpha)
        if not args.no_spread:
            # Most pixels hold 0-1 points at this density, so isolated articles
            # render as invisible single pixels without spreading.
            img = tf.dynspread(img, threshold=0.35, max_px=args.max_px)
        img = tf.set_background(img, "black")
        pil = img.to_pil()

    pil.save(output, "PNG")
    print(f"   Saved → {output}")

    thumb_path = output.replace(".png", "_thumb.png")
    pil.resize((args.width // 4, args.height // 4), Image.LANCZOS).save(thumb_path, "PNG")
    print(f"   Thumbnail → {thumb_path}")
    print(f"   Done in {time.time() - t0:.1f}s")


if __name__ == "__main__":
    main()
