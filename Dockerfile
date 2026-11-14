# herdr-pm's standalone commands in a container, for CI jobs and Git
# servers that publish catalogs (`index build`, `index check`, `search`,
# `sources`). Plugin management itself needs a local Herdr and is not served
# by this image.
FROM alpine:3.24

ARG TARGETARCH

# Repositories mounted from the host belong to another uid
RUN apk add --no-cache ca-certificates git openssh-client \
    && git config --system --add safe.directory '*'

COPY ${TARGETARCH}/herdr-pm /usr/local/bin/herdr-pm

WORKDIR /work
ENTRYPOINT ["herdr-pm"]
CMD ["--help"]
