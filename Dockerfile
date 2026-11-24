# Artifact Service 最小运行镜像（多阶段；无 apt 操作）。
# 基础镜像 CI 中从 Docker Hub 拉取；本地构建需提前准备：
#   docker pull rust:1-bookworm && docker pull debian:bookworm-slim

FROM rust:1-bookworm AS builder
ARG ARTIFACT_GIT_SHA=unknown
ENV ARTIFACT_GIT_SHA=${ARTIFACT_GIT_SHA}
WORKDIR /build
COPY . .
RUN cargo build --release -p artifact-api && \
    cp target/release/artifact-api /out-artifact-api

FROM debian:bookworm-slim AS runtime
# CA 证书来自 builder（buildpack-deps 自带 ca-certificates），运行层不做任何网络安装。
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /out-artifact-api /usr/local/bin/artifact-api
# 非 root 运行。
RUN useradd --system --no-create-home artifact
USER artifact
EXPOSE 8110
ENTRYPOINT ["/usr/local/bin/artifact-api"]
CMD ["serve"]
