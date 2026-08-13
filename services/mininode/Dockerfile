FROM rust:1.89-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p msnnext --bin msnnext

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/msnnext /usr/local/bin/msnnext
VOLUME ["/data"]
EXPOSE 4001/tcp 4001/udp
ENTRYPOINT ["/usr/local/bin/msnnext"]
CMD ["--relay-server", "--listen", "/ip4/0.0.0.0/udp/4001/quic-v1", "--listen-tcp", "/ip4/0.0.0.0/tcp/4001", "--identity", "/data/identity.key", "--emotes", "/data/emoticons", "--downloads", "/data/downloads", "--history", "/data/history.db", "--name", "msnnext-mininode"]

