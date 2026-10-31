# Build from the repository root using this file or the Compose examples.
FROM rust:1.91.1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /source
COPY Cargo.toml Cargo.lock README.md ./
COPY crates ./crates
ARG FEATURES=stores,tls
RUN cargo build --release --locked -p kimetsu-remote --bin kimetsu-remote --features "$FEATURES"

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 libstdc++6 git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 kimetsu
COPY --from=build /source/target/release/kimetsu-remote /usr/local/bin/kimetsu-remote
USER 10001:10001
ENV HOME=/home/kimetsu
EXPOSE 8787
ENTRYPOINT ["kimetsu-remote"]
CMD ["serve", "--addr", "0.0.0.0:8787", "--allow-public-http", "--reranker", "off"]
