# Stage 1: Build
FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Dependency caching layer: build deps with dummy src, then replace
RUN mkdir src && echo 'fn main(){}' > src/main.rs && touch src/lib.rs && cargo build --release && rm -rf src
COPY src/ src/
RUN touch src/main.rs src/lib.rs && cargo build --release

# Stage 2: Runtime
FROM gcr.io/distroless/static-debian12
COPY --from=builder /app/target/release/tts-bridge /
EXPOSE 8000
ENTRYPOINT ["/tts-bridge"]
