# Self-contained dbgen image, for hosts without a Rust toolchain:
#
#   docker build -t dbgen .
#   docker run --rm --network host -v "$PWD:/work" -w /work dbgen generate -c dbgen.yaml
#
# The configuration's `schema` paths must be inside the mounted directory.
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin dbgen

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/dbgen /usr/local/bin/dbgen
ENTRYPOINT ["dbgen"]
