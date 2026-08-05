# syntax=docker/dockerfile:1
ARG TARGET=ngit-cli

ARG ALPINE_VERSION="3.21"
ARG RUST_VERSION="1.95"
ARG BUILD_IMAGE=docker.io/rust:${RUST_VERSION}-alpine${ALPINE_VERSION}

FROM ${BUILD_IMAGE} AS builder
RUN apk --no-cache add \
  libressl-dev
WORKDIR /src
COPY . .
# perform single-architecture build.
# TARGETARCH automatically set by buildx.
ARG TARGETARCH=amd64
RUN cargo build \
        --release \
        --locked \
        --target "$(echo "${TARGETARCH}" | sed -e s/arm64/aarch64/ -e s/amd64/x86_64/)-unknown-linux-musl"

RUN find -mmin -2 -type f

RUN mkdir -p /out/ && \
  find target \(  \
      -path '*/release/git-remote-nostr' -o \
      -path '*/release/ngit'  \
    \) -exec mv -v {} /out/ \;


FROM docker.io/alpine:${ALPINE_VERSION} AS base
RUN apk --no-cache add \
  git libcrypto3 libressl
ARG UID=65534
ARG GID=65534
RUN mkdir -p /opt/ngit && chown ${UID}:${GID} /opt/ngit
USER ${UID}:${GID}
ENV HOME=/opt/ngit
WORKDIR /opt/ngit


FROM base AS ngit-cli
COPY --from=builder /out/* /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/ngit"]


FROM ngit-cli AS ngit-utils
USER root
ARG EXTRA_PACKAGES="bash curl git grep jq netcat-openbsd psmisc sed strace tar xz"
RUN apk --no-cache add ${EXTRA_PACKAGES}
ARG UID=65534
ARG GID=65534
USER ${UID}:${GID}
ENTRYPOINT ["/bin/bash"]


FROM ${TARGET}
