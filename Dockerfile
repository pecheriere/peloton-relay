# The builder always runs on the build host's platform and cross-compiles the
# static musl binary for the requested target with zig as the C compiler and
# linker, so multi-platform builds need no emulation. The Rust version tracks
# rust-toolchain.toml (1.99). No `# syntax=` line: the Dockerfile frontend
# built into BuildKit covers everything used here, so no unpinned frontend
# image is pulled at build time.
FROM --platform=$BUILDPLATFORM rust:1.99.1-alpine3.23@sha256:9bdc8babc876f1414b51b20ed3acf9432e543afafa9294c560cb59dafc377175 AS build

ARG TARGETARCH

# zig is pinned to the version in Alpine 3.23 community, so a repository update
# cannot change the C compiler and linker under a digest-pinned base image.
RUN apk add --no-cache musl-dev zig=0.15.2-r0 \
    && cargo install --locked --version 0.24.0 cargo-zigbuild \
    && rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl

WORKDIR /src

# Dependencies build in their own layer, from the manifests and Cargo.lock
# only, so the build cache reuses it until Cargo.lock changes. Each crate gets
# empty sources for this step. Only the server binary is built; the macOS login
# crate is never compiled, but the workspace needs its manifest and sources.
COPY Cargo.toml Cargo.lock ./
COPY crates/peloton/Cargo.toml crates/peloton/
COPY crates/peloton-relay/Cargo.toml crates/peloton-relay/
COPY crates/peloton-relay-login/Cargo.toml crates/peloton-relay-login/
COPY crates/peloton-relay-test-support/Cargo.toml crates/peloton-relay-test-support/
RUN case "$TARGETARCH" in \
        arm64) echo aarch64-unknown-linux-musl ;; \
        amd64) echo x86_64-unknown-linux-musl ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac > /target-triple \
    && for crate in peloton-relay peloton-relay-login; do \
        mkdir -p "crates/$crate/src" && echo 'fn main() {}' > "crates/$crate/src/main.rs"; \
    done \
    && for crate in peloton peloton-relay peloton-relay-login peloton-relay-test-support; do \
        mkdir -p "crates/$crate/src" && : > "crates/$crate/src/lib.rs"; \
    done \
    && cargo zigbuild --release --locked -p peloton-relay --target "$(cat /target-triple)"

COPY crates ./crates

# The placeholder build is removed first, so the real sources always compile,
# whatever their timestamps.
RUN target="$(cat /target-triple)" \
    && cargo clean --release --target "$target" -p peloton-relay -p peloton \
    && cargo zigbuild --release --locked -p peloton-relay --target "$target" \
    && install -D -m 0755 "target/$target/release/peloton-relay" /out/rootfs/peloton-relay \
    && install -d -m 0700 -o 65532 -g 65532 /out/rootfs/state

# The clients trust the Mozilla root certificates compiled into the binary, so no CA
# bundle is copied in.
FROM scratch

# Copied as one tree, which keeps each entry's owner and mode. Copying the
# state directory on its own would recreate it with mode 0755.
COPY --from=build /out/rootfs/ /

USER 65532:65532
ENV PELOTON_RELAY_STATE_DIR=/state
VOLUME /state
EXPOSE 8080
ENTRYPOINT ["/peloton-relay"]
CMD ["serve"]
