FROM rust:1-slim-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake make gcc g++ nasm pkg-config git ca-certificates libdav1d-dev \
    && rm -rf /var/lib/apt/lists/*
# SVT-AV1 4.1 from source: distro packages are either older or built
# without optimization (some ship debug builds that encode at half speed).
RUN git clone --depth 1 -b v4.1.0 https://gitlab.com/AOMediaCodec/SVT-AV1.git /svt \
    && cmake -S /svt -B /svt/build -DCMAKE_BUILD_TYPE=Release \
       -DBUILD_APPS=OFF -DBUILD_TESTING=OFF \
       -DCMAKE_INSTALL_PREFIX=/usr/local -DCMAKE_INSTALL_LIBDIR=lib \
    && make -C /svt/build -j"$(nproc)" install \
    && rm -rf /svt
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY examples ./examples
RUN cargo build --release --locked --features avif

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends libdav1d7 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/lib/libSvtAv1Enc.so* /usr/local/lib/
RUN ldconfig
COPY --from=build /app/target/release/oximg /usr/local/bin/oximg
ENV IMAGES_DIR=/images PORT=8081
EXPOSE 8081
CMD ["oximg"]
