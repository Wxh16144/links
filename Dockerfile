# 构建期需要网络拉 crates；运行期完全离线，只读挂载进来的 git 仓库。
# 国内网络可覆盖 crates 镜像源：
#   docker build --build-arg CARGO_SPARSE_INDEX=sparse+https://rsproxy.cn/index/ -t wxh16144/links .
FROM rust:1-alpine AS builder
ARG CARGO_SPARSE_INDEX=""
RUN if [ -n "$CARGO_SPARSE_INDEX" ]; then \
      mkdir -p /usr/local/cargo && \
      printf '[source.crates-io]\nreplace-with = "mirror"\n[source.mirror]\nregistry = "%s"\n' \
        "$CARGO_SPARSE_INDEX" > /usr/local/cargo/config.toml; \
    fi
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --locked

FROM alpine:3.22
# 索引直接读本地仓库，所以运行时只需要 git
RUN apk add --no-cache git && adduser -D -u 10001 app

COPY --from=builder /src/target/release/links /usr/local/bin/links

USER app
ENV PORT=8080 \
    REPO_DIR=/repo
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s \
    CMD wget -q -O- http://127.0.0.1:8080/_health > /dev/null || exit 1

ENTRYPOINT ["links"]
