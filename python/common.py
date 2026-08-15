"""Shared configuration, paths, and cache bookkeeping for the pipeline.

The parser (`wiki-parser`) now produces the graph directly, so the Python side
starts from integer Parquet and never touches article strings until the very
last step.

Inputs produced by the parser:
    titles.parquet     id (dense 0..N-1), title
    redirects.parquet  alias, article_id
    edges.parquet      src, dst  (int32, deduped, no self-loops)
"""

import contextlib
import json
import os
import subprocess
import threading
import time

import pyarrow.parquet as pq
import yaml

DEFAULTS = {
    "pipeline": {"sample_ratio": 1.0, "data_dir": "data"},
    "gpu": {"store_transposed": True},
    "layout": {
        "backend": "cpu",
        "algorithm": "fa2",
        "max_iter": 500,
        "lin_log": False,
        "cpu_method": "sfdp",
        "backbone_frac": 0.10,
    },
    "community": {"objective": "modularity", "resolution": 1.0, "top_n": 20},
    "gemini": {"model": "gemini-2.5-flash", "rate_limit_sleep": 5},
    "visualization": {
        "width": 1200,
        "height": 800,
        "bgcolor": "black",
        "title": "The Map of Wikipedia",
        "max_categories": 24,
    },
    "export": {"width": 4096, "height": 2160, "max_categories": 24},
}


def load_config(path="config.yaml"):
    """Load config.yaml over the defaults. Unknown sections are preserved."""
    cfg = {k: dict(v) for k, v in DEFAULTS.items()}
    try:
        with open(path) as f:
            user = yaml.safe_load(f) or {}
    except FileNotFoundError:
        return cfg
    for section, values in user.items():
        if isinstance(values, dict):
            cfg.setdefault(section, {}).update(values)
        else:
            cfg[section] = values
    return cfg


class Paths:
    """Every file the pipeline reads or writes, in one place.

    Previously these lists were duplicated between 01 and 07 and drifted.
    """

    def __init__(self, data_dir="data"):
        self.data_dir = data_dir
        j = lambda *p: os.path.join(data_dir, *p)

        # Parser outputs (inputs to this pipeline)
        self.titles = j("titles.parquet")
        self.redirects = j("redirects.parquet")
        self.edges = j("edges.parquet")

        # Pipeline caches
        self.manifest = j("manifest.json")
        self.cache_metrics = j("cache_metrics.parquet")
        self.cache_layout = j("cache_layout.parquet")
        self.sfdp_raw = j("cache_sfdp_raw.npz")

        # Final outputs
        self.nodes = j("nodes.parquet")
        self.community_labels = j("community_labels.json")
        self.community_stats = j("community_stats.json")

    @property
    def caches(self):
        return [self.manifest, self.cache_metrics, self.cache_layout, self.nodes,
                self.sfdp_raw]

    @property
    def layout_caches(self):
        """Everything phase 2 onward owns — deliberately NOT cache_metrics.

        PageRank is expensive and often computed on a different machine than
        the layout, so re-tuning a layout setting must not throw it away.

        Also deliberately NOT sfdp_raw. The raw positions are by far the most
        expensive artifact in the pipeline (hours at enwiki scale) and depend
        only on link structure, so re-running Leiden at a new resolution must
        not destroy them. `_layout_sfdp` fingerprints that file itself and
        rebuilds it when the graph or backbone_frac really did change; use
        --reset-sfdp to force it.
        """
        return [self.manifest, self.cache_layout, self.nodes]

    @property
    def sfdp_caches(self):
        return self.layout_caches + [self.sfdp_raw]

    @property
    def parser_outputs(self):
        return [self.titles, self.redirects, self.edges]

    def require_parser_outputs(self):
        missing = [p for p in (self.titles, self.edges) if not os.path.exists(p)]
        if missing:
            raise SystemExit(
                "Missing parser output: "
                + ", ".join(missing)
                + "\n  Run the Rust parser first:\n"
                "    cargo build --release\n"
                "    ./target/release/wiki-parser <dump>.xml.bz2 --out "
                + self.data_dir
            )

    def n_articles(self):
        """Authoritative vertex count.

        Taken from titles.parquet rather than max(edge id)+1, because articles
        with no links in either direction exist and must still be nodes.
        """
        return pq.read_metadata(self.titles).num_rows


# --------------------------------------------------------------------------
# Cache validation
# --------------------------------------------------------------------------

