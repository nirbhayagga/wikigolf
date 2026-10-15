"""Numpy indexes shared by the explorer and its tests."""
import numpy as np

class EdgeStore:
    """Both directions of the link graph, grouped for O(degree) lookup.

    One stable argsort per direction: `order[offsets[v]:offsets[v+1]]` are the
    edge rows whose src (resp. dst) is v. The permutations are int32 — the
    edge count fits — which keeps the whole store at ~2 int32 arrays per
    direction (~3.7 GB total at 231.7M edges) instead of the ~7.4 GB that
    int64 argsort output would occupy.
    """

    def __init__(self, path, n):
        import pyarrow.parquet as pq

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
        self.x0 = float(self.x.min()) if len(self.x) else 0.0
        self.y0 = float(self.y.min()) if len(self.y) else 0.0
        self.sx = ((float(self.x.max()) if len(self.x) else 0.0) - self.x0) / self.BINS or 1.0
        self.sy = ((float(self.y.max()) if len(self.y) else 0.0) - self.y0) / self.BINS or 1.0
        cell = self._cell(self.x, self.y)
        self.off, self.order = EdgeStore._group(cell, self.BINS * self.BINS)

    def _cell(self, x, y):
        cx = np.clip(((x - self.x0) / self.sx).astype(np.int64), 0, self.BINS - 1)
        cy = np.clip(((y - self.y0) / self.sy).astype(np.int64), 0, self.BINS - 1)
        return (cy * self.BINS + cx).astype(np.int32)

    def nearest(self, x, y, allowed=None):
        if not np.isfinite(x) or not np.isfinite(y) or not len(self.ids):
            return None
        cx = int(np.clip((x - self.x0) / self.sx, 0, self.BINS - 1))
        cy = int(np.clip((y - self.y0) / self.sy, 0, self.BINS - 1))
        for radius in (1, 4, 16, 64):
            left, right = max(0, cx-radius), min(self.BINS-1, cx+radius)
            bottom, top = max(0, cy-radius), min(self.BINS-1, cy+radius)
            cand = [self.order[self.off[gy*self.BINS+left]:self.off[gy*self.BINS+right+1]]
                    for gy in range(bottom, top+1)]
            idx = np.concatenate(cand)
            if allowed is not None:
                idx = idx[allowed[self.ids[idx]]]
            if not len(idx):
                continue
            d = (self.x[idx]-x)**2 + (self.y[idx]-y)**2
            best = int(np.argmin(d))
            # Stop only when no point in an unsearched cell can be closer.
            bounds = []
            if left > 0:
                bounds.append(max(0, x-(self.x0+left*self.sx)))
            if right < self.BINS-1:
                bounds.append(max(0, self.x0+(right+1)*self.sx-x))
            if bottom > 0:
                bounds.append(max(0, y-(self.y0+bottom*self.sy)))
            if top < self.BINS-1:
                bounds.append(max(0, self.y0+(top+1)*self.sy-y))
            if not bounds or d[best] <= min(bounds)**2:
                return int(self.ids[idx[best]])
        idx = np.arange(len(self.ids))
        if allowed is not None:
            idx = idx[allowed[self.ids]]
        if not len(idx):
            return None
        d = (self.x[idx]-x)**2 + (self.y[idx]-y)**2
        return int(self.ids[idx[int(np.argmin(d))]])

