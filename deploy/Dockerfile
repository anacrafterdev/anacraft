# `craft serve --public-url`, as a container, from the published release
# rather than from source: the release binary is the one with the
# subscription service compiled in. The Google Web client it signs visitors
# in with is not compiled in — it arrives at runtime, from Secret Manager.
FROM alpine:3.20

ARG VERSION=v0.36.0
ARG TARGET=x86_64-unknown-linux-musl

RUN apk add --no-cache ca-certificates curl tini \
 && cd /tmp \
 && archive="anacraft-${VERSION}-${TARGET}.tar.gz" \
 && curl -fsSLO "https://github.com/anacrafterdev/anacraft/releases/download/${VERSION}/${archive}" \
 && curl -fsSLO "https://github.com/anacrafterdev/anacraft/releases/download/${VERSION}/SHA256SUMS" \
 && grep " ${archive}\$" SHA256SUMS | sha256sum -c - \
 && tar -xzf "${archive}" \
 && install -m 0755 "anacraft-${VERSION}-${TARGET}/craft" /usr/local/bin/craft \
 && rm -rf /tmp/* \
 && adduser -D -h /home/craft craft

COPY entrypoint.sh /usr/local/bin/entrypoint.sh

USER craft
ENV HOME=/home/craft
WORKDIR /home/craft

EXPOSE 8080

ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/entrypoint.sh"]
