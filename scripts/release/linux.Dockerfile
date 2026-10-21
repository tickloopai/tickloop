# The Linux environment of a local release (scripts/release-publish.mjs): it checks the workspace
# like CI and builds the static musl archive of the image's own architecture.
FROM rust:1-bookworm

ARG HERDR_VERSION=0.9.0
ARG HERDR_AARCH64_SHA256=9c8db20fb7e7427b138d5367113f1621ffd319f2f65d6f009e2594029115f0d2
ARG HERDR_X86_64_SHA256=4fa1a01158dd8043da92d31b270780b0dcc10603038d9b61cac4d81ab63fb71f

RUN apt-get update \
    && apt-get install --yes --no-install-recommends file git musl-tools nodejs python3 \
    && rm -rf /var/lib/apt/lists/*
RUN rustup update --no-self-update stable \
    && rustup default stable \
    && rustup component add clippy rustfmt \
    && rustup target add "$(uname -m)-unknown-linux-musl"
RUN case "$(uname -m)" in \
        aarch64) nextest=linux-arm; herdr=herdr-linux-aarch64; sha256=$HERDR_AARCH64_SHA256 ;; \
        x86_64) nextest=linux; herdr=herdr-linux-x86_64; sha256=$HERDR_X86_64_SHA256 ;; \
        *) echo "Unsupported architecture $(uname -m)" >&2; exit 1 ;; \
    esac \
    && curl --fail --silent --show-error --location "https://get.nexte.st/latest/$nextest" | tar -xzf - -C /usr/local/cargo/bin \
    && curl --fail --silent --show-error --location --output /usr/local/bin/herdr \
        "https://github.com/herdrdev/herdr/releases/download/v$HERDR_VERSION/$herdr" \
    && echo "$sha256  /usr/local/bin/herdr" | sha256sum --check --strict \
    && chmod +x /usr/local/bin/herdr
# Tests check file permissions that root would bypass.
RUN useradd --create-home --uid 1000 builder
ENV LANG=C.UTF-8