# Config keys that change the numbers a phase produces. Editing any of these
# must invalidate the caches, exactly as changing the input data does — a
# layout cached at resolution 4.0 is not a layout at resolution 1.0.
#
# layout.backend is deliberately absent: "auto" that falls back to CPU and an
# explicit "cpu" produce identical output, so including it would invalidate
# caches for no reason.
CACHE_KEYS = [
    ("community", "resolution"),
    ("community", "objective"),
    ("layout", "algorithm"),
    ("layout", "max_iter"),
    ("layout", "lin_log"),
    ("layout", "cpu_method"),
    ("layout", "backbone_frac"),
]


def fingerprint(paths, sample_ratio, cfg=None):
    """Identify the exact inputs and settings a cache was built from."""
    fp = {
        "edges_bytes": os.path.getsize(paths.edges),
        "edges_rows": pq.read_metadata(paths.edges).num_rows,
        "titles_bytes": os.path.getsize(paths.titles),
        "n_articles": paths.n_articles(),
        "sample_ratio": float(sample_ratio),
    }
    if cfg is not None:
        for section, key in CACHE_KEYS:
            fp[f"{section}.{key}"] = cfg.get(section, {}).get(key)
    return fp


def check_manifest(paths, sample_ratio, cfg=None):
    """Refuse to mix caches built from different inputs or settings.

    Without this, running one phase on a sample and another on the full graph
    produces a merged result that looks fine and is meaningless. The same
    applies to editing config: a layout cached at one resolution silently
    surviving a resolution change is the same class of error.
    """
    current = fingerprint(paths, sample_ratio, cfg)
    if not os.path.exists(paths.manifest):
        with open(paths.manifest, "w") as f:
            json.dump(current, f, indent=2)
        return current

    with open(paths.manifest) as f:
        stored = json.load(f)

    if stored != current:
        diff = [k for k in current if stored.get(k) != current.get(k)]
        raise SystemExit(
            "Cached results were built from different inputs.\n"
            f"  Changed: {', '.join(diff)}\n"
            f"  cached:  { {k: stored.get(k) for k in diff} }\n"
            f"  current: { {k: current[k] for k in diff} }\n"
            "  Run with --reset to recompute from scratch."
        )
    return current


def reset_caches(paths, layout_only=False, sfdp=False):
    if layout_only:
        targets = paths.sfdp_caches if sfdp else paths.layout_caches
    else:
        targets = paths.caches
    for p in targets:
        if os.path.exists(p):
            os.remove(p)
            print(f"   Deleted {p}")
    print("   Caches cleared.")


# --------------------------------------------------------------------------
# Reporting helpers
# --------------------------------------------------------------------------

def log_phase(name):
    print(f"\n{'=' * 60}")
    print(f"[{time.strftime('%H:%M:%S')}] {name}")
    print(f"{'=' * 60}")


def elapsed(start):
    secs = time.time() - start
    return f"{secs:.1f}s" if secs < 60 else f"{secs / 60:.1f}m"


def vram_status():
    try:
        out = subprocess.check_output(
            [
                "nvidia-smi",
                "--query-gpu=memory.used,memory.total",
                "--format=csv,nounits,noheader",
            ],
            stderr=subprocess.DEVNULL,
        )
        used, total = out.decode().strip().split(", ")
        print(f"   VRAM: {used}/{total} MB")
    except (FileNotFoundError, subprocess.CalledProcessError, ValueError):
        pass


def _ticker(stop, desc):
    start = time.time()
    while not stop.is_set():
        s = int(time.time() - start)
        print(f"\r   {desc}: {s // 3600:02d}:{(s % 3600) // 60:02d}:{s % 60:02d}",
              end="", flush=True)
        stop.wait(1)
    print()


@contextlib.contextmanager
def run_with_timer(desc):
    """Tick a clock during long operations.

    Uses a thread rather than a subprocess: forking after CUDA has initialized
    corrupts the context.
    """
    stop = threading.Event()
    t = threading.Thread(target=_ticker, args=(stop, desc), daemon=True)
    t.start()
    try:
        yield
    finally:
        stop.set()
        t.join()


def free_gpu_memory():
    """Release RMM/CuPy pools between phases.

    RAPIDS keeps its pool allocated after `del` + `gc.collect()`, so the next
    phase starts with VRAM already consumed and OOMs.
    """
    try:
        import cupy as cp

        cp.get_default_memory_pool().free_all_blocks()
        cp.get_default_pinned_memory_pool().free_all_blocks()
    except ImportError:
        pass
    try:
        import rmm

        rmm.reinitialize()
    except ImportError:
        pass
