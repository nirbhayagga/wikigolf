#!/usr/bin/env bash
# Create the RAPIDS environment for the GPU half of the pipeline.
#
#   bash scripts/setup-gpu-env.sh
#
# Everything conda-installable goes in ONE solve. Do not `pip install -r
# python/requirements.txt` into this env afterwards: pip will pull
# pandas/numpy/pyarrow from PyPI and can upgrade them out from under cudf,
# which is compiled against specific versions. The env then imports fine and
# fails at runtime, which is a miserable thing to debug mid-run.
set -euo pipefail

ENV_NAME="${ENV_NAME:-rapids-env}"
RAPIDS_VERSION="${RAPIDS_VERSION:-24.04}"
PYTHON_VERSION="${PYTHON_VERSION:-3.11}"
CUDA_VERSION="${CUDA_VERSION:-12.2}"

if ! command -v mamba >/dev/null 2>&1; then
  echo "mamba not found. Install miniforge first: https://github.com/conda-forge/miniforge" >&2
  exit 1
fi

echo "Creating '$ENV_NAME' (rapids=$RAPIDS_VERSION, python=$PYTHON_VERSION, cuda=$CUDA_VERSION)"
echo "This downloads several GB and can take 5-15 minutes."

# rapids is pinned deliberately: _layout_gpu depends on cuGraph internals
# (from_cudf_adjlist, graph_properties.directed, a monkey-patched G.nodes()).
# Newer majors have reorganised these, so upgrade only with a working
# end-to-end run to compare against.
mamba create -n "$ENV_NAME" -c rapidsai -c conda-forge -c nvidia "rapids=$RAPIDS_VERSION" "python=$PYTHON_VERSION" "cuda-version=$CUDA_VERSION" python-igraph pyyaml tqdm datashader holoviews bokeh colorcet panel pillow -y

echo
echo "Done. Next:"
echo "    mamba activate $ENV_NAME"
echo "    pip install google-genai      # the only dep not on conda-forge"
echo "    export KVIKIO_COMPAT_MODE=ON  # Fedora: disable GPU Direct Storage"
