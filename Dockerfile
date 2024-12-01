FROM rust:1.82-bookworm AS builder

WORKDIR /src

COPY . .

RUN --mount=type=cache,target=/src/target \
    cargo install --path .

FROM debian:bookworm

RUN apt-get update && \
    apt-get upgrade -y && \
    apt-get install -y \
        ca-certificates tini && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/cargo/bin/hashtag-importer /usr/local/bin/hashtag-importer
WORKDIR /app
ENTRYPOINT ["tini", "--", "hashtag-importer"]
CMD ["run"]
