# Build stage
FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json

RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build --release

FROM debian:stable-slim AS runtime

RUN apt-get update && apt-get install --no-install-recommends -y tini ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/cam_rs /usr/local/bin

EXPOSE 3000
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["cam_rs"]
