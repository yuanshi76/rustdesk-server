# s6 image: hbbs + hbbr supervised by s6-overlay.
#
# Multi-arch: one `docker buildx build --platform linux/amd64,linux/arm64`
# produces both. The binaries are built outside this Dockerfile and are expected
# at bin/<TARGETARCH>/, i.e. bin/amd64/hbbs and bin/arm64/hbbs.

# Fetched on the build host, not under emulation.
FROM --platform=$BUILDPLATFORM alpine:3.20 AS s6
ARG TARGETARCH
ARG S6_OVERLAY_VERSION=3.2.0.0
RUN apk add --no-cache ca-certificates
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) s6_arch=x86_64 ;; \
      arm64) s6_arch=aarch64 ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    base="https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}"; \
    wget -qO /tmp/noarch.tar.xz "${base}/s6-overlay-noarch.tar.xz"; \
    wget -qO /tmp/arch.tar.xz   "${base}/s6-overlay-${s6_arch}.tar.xz"; \
    mkdir -p /s6root; \
    tar -C /s6root -Jxpf /tmp/noarch.tar.xz; \
    tar -C /s6root -Jxpf /tmp/arch.tar.xz; \
    rm -f /tmp/*.tar.xz

FROM busybox:stable
ARG TARGETARCH

COPY --from=s6 /s6root /
# Root certificates, so that rustdesk-utils geo-update can check who it is talking to.
# (This base has none, and busybox wget does not check certificates at all.)
COPY --from=s6 /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
RUN ln -s /run /var/run

COPY rootfs /
COPY bin/${TARGETARCH}/hbbs /usr/bin/hbbs
COPY bin/${TARGETARCH}/hbbr /usr/bin/hbbr
COPY bin/${TARGETARCH}/rustdesk-utils /usr/bin/rustdesk-utils
RUN chmod -v a+x /usr/bin/hbbs /usr/bin/hbbr /usr/bin/rustdesk-utils /usr/bin/healthcheck.sh

ENV RELAY=relay.example.com
ENV ENCRYPTED_ONLY=0
# Refuse a connection that presents no login token. See README.
ENV MUST_LOGIN=N
# Seconds a websocket may go silent - including the Pong answering our Ping -
# before hbbs closes it.
ENV WS_IDLE_TIMEOUT=90
# Y: download and keep current the GeoIP database (DB-IP "IP to City Lite", CC BY 4.0)
# at GEO_DB, default /data/geo.mmdb. Also set RELAY_LOCATIONS for it to be used.
ENV GEO_AUTO_UPDATE=N

EXPOSE 21115 21116 21116/udp 21117 21118 21119

HEALTHCHECK --interval=10s --timeout=5s CMD /usr/bin/healthcheck.sh

WORKDIR /data

VOLUME /data

ENTRYPOINT ["/init"]
