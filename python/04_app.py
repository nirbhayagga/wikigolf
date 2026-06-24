"""
04_app.py — Interactive Wikipedia Graph Web Viewer
Features: Datashader raster, search-by-article, hover tooltips, community legend
Run with: panel serve python/04_app.py --show
"""

import os
import pandas as pd
import json
import warnings
import numpy as np
import yaml

warnings.filterwarnings("ignore", category=RuntimeWarning)

import holoviews as hv
import holoviews.operation.datashader as hd
import datashader as ds
import colorcet as cc
import panel as pn

hv.extension('bokeh')
pn.extension()


def load_config():
    defaults = {"visualization": {"width": 1200, "height": 800,
                "bgcolor": "black", "title": "The Map of Wikipedia"}}
    try:
        with open("config.yaml") as f:
            user = yaml.safe_load(f)
        if "visualization" in user:
            defaults["visualization"].update(user["visualization"])
    except FileNotFoundError:
        pass
    return defaults["visualization"]


def build_app():
    viz = load_config()
    print("Loading nodes...")

    if not os.path.exists("data/nodes.parquet"):
        print("ERROR: data/nodes.parquet not found.")
        print("  Run the pipeline first: python python/01_graph_compute.py")
        raise SystemExit(1)

    nodes = pd.read_parquet("data/nodes.parquet")
    print(f"   {len(nodes):,} nodes loaded")

    # Load community labels
    try:
        with open("data/community_labels.json", "r") as f:
            labels = json.load(f)
    except FileNotFoundError:
        labels = {}

    nodes['label'] = nodes['community'].astype(str).map(labels).fillna("Other")
    nodes['community'] = nodes['community'].astype(str).astype('category')

    # Build datashaded point cloud
    points = hv.Points(
        nodes, kdims=['x', 'y'],
        vdims=['community', 'vertex', 'pagerank', 'label']
    )

    shaded = hd.datashade(
        points,
        aggregator=ds.count_cat('community'),
        color_key=cc.glasbey_dark,
        min_alpha=100
    ).opts(
        width=viz['width'], height=viz['height'],
        bgcolor=viz['bgcolor'], xaxis=None, yaxis=None,
        title=viz['title']
    )

    # Hover: show nearest points when zoomed in
    try:
        hover_points = hd.inspect_points(
            shaded, points,
            columns=['vertex', 'pagerank', 'label'],
            n=5
        ).opts(
            tools=['hover'], size=8, color='white',
            fill_alpha=0, line_color='white', line_width=2
        )
        plot = shaded * hover_points
    except Exception as e:
        print(f"   ⚠ Hover tooltips disabled: {e}")
        plot = shaded

    # Search widget
    search_input = pn.widgets.TextInput(
        name='Search Article', placeholder='e.g. Python (programming language)',
        width=350
    )
    search_btn = pn.widgets.Button(name='Find', button_type='primary', width=80)
    search_results = pn.pane.Markdown("", width=400)

    def do_search(event=None):
        query = search_input.value.strip()
        if not query:
            search_results.object = ""
            return
        matches = nodes[nodes['vertex'].str.contains(query, case=False, na=False, regex=False)]
        if len(matches) == 0:
            search_results.object = f"*No results for '{query}'*"
            return
        top = matches.nlargest(10, 'pagerank')
        lines = [f"**Found {len(matches):,} matches** (top 10 by PageRank):\n"]
        for _, r in top.iterrows():
            lines.append(
                f"- **{r['vertex']}** — PR: {r['pagerank']:.2e}, "
                f"Community: {r.get('label', r['community'])}"
            )
        search_results.object = "\n".join(lines)

    search_btn.on_click(do_search)
    search_input.param.watch(lambda e: do_search(), 'value')

    sidebar = pn.Column(
        pn.pane.Markdown("## Wikipedia Graph Explorer"),
        pn.Row(search_input, search_btn),
        search_results,
        pn.pane.Markdown("---"),
        pn.pane.Markdown(
            f"**{len(nodes):,}** articles\n\n"
            f"**{nodes['community'].nunique()}** communities\n\n"
            f"*Hover over points when zoomed in for details.*"
        ),
        width=420,
    )

    app = pn.Row(
        sidebar,
        pn.pane.HoloViews(plot, sizing_mode='stretch_both'),
        sizing_mode='stretch_both'
    )
    return app


app = build_app()
app.servable(title="Wikipedia Graph")