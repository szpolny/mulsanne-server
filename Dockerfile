# Build and runtime use the same Debian release so the binary's glibc matches.
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p mulsanne-server

FROM debian:bookworm-slim
# Root certificates for TLS to object storage (the S3 client uses the system store).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/mulsanne-server /usr/local/bin/mulsanne-server
EXPOSE 8080
USER nobody
CMD ["mulsanne-server"]
