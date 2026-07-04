FROM rust:1-slim-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake make gcc g++ nasm pkg-config git ca-certificates libdav1d-dev \
    && rm -rf /var/lib/apt/lists/*
# SVT-AV1 from source at a pinned post-4.1 revision: distro packages are
# older or unoptimized (some ship debug builds that encode at half
# speed), and this revision carries the aarch64 kernels missing from
# 4.1 for the QM/tune=IQ still-image path (-36% encode time at equal
# quality). ABI-verified against the pregenerated bindings (identical
# struct size and field offsets).
RUN git clone --depth 1 https://gitlab.com/AOMediaCodec/SVT-AV1.git /svt \
    && git -C /svt fetch --depth 1 origin d3c4cb3947a8bfed0aa5a2be996b37bb117fa1bd \
    && git -C /svt checkout d3c4cb3947a8bfed0aa5a2be996b37bb117fa1bd \
    && cmake -S /svt -B /svt/build -DCMAKE_BUILD_TYPE=Release \
       -DBUILD_APPS=OFF -DBUILD_TESTING=OFF \
       -DCMAKE_INSTALL_PREFIX=/usr/local -DCMAKE_INSTALL_LIBDIR=lib \
    && make -C /svt/build -j"$(nproc)" install \
    && rm -rf /svt
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY examples ./examples
# Deployment-specific codegen tuning, e.g. --build-arg RUSTFLAGS="-C target-cpu=native".
ARG RUSTFLAGS=""
ENV RUSTFLAGS=${RUSTFLAGS}
RUN cargo build --release --locked --features avif

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends libdav1d7 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/lib/libSvtAv1Enc.so* /usr/local/lib/
RUN ldconfig
COPY --from=build /app/target/release/oximg /usr/local/bin/oximg
LABEL org.opencontainers.image.title="oximg" \
      org.opencontainers.image.description="High-performance image compression and resizing: JPEG, PNG, WebP, AVIF. Linear-light Lanczos, per-architecture SIMD." \
      org.opencontainers.image.source="https://github.com/oximg/oximg" \
      org.opencontainers.image.licenses="Apache-2.0"
ENV IMAGES_DIR=/images PORT=8081
EXPOSE 8081
CMD ["oximg"]
