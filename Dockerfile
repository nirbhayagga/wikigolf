FROM python:3.11-slim

WORKDIR /app

# Install only visualization dependencies (no GPU/igraph needed for serving)
# Versions pinned for reproducibility
RUN pip install --no-cache-dir \
    pandas==2.2.* pyarrow==17.* numpy \
    panel==1.4.* holoviews==1.19.* bokeh==3.4.* \
    datashader==0.16.* colorcet==3.1.* \
    pyyaml==6.* Pillow==10.* tqdm==4.*

COPY python/04_app.py .
COPY python/05_export_png.py .
COPY config.yaml .

# Data must be mounted at runtime: -v ./data:/app/data
VOLUME /app/data

EXPOSE 5006

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:5006/')" || exit 1

CMD ["panel", "serve", "04_app.py", \
     "--address", "0.0.0.0", \
     "--port", "5006", \
     "--allow-websocket-origin", "*", \
     "--num-procs", "2"]
