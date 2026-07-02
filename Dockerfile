FROM rust:slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake make gcc g++ nasm && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /app/target/release/oximg /usr/local/bin/oximg
ENV IMAGES_DIR=/images PORT=8081
EXPOSE 8081
CMD ["oximg"]
