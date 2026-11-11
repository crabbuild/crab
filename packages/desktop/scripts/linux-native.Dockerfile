FROM node:22.20.0-bookworm-slim AS node
FROM rust:1.97.0-slim-bookworm AS rust
FROM almalinux:8

RUN dnf install -y gcc-toolset-14-gcc gcc-toolset-14-gcc-c++ make cmake pkgconf-pkg-config git curl tar xz ca-certificates && dnf clean all
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=rust /usr/local/cargo /usr/local/cargo
COPY --from=rust /usr/local/rustup /usr/local/rustup
ENV PATH="/usr/local/cargo/bin:/usr/local/bin:/opt/rh/gcc-toolset-14/root/usr/bin:${PATH}" RUSTUP_HOME="/usr/local/rustup"
