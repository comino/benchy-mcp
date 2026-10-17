# The release workflow uses --target release with an unpacked, tested bundle as its context.
# A normal `docker build .` builds the same layout from source (the final stage).
FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587 AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
LABEL org.opencontainers.image.source="https://github.com/comino/benchy-mcp" \
      org.opencontainers.image.title="benchy-mcp" \
      org.opencontainers.image.description="Experimental MCP analytics workbench. Prototype; frequent breaking changes expected." \
      org.opencontainers.image.licenses="MIT"
WORKDIR /data
USER 65532:65532
ENTRYPOINT ["/opt/benchy/benchy-mcp"]
CMD ["/data"]

FROM runtime AS release
COPY . /opt/benchy/

# The same Rust as rust-toolchain.toml, which the native bundles are built with;
# scripts/check_toolchain.py keeps the two in step.
FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
RUN target="$(rustc -vV | sed -n 's/^host: //p')" \
    && case "$target" in \
         x86_64-unknown-linux-gnu) platform=linux-x86_64 ;; \
         aarch64-unknown-linux-gnu) platform=linux-aarch64 ;; \
         *) echo "unsupported container target: $target" >&2; exit 1 ;; \
       esac \
    && BENCHY_BUNDLE=1 cargo build --release --locked --target "$target" \
    && packaging/package.sh "$target" "$platform" /dist \
    && mkdir /bundle \
    && tar -xzf "/dist/benchy-mcp-$platform.tar.gz" --strip-components=1 -C /bundle

FROM runtime AS source
COPY --from=build /bundle/ /opt/benchy/
