"""
04_app.py — Interactive Wikipedia Graph Web Viewer

Datashader raster + search (including redirect aliases) + community legend,
plus per-article inspection: click any point (or pick a search hit) to
highlight that article's incoming and/or outgoing links as line segments,
and isolate a single region to see its shape without the other 7M points.

Edge highlighting needs edges.parquet next to nodes.parquet and costs ~3 GB
of RAM for the two direction indexes at enwiki scale; without the file the
viewer runs exactly as before, minus that feature.

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


class EdgeStore:
    """Both directions of the link graph, grouped for O(degree) lookup.

    One stable argsort per direction: `order[offsets[v]:offsets[v+1]]` are the
    edge rows whose src (resp. dst) is v. The permutations are int32 — the
    edge count fits — which keeps the whole store at ~2 int32 arrays per
    direction (~3.7 GB total at 231.7M edges) instead of the ~7.4 GB that
    int64 argsort output would occupy.
    """

    def __init__(self, path, n):
        tbl = pq.read_table(path, columns=["src", "dst"])
        self.src = tbl.column("src").to_numpy().astype(np.int32, copy=False)
        self.dst = tbl.column("dst").to_numpy().astype(np.int32, copy=False)
        self.out_off, self.out_ord = self._group(self.src, n)
        self.in_off, self.in_ord = self._group(self.dst, n)
        print(f"   {len(self.src):,} edges indexed both ways")

    @staticmethod
    def _group(keys, n):
        counts = np.bincount(keys, minlength=n)
        offsets = np.zeros(n + 1, dtype=np.int64)
        np.cumsum(counts, out=offsets[1:])
        order = np.argsort(keys, kind="stable").astype(np.int32)
        return offsets, order

    def neighbors(self, v, direction):
        """Endpoint ids on the far side of v's edges in one direction."""
        if direction == "out":
            rows = self.out_ord[self.out_off[v] : self.out_off[v + 1]]
            return self.dst[rows]
        rows = self.in_ord[self.in_off[v] : self.in_off[v + 1]]
        return self.src[rows]


