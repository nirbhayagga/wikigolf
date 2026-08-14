"""
04_app.py — Interactive Wikipedia Graph Web Viewer

Datashader raster + search (including redirect aliases) + community legend.

Run with: panel serve python/04_app.py --show
"""

import json
import os
import sys
import warnings

warnings.filterwarnings("ignore", category=RuntimeWarning)

import numpy as np
import pandas as pd
import pyarrow.compute as pc
import pyarrow.parquet as pq

import colorcet as cc
import datashader as ds
import holoviews as hv
import holoviews.operation.datashader as hd
import panel as pn

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import Paths, load_config  # noqa: E402

hv.extension("bokeh")
pn.extension()

# Datashader's count_cat allocates one aggregate plane per category, so the
# raster costs width * height * categories * 4 bytes. At 1200x800 with the
# ~1,400 communities Leiden finds, that is over 5 GB for a single frame — and
# a legend nobody can read. Keep the largest few and fold the rest into one
# bucket. Override with visualization.max_categories; raise it together with
# community.resolution, or the extra communities all land in "Other".
MAX_CATEGORIES = 24
OTHER = "Other"
MAX_RESULTS = 15


def load_labels(paths):
    try:
        with open(paths.community_labels) as f:
            return json.load(f)
    except FileNotFoundError:
        return {}


def build_categories(nodes, labels, max_categories=MAX_CATEGORIES):
    """Reduce communities to a bounded set of named categories."""
    sizes = nodes["community"].value_counts()
    keep = list(sizes.head(max_categories).index)
    keep_set = set(keep)

    def name_of(cid):
        return labels.get(str(cid), f"Cluster {cid}")

    cat = np.where(
        nodes["community"].isin(keep_set),
        nodes["community"].map(name_of),
        OTHER,
    )
    categories = [name_of(c) for c in keep] + [OTHER]
    nodes["label"] = pd.Categorical(cat, categories=categories)

    covered = int(sizes.head(max_categories).sum())
    print(
        f"   {len(sizes):,} communities → {len(keep)} shown "
        f"({100 * covered / len(nodes):.1f}% of articles), rest as '{OTHER}'"
    )
    return categories


class Search:
    """Prefix-then-substring search over titles and redirect aliases.

    Matching runs in Arrow's C++ kernels rather than pandas' str accessor, so
    a query over millions of titles stays in the tens of milliseconds instead
    of blocking the server for seconds on every search.
    """

    def __init__(self, nodes, paths):
        self.nodes = nodes
        self.titles = pq.read_table(paths.titles, columns=["title"]).column("title")

        self.aliases = None
        self.alias_ids = None
        if os.path.exists(paths.redirects):
            tbl = pq.read_table(paths.redirects, columns=["alias", "article_id"])
            self.aliases = tbl.column("alias")
            self.alias_ids = tbl.column("article_id").to_numpy()
            print(f"   {len(self.aliases):,} redirect aliases searchable")

    def _ids(self, arr, query, ids=None):
        prefix = pc.starts_with(arr, query, ignore_case=True)
        idx = np.flatnonzero(prefix.to_numpy(zero_copy_only=False))
        if len(idx) < MAX_RESULTS:
            sub = pc.match_substring(arr, query, ignore_case=True)
            extra = np.flatnonzero(sub.to_numpy(zero_copy_only=False))
            idx = np.union1d(idx, extra)
        return ids[idx] if ids is not None else idx

    def query(self, text):
        q = text.strip()
        if not q:
            return None, 0

        hits = self._ids(self.titles, q)
        via_alias = np.array([], dtype=np.int64)
        if self.aliases is not None:
            via_alias = self._ids(self.aliases, q, self.alias_ids)

        all_ids = np.union1d(hits, via_alias)
        if len(all_ids) == 0:
            return None, 0

        found = self.nodes.iloc[all_ids]
        return found.nlargest(MAX_RESULTS, "pagerank"), len(all_ids)


def build_app():
    cfg = load_config()
    viz = cfg["visualization"]
    paths = Paths(cfg["pipeline"]["data_dir"])

    if not os.path.exists(paths.nodes):
        raise SystemExit(
            f"{paths.nodes} not found.\n"
            "  Run the pipeline first: python python/01_graph_compute.py"
        )

    print("Loading nodes...")
    nodes = pd.read_parquet(paths.nodes)
    print(f"   {len(nodes):,} nodes")

    labels = load_labels(paths)
    if not labels:
        print("   (no community_labels.json — run 03_name_clusters.py for names)")
    build_categories(nodes, labels, viz.get("max_categories", MAX_CATEGORIES))

    search = Search(nodes, paths)

    points = hv.Points(nodes, kdims=["x", "y"], vdims=["label", "vertex", "pagerank"])
    shaded = hd.datashade(
        points,
        aggregator=ds.count_cat("label"),
        color_key=cc.glasbey_dark,
        min_alpha=100,
    ).opts(
        width=viz["width"],
        height=viz["height"],
        bgcolor=viz["bgcolor"],
        xaxis=None,
        yaxis=None,
        title=viz["title"],
    )

    # inspect_points takes a single element (the datashaded plot) and resolves
    # the original points through its pipeline; hover contents come from the
    # vdims above. It is `max_indicators`, not `n`, and there is no `columns`
    # argument — passing those silently disabled hover entirely.
    try:
        hover = hd.inspect_points(shaded, max_indicators=5).opts(
            tools=["hover"],
            size=8,
            color="white",
            fill_alpha=0,
            line_color="white",
            line_width=2,
        )
        plot = shaded * hover
    except Exception as e:
        import traceback

        print(f"   ⚠ Hover tooltips disabled ({type(e).__name__}: {e})")
        print("     holoviews API drift — check inspect_points signature:")
        traceback.print_exc()
        plot = shaded

    box = pn.widgets.TextInput(
        name="Search article",
        placeholder="title or redirect, e.g. USA",
        width=340,
    )
    button = pn.widgets.Button(name="Find", button_type="primary", width=80)
    results = pn.pane.Markdown("", width=420)

    def do_search(*_):
        top, total = search.query(box.value)
        if top is None:
            results.object = (
                f"*No results for '{box.value.strip()}'*" if box.value.strip() else ""
            )
            return
        lines = [f"**{total:,} matches** (top {len(top)} by PageRank):\n"]
        for _, r in top.iterrows():
            lines.append(
                f"- **{r['vertex']}** — PR {r['pagerank']:.2e}, "
                f"deg {int(r['degree']):,} · *{r['label']}*"
            )
        results.object = "\n".join(lines)

    button.on_click(do_search)
    # `value` fires on Enter or blur, not per keystroke, so this does not
    # re-scan the corpus while the user is still typing.
    box.param.watch(do_search, "value")

    sidebar = pn.Column(
        pn.pane.Markdown("## Wikipedia Graph Explorer"),
        pn.Row(box, button),
        results,
        pn.pane.Markdown("---"),
        pn.pane.Markdown(
            f"**{len(nodes):,}** articles\n\n"
            f"**{nodes['community'].nunique():,}** communities\n\n"
            f"*Zoom in and hover for article details.*"
        ),
        width=430,
    )

    return pn.Row(
        sidebar,
        pn.pane.HoloViews(plot, sizing_mode="stretch_both"),
        sizing_mode="stretch_both",
    )


app = build_app()
app.servable(title="Wikipedia Graph")
