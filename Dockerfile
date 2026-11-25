# Self-contained dbgen image, for hosts without a Rust toolchain:
#
#   docker build -t dbgen .
#   docker run --rm --network host --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work dbgen generate -c dbgen.yaml
#
# The configuration's `schema` paths must be inside the mounted directory. dbgen writes
# its resume state (and SQLite files) there, so `--user` keeps them owned by you; without
# it the image runs as its unprivileged `nonroot` user, which may not be able to write the
# mount.
FROM rust:1.100.0-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cargo.toml names the integration tests, so cargo needs them to read the manifest;
# building the binary does not compile them.
COPY tests ./tests
# The image drops debug info (the release profile keeps line tables) but keeps symbol
# names, so backtraces still name functions.
RUN CARGO_PROFILE_RELEASE_STRIP=debuginfo cargo build --release --locked --bin dbgen

# The binary needs only the C library, libm and libgcc_s (SQLite is compiled in): a
# distroless image holds just those and the CA certificates, without a shell or a
# package manager, so there is little to patch. It runs as an unprivileged user.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /src/target/release/dbgen /usr/local/bin/dbgen
USER nonroot
ENTRYPOINT ["/usr/local/bin/dbgen"]