class GridIndex:
    """Uniform-grid nearest-point lookup, pure numpy.

    scipy's KDTree would be the obvious tool, but the viewer image
    deliberately excludes scipy (see the Dockerfile) — and a 1024x1024
    bucket grid over 7.2M points answers a click in well under a
    millisecond anyway.
    """

    BINS = 1024

    def __init__(self, x, y):
        # The stragglers outside the giant component may carry NaN positions;
        # they can never be clicked, so they are simply not indexed.
        ok = np.isfinite(x) & np.isfinite(y)
        self.ids = np.flatnonzero(ok).astype(np.int32)
        self.x, self.y = x[self.ids], y[self.ids]
        self.x0, self.y0 = float(self.x.min()), float(self.y.min())
        self.sx = (float(self.x.max()) - self.x0) / self.BINS or 1.0
        self.sy = (float(self.y.max()) - self.y0) / self.BINS or 1.0
        cell = self._cell(self.x, self.y)
        self.off, self.order = EdgeStore._group(cell, self.BINS * self.BINS)

    def _cell(self, x, y):
        cx = np.clip(((x - self.x0) / self.sx).astype(np.int64), 0, self.BINS - 1)
        cy = np.clip(((y - self.y0) / self.sy).astype(np.int64), 0, self.BINS - 1)
        return (cy * self.BINS + cx).astype(np.int32)

    def nearest(self, x, y):
        cx = int(np.clip((x - self.x0) / self.sx, 0, self.BINS - 1))
        cy = int(np.clip((y - self.y0) / self.sy, 0, self.BINS - 1))
        for radius in (1, 4, 16):
            cand = []
            for gy in range(max(0, cy - radius), min(self.BINS, cy + radius + 1)):
                a = gy * self.BINS + max(0, cx - radius)
                b = gy * self.BINS + min(self.BINS - 1, cx + radius)
                cand.append(self.order[self.off[a] : self.off[b + 1]])
            idx = np.concatenate(cand) if cand else np.array([], dtype=np.int32)
            if len(idx):
                d = (self.x[idx] - x) ** 2 + (self.y[idx] - y) ** 2
                # idx addresses the compacted arrays; map back to article ids.
                return int(self.ids[idx[int(np.argmin(d))]])
        # A click in open ocean: brute-force the lot (~30 ms at 7.2M points).
        # Returning nothing would make the click feel broken; returning the
        # honest nearest point never does.
        if len(self.x) == 0:
            return None
        d = (self.x - x) ** 2 + (self.y - y) ** 2
        return int(self.ids[int(np.argmin(d))])


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
    categories = build_categories(nodes, labels, viz.get("max_categories", MAX_CATEGORIES))

    search = Search(nodes, paths)

    coords_x = nodes["x"].to_numpy().astype(np.float32, copy=False)
    coords_y = nodes["y"].to_numpy().astype(np.float32, copy=False)
    titles_of = nodes["vertex"].to_numpy()

    edges = None
    if os.path.exists(paths.edges):
        print("Indexing edges for click highlighting (~3 GB at enwiki scale)...")
        edges = EdgeStore(paths.edges, len(nodes))
    else:
        print("   (no edges.parquet — click highlighting disabled)")
    grid = GridIndex(coords_x, coords_y)

    # -- widgets that drive the plot -------------------------------------
    isolate = pn.widgets.Select(
        name="Isolate region", options=["All"] + categories, value="All", width=200
    )
    edge_dir = pn.widgets.RadioButtonGroup(
        name="Edges", options=["Out", "In", "Both"], value="Both", width=200
    )
    info = pn.pane.Markdown("*Click any point to inspect an article.*", width=420)

    # The isolate filter feeds datashade through a DynamicMap, so zooming
    # re-aggregates only the chosen region's points. The categorical dtype
    # survives the filter, which is what keeps count_cat happy.
    def points_for(region):
        df = nodes if region in (None, "All") else nodes[nodes["label"] == region]
        return hv.Points(df, kdims=["x", "y"], vdims=["label", "vertex", "pagerank"])

    region_stream = hv.streams.Params(isolate, ["value"], rename={"value": "region"})
    pts_dmap = hv.DynamicMap(points_for, streams=[region_stream])
    shaded = hd.datashade(
        pts_dmap,
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

    # -- click -> nearest article -> its edges ---------------------------
    # One persistent Tap stream on the shaded plot; search selects by firing
    # the same stream, so both paths share one code path and one overlay.
    tap = hv.streams.Tap(source=shaded, x=None, y=None)
    dir_stream = hv.streams.Params(edge_dir, ["value"], rename={"value": "mode"})

    MAX_DRAWN = 1500

    def segments_for(v, direction):
        nbr = edges.neighbors(v, direction)
        total = len(nbr)
        if total > MAX_DRAWN:
            nbr = nbr[:: max(1, total // MAX_DRAWN)][:MAX_DRAWN]
        return (
            hv.Segments(
                {
                    "x0": np.full(len(nbr), coords_x[v]),
                    "y0": np.full(len(nbr), coords_y[v]),
                    "x1": coords_x[nbr],
                    "y1": coords_y[nbr],
                },
                kdims=["x0", "y0", "x1", "y1"],
            ),
            total,
            nbr,
        )

    def selection(x, y, mode):
        seg_out = seg_in = hv.Segments([], kdims=["x0", "y0", "x1", "y1"])
        marker = hv.Points([])
        v = grid.nearest(x, y) if (x is not None and y is not None) else None
        if v is not None:
            out_n = in_n = 0
            sample = np.array([], dtype=np.int32)
            if edges is not None:
                if mode in ("Out", "Both"):
                    seg_out, out_n, sample = segments_for(v, "out")
                if mode in ("In", "Both"):
                    seg_in, in_n, s2 = segments_for(v, "in")
                    if not len(sample):
                        sample = s2
            marker = hv.Points({"x": [coords_x[v]], "y": [coords_y[v]]})
            row = nodes.iloc[v]
            names = ", ".join(str(titles_of[w]) for w in sample[:10])
            trunc = (
                " *(a sample is drawn — hubs would be solid ink)*"
                if max(out_n, in_n) > MAX_DRAWN
                else ""
            )
            info.object = (
                f"### {titles_of[v]}\n"
                f"*{row['label']}* · PR {row['pagerank']:.2e}\n\n"
                f"**{out_n:,}** outgoing · **{in_n:,}** incoming{trunc}\n\n"
                + (f"Links to: {names}…" if names else "")
                + ("" if edges is not None else "\n\n*(edges.parquet absent — counts only)*")
            )
        return (
            seg_out.opts(color="#ff9d3c", line_width=0.6, alpha=0.5)
            * seg_in.opts(color="#39c0ff", line_width=0.6, alpha=0.5)
            * marker.opts(size=11, color="white", fill_alpha=0, line_width=2)
        )

    sel_dmap = hv.DynamicMap(selection, streams=[tap, dir_stream])

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
        plot = shaded * hover * sel_dmap
    except Exception as e:
        import traceback

        print(f"   ⚠ Hover tooltips disabled ({type(e).__name__}: {e})")
        print("     holoviews API drift — check inspect_points signature:")
        traceback.print_exc()
        plot = shaded * sel_dmap

    box = pn.widgets.TextInput(
        name="Search article",
        placeholder="title or redirect, e.g. USA",
        width=340,
    )
    button = pn.widgets.Button(name="Find", button_type="primary", width=80)
    hits = pn.widgets.Select(name="Matches (by PageRank)", options={}, width=340)
    mark_btn = pn.widgets.Button(name="Highlight", width=80)
    results = pn.pane.Markdown("", width=420)

    def do_search(*_):
        top, total = search.query(box.value)
        if top is None:
            results.object = (
                f"*No results for '{box.value.strip()}'*" if box.value.strip() else ""
            )
            hits.options = {}
            return
        results.object = f"**{total:,} matches**"
        # Row position is the article id — the frame was never reordered.
        hits.options = {
            f"{r['vertex']}  ·  deg {int(r['degree']):,}": int(i)
            for i, r in top.iterrows()
        }

    def do_mark(*_):
        v = hits.value
        if v is None:
            return
        # Selecting a hit is a synthetic click at the article's position:
        # same stream, same overlay, same info pane as a real tap.
        tap.event(x=float(coords_x[v]), y=float(coords_y[v]))

    button.on_click(do_search)
    mark_btn.on_click(do_mark)
    hits.param.watch(do_mark, "value")
    # `value` fires on Enter or blur, not per keystroke, so this does not
    # re-scan the corpus while the user is still typing.
    box.param.watch(do_search, "value")

    sidebar = pn.Column(
        pn.pane.Markdown("## Wikipedia Graph Explorer"),
        pn.Row(box, button),
        results,
        pn.Row(hits, mark_btn),
        pn.pane.Markdown("---"),
        isolate,
        pn.pane.Markdown("**Edge direction** (orange out · blue in)"),
        edge_dir,
        info,
        pn.pane.Markdown("---"),
        pn.pane.Markdown(
            f"**{len(nodes):,}** articles\n\n"
            f"**{nodes['community'].nunique():,}** communities\n\n"
            f"*Zoom and hover for details; click a point or highlight a "
            f"search hit to see its links.*"
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
