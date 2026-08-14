FROM python:3.11-slim

WORKDIR /app

# Visualization dependencies only — the image serves precomputed results, so
# it needs no GPU, no RAPIDS and no igraph.
#
# Versions are pinned to the majors the viewer is tested against. holoviews
# matters in particular: inspect_points changed signature after 1.19, and the
# viewer uses the newer form (single element + max_indicators).
RUN pip install --no-cache-dir \
    pandas==2.* pyarrow==21.* numpy==2.* \
    panel==1.9.* holoviews==1.23.* bokeh==3.9.* \
    datashader==0.19.* colorcet==3.* \
    pyyaml==6.* Pillow==11.* tqdm==4.*

COPY python/common.py .
COPY python/04_app.py .
COPY python/05_export_png.py .
COPY config.yaml .

# Data must be mounted at runtime: -v ./data:/app/data
VOLUME /app/data

EXPOSE 5006

# Restrict this to your real hostname in production; "*" accepts a websocket
# handshake from any origin.
ENV WEBSOCKET_ORIGIN="*"

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:5006/')" || exit 1

CMD panel serve 04_app.py \
    --address 0.0.0.0 \
    --port 5006 \
    --allow-websocket-origin "$WEBSOCKET_ORIGIN" \
    --num-procs 2
