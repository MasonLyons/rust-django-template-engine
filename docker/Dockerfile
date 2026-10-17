# syntax=docker/dockerfile:1
# Development image: Rust toolchain + maturin + test dependencies. The source
# is bind-mounted at /src and built by docker/dev.sh on every run, so the image
# only needs rebuilding when the toolchain or Python dependencies change.
FROM python:3.13-slim AS dev
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential curl git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
        --default-toolchain 1.94.0 --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
RUN python3.13 -m venv /opt/dtlvenv \
    && /opt/dtlvenv/bin/pip install --no-cache-dir maturin==1.15.0 pytest hypothesis
ENV CARGO_TARGET_DIR=/ctarget
COPY docker/dev.sh /usr/local/bin/dev.sh
ENTRYPOINT ["/usr/local/bin/dev.sh"]
