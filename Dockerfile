# syntax=docker/dockerfile:1.7
# Build: DOCKER_BUILDKIT=1 docker build -t mudah/api-rs:latest .
ARG RUST_VERSION=1.99.1

FROM rust:${RUST_VERSION}-slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Lapisan dependensi di-cache terpisah dari kode aplikasi.
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin mudah-rs

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
WORKDIR /app
COPY --from=builder /app/target/release/mudah-rs /app/mudah-rs
COPY templates /app/templates
ENV RS_PORT=3100 \
    MAIL_TEMPLATE_DIR=/app/templates/mail \
    RS_LOG_DIR=/app/logs \
    RUST_LOG=info,sqlx=warn
EXPOSE 3100
USER nonroot
ENTRYPOINT ["/app/mudah-rs"]
