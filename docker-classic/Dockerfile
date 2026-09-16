# Classic image: just the binaries, no supervisor. One process per container.
#
# Multi-arch: binaries are expected at bin/<TARGETARCH>/, built outside this
# Dockerfile.
FROM scratch
ARG TARGETARCH
COPY bin/${TARGETARCH}/hbbs /usr/bin/hbbs
COPY bin/${TARGETARCH}/hbbr /usr/bin/hbbr
COPY bin/${TARGETARCH}/rustdesk-utils /usr/bin/rustdesk-utils
WORKDIR /root
ENV HOME=/root
EXPOSE 21115 21116 21116/udp 21117 21118 21119
# No ENTRYPOINT on purpose: existing compose files pass `command: hbbs ...`
# or `command: hbbr`, the same as upstream's classic image.
