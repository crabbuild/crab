# Cloud Run Dockerfile for the crab-auth endpoint.
#
# Deploy:
#   docker build -f crab/deploy/auth-service/cloudrun/Dockerfile -t gcr.io/PROJECT/crab-auth .
#   docker push gcr.io/PROJECT/crab-auth
#   gcloud run deploy crab-auth --image gcr.io/PROJECT/crab-auth

FROM rust:1-slim-bookworm AS receive-helper

WORKDIR /workspace

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git build-essential cmake pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY crab/ crab/
COPY beyond-desktop/agent/ beyond-desktop/agent/
RUN cargo build --release --locked -p crab-auth-server \
    --bin crab-auth-receive \
    --bin crab-auth-view \
    --no-default-features

FROM python:3.12-slim AS runtime

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install dependencies.
COPY crab/deploy/auth-service/requirements-docker.txt .
RUN pip install --no-cache-dir -r requirements-docker.txt

# Copy application code.
COPY crab/deploy/auth-service/src/ src/
COPY crab/deploy/auth-service/config/ /etc/crab-auth/
COPY --from=receive-helper /workspace/target/release/crab-auth-receive /usr/local/bin/crab-auth-receive
COPY --from=receive-helper /workspace/target/release/crab-auth-view /usr/local/bin/crab-auth-view

# Non-root user.
RUN useradd --create-home --shell /bin/bash crab
USER crab

# Cloud Run sets PORT env var.
CMD exec uvicorn src.app:app --host 0.0.0.0 --port ${PORT:-8080}
