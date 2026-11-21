# siple-rename: keep (the entrypoint below names the deprecated ZYNOPHONE_CONFIG on purpose)
# Siple Linux images. Build from the repo root:
#   docker build -t siple:dev --target runtime -f packaging/linux/Dockerfile .
#   docker build -t siple-build --target build-env -f packaging/linux/Dockerfile .
# The build context must never contain tests/interop/pbx.env (see .dockerignore).
ARG RUST_IMAGE=rust:1.100-bookworm
FROM ${RUST_IMAGE} AS build-env
RUN apt-get update && apt-get install -y --no-install-recommends cmake pkg-config libasound2-dev \
 && rm -rf /var/lib/apt/lists/* && rustup component add clippy rustfmt
WORKDIR /src

FROM build-env AS build
COPY . .
# Context mtimes (rsync from another OS) can predate the cached artifacts: touch workspace
# sources so cargo always rebuilds the workspace crates.
RUN --mount=type=cache,id=siple-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=siple-target,target=/src/target \
    find . -path ./target -prune -o \( -name '*.rs' -o -name Cargo.toml -o -name build.rs \) -type f -exec touch {} + \
 && cargo build --release --locked -p siple-cli && install -D -s target/release/siple-cli /out/siple-cli

FROM debian:trixie-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libasound2t64 tini \
 && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 --create-home siple && install -d -o 10001 /data
COPY --from=build /out/siple-cli /usr/local/lib/siple/siple-cli
# No baked SIPLE_CONFIG: it would silently override a user's `-e ZYNOPHONE_CONFIG`. /usr/local/bin/siple-cli
# is a wrapper (so `docker exec` gets it too) that defaults the path to /data/config.toml only when
# neither variable is set, then runs the real binary.
RUN printf '%s\n' '#!/bin/sh' \
      'if [ -z "${SIPLE_CONFIG:-}" ] && [ -z "${ZYNOPHONE_CONFIG:-}" ]; then export SIPLE_CONFIG=/data/config.toml; fi' \
      'exec /usr/local/lib/siple/siple-cli "$@"' > /usr/local/bin/siple-cli \
 && chmod 755 /usr/local/bin/siple-cli
USER 10001
VOLUME ["/data"]
ENTRYPOINT ["/usr/bin/tini","--","siple-cli"]
CMD ["run","--headless","--no-audio"]
HEALTHCHECK --interval=10s --start-period=10s --start-interval=1s --timeout=5s CMD ["siple-cli","ctl","ping"]

# Optional (speech hooks; user-built, never published):
#   docker build --target speech -t siple:speech -f packaging/linux/Dockerfile .
# = runtime + espeak-ng (apt) + whisper.cpp built from a pinned tag + the ggml-base.en model.
FROM build-env AS whisper-build
ARG WHISPER_TAG=v1.9.5
RUN git clone --depth 1 --branch ${WHISPER_TAG} https://github.com/ggml-org/whisper.cpp /whisper \
 && cd /whisper && cmake -B build -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF -DCMAKE_BUILD_TYPE=Release \
 && cmake --build build -j"$(nproc)" --target whisper-cli \
 && install -D build/bin/whisper-cli /out/whisper-cli \
 && sh models/download-ggml-model.sh base.en && install -D models/ggml-base.en.bin /out/ggml-base.en.bin

FROM runtime AS speech
USER root
RUN apt-get update && apt-get install -y --no-install-recommends espeak-ng libgomp1 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=whisper-build /out/whisper-cli /usr/local/bin/whisper-cli
COPY --from=whisper-build /out/ggml-base.en.bin /usr/local/share/whisper/ggml-base.en.bin
USER 10001
