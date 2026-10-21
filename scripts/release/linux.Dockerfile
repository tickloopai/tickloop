# The Linux environment of a local release (scripts/release-publish.mjs): it builds and
# smoke-tests the static musl archive of the image's own architecture.
FROM rust:1-bookworm

RUN apt-get update \
    && apt-get install --yes --no-install-recommends file musl-tools \
    && rm -rf /var/lib/apt/lists/*
RUN rustup update --no-self-update stable \
    && rustup default stable \
    && rustup target add "$(uname -m)-unknown-linux-musl"
RUN useradd --create-home --uid 1000 builder
ENV LANG=C.UTF-8
