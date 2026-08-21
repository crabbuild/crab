# Standalone Docker image for the crab-auth endpoint.
#
# Build:
#   docker build -f crab/deploy/auth-service/Dockerfile -t crab-auth .
#
# Run:
#   docker run -p 8080:8080 \
#     -v $(pwd)/crab/deploy/auth-service/config:/etc/crab-auth:ro \
#     -e CRAB_AUTH_JWKS_URL=https://your-idp.example.com/.well-known/jwks.json \
#     -e CRAB_AUTH_ISSUER=https://your-idp.example.com \
#     -e CRAB_AUTH_AUDIENCE=crab-cli \
#     -e CRAB_AUTH_AWS_ROLE_ARN=arn:aws:iam::123456789012:role/crab-auth \
#     -e CRAB_AUTH_AWS_EXTERNAL_ID=crab-auth \
#     crab-auth

FROM rust:1-slim-bookworm AS receive-helper

WORKDIR /workspace

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git build-essential cmake pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY crab/ crab/
RUN cargo build --release --locked -p crab-auth-server \
    --bin crab-auth-receive \
    --bin crab-auth-view \
    --no-default-features \
    && cargo build --release --locked -p crab --bin crab --no-default-features

FROM python:3.12-slim AS base

# Prevent Python from writing .pyc files and enable unbuffered output.
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install dependencies first for layer caching.
COPY crab/deploy/auth-service/requirements-docker.txt .
RUN pip install --no-cache-dir -r requirements-docker.txt

# Copy application code.
COPY crab/deploy/auth-service/src/ src/
COPY --from=receive-helper /workspace/target/release/crab-auth-receive /usr/local/bin/crab-auth-receive
COPY --from=receive-helper /workspace/target/release/crab-auth-view /usr/local/bin/crab-auth-view
COPY --from=receive-helper /workspace/target/release/crab /usr/local/bin/crab
RUN ln -s /usr/local/bin/crab /usr/local/bin/git-remote-crab

# Default config mount point.
RUN mkdir -p /etc/crab-auth

# Non-root user for security.
RUN useradd --create-home --shell /bin/bash crab
USER crab

EXPOSE 8080

# Readiness check for standalone container runtimes.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD python -c "import httpx; r = httpx.get('http://localhost:8080/ready'); r.raise_for_status()"

ENTRYPOINT ["python", "-m", "uvicorn", "src.app:app", "--host", "0.0.0.0", "--port", "8080"]
