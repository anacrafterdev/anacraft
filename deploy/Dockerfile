# `craft serve` as a container, from the published release rather than from
# source: the release binary is the one with the OAuth client and the
# subscription backend compiled in, and building it here would mean handing
# those to Docker as build args.
#
# `craft serve` binds 127.0.0.1 and answers only its own loopback origin, so
# nginx sits in front of it: it listens on 0.0.0.0 for the Service and
# presents the public origin as the loopback one. See nginx.conf.
FROM alpine:3.20

ARG VERSION=v0.35.0
ARG TARGET=x86_64-unknown-linux-musl

RUN apk add --no-cache ca-certificates curl nginx tini \
 && cd /tmp \
 && archive="anacraft-${VERSION}-${TARGET}.tar.gz" \
 && curl -fsSLO "https://github.com/mehfuzh/anacraft/releases/download/${VERSION}/${archive}" \
 && curl -fsSLO "https://github.com/mehfuzh/anacraft/releases/download/${VERSION}/SHA256SUMS" \
 && grep " ${archive}\$" SHA256SUMS | sha256sum -c - \
 && tar -xzf "${archive}" \
 && install -m 0755 "anacraft-${VERSION}-${TARGET}/craft" /usr/local/bin/craft \
 && rm -rf /tmp/* \
 && adduser -D -h /home/craft craft

COPY nginx.conf /etc/nginx/craft.conf.in
COPY entrypoint.sh /usr/local/bin/entrypoint.sh

USER craft
ENV HOME=/home/craft
WORKDIR /home/craft

EXPOSE 8080

ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/entrypoint.sh"]
