# OpenContextEngine（Rust 版）多阶段构建。
#
# 阶段 1（builder）：cargo build --release。
# - 静态嵌入模型（Model2Vec）走 HF 缓存或网络下载，构建期不依赖本地模型
# - sqlx 用运行时查询（无 query! 宏），无 DATABASE_URL 构建期依赖
#
# 阶段 2（runtime）：仅拷贝二进制 + CA 证书。
# - 二进制自带 tree-sitter 语法（静态链接）、rusqlite bundled SQLite
# - TriviumDB 单文件向量库在运行时创建（挂载卷）
#
# 构建：docker build -f rust/Dockerfile -t oce-rust .
# 平台注意：默认按构建机架构；交叉编译用 buildx --platform。

FROM rust:1.97-slim AS builder

WORKDIR /build

# 构建依赖：g++（tree-sitter 语法 C++ 源码 + esaxx-rs C++ 组件）、pkg-config
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    g++ \
    && rm -rf /var/lib/apt/lists/*

# 先拷贝清单，利用层缓存编译依赖
COPY rust/Cargo.toml rust/Cargo.lock ./
COPY rust/crates/oce-core/Cargo.toml crates/oce-core/Cargo.toml
COPY rust/crates/oce-infra/Cargo.toml crates/oce-infra/Cargo.toml
COPY rust/crates/oce-app/Cargo.toml crates/oce-app/Cargo.toml
COPY rust/crates/oce-server/Cargo.toml crates/oce-server/Cargo.toml
COPY rust/crates/oce-bench/Cargo.toml crates/oce-bench/Cargo.toml

# 占位 src 让 cargo 能解析 workspace（真源码下一层覆盖触发重编译）
RUN mkdir -p crates/oce-core/src crates/oce-infra/src crates/oce-app/src crates/oce-server/src crates/oce-bench/src \
    && echo "pub fn _placeholder() {}" > crates/oce-core/src/lib.rs \
    && echo "pub fn _placeholder() {}" > crates/oce-infra/src/lib.rs \
    && echo "pub fn _placeholder() {}" > crates/oce-app/src/lib.rs \
    && echo "fn main() {}" > crates/oce-server/src/main.rs     && touch crates/oce-server/src/lib.rs crates/oce-server/src/mcp.rs crates/oce-server/src/routes.rs crates/oce-server/src/schemas.rs \
    && echo "fn main() {}" > crates/oce-bench/src/main.rs

RUN cargo build --release -p oce-server

# 真源码
COPY rust/crates crates

# 触碰源文件使 cargo 重编译（占位层与真源码的 mtime 差可能不足以失效层缓存）
RUN find crates -name "*.rs" -exec touch {} + \
    && cargo build --release -p oce-server

# ───────────────────────── runtime

FROM debian:trixie-slim

# CA 证书（HTTPS 嵌入/LLM 端点）+ tini（信号转发，容器内 PID 1）
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    tini \
    && rm -rf /var/lib/apt/lists/*

# 非 root 运行
RUN useradd --system --create-home --shell /usr/sbin/nologin oce
RUN mkdir -p /var/lib/oce && chown oce:oce /var/lib/oce

COPY --from=builder /build/target/release/oce /usr/local/bin/oce

USER oce
WORKDIR /var/lib/oce
VOLUME /var/lib/oce

EXPOSE 8986

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["oce", "serve", "--data-dir", "/var/lib/oce", "--host", "0.0.0.0", "--port", "8986"]
