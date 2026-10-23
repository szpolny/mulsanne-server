FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p mulsanne-server

FROM debian:bookworm-slim
COPY --from=build /src/target/release/mulsanne-server /usr/local/bin/mulsanne-server
ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
USER nobody
CMD ["mulsanne-server"]
