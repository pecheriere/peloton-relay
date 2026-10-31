# syntax=docker/dockerfile:1

# The builder always runs on the build host's platform and cross-compiles the
# static musl binary for the requested target with zig as the C compiler and
# linker, so multi-platform builds need no emulation. The Rust version tracks
# rust-toolchain.toml (1.99.0).
FROM --platform=$BUILDPLATFORM rust:1.99.0-alpine3.23@sha256:42c2519fdf75d9e34cc61a1aaf34b1a684da9bfd65fe5daefef98d92a801b001 AS build

ARG TARGETARCH

# zig is pinned to the version in Alpine 3.23 community, so a repository update
# cannot change the C compiler and linker under a digest-pinned base image.
RUN apk add --no-cache musl-dev zig=0.15.2-r0 \
    && cargo install --locked --version 0.24.0 cargo-zigbuild \
    && rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Only the server binary is built; the macOS login crate is never compiled.
RUN case "$TARGETARCH" in \
        arm64) target=aarch64-unknown-linux-musl ;; \
        amd64) target=x86_64-unknown-linux-musl ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && CARGO_PROFILE_RELEASE_STRIP=true \
       cargo zigbuild --release --locked -p peloton-relay --target "$target" \
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
