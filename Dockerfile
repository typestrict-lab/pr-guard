# pr-guard with the TS-PRGuard weights inside: no download at check time, the easiest way to run it on Jenkins,
# Bitrise, GitLab and any CI that runs jobs in containers. Published as ghcr.io/typestrict-lab/pr-guard.
#
#   docker build -t pr-guard .
#   docker run --rm -v "$PWD:/repo" -w /repo pr-guard check --base origin/main
#
# linux/amd64 and linux/arm64. ONNX Runtime is linked statically (ort's prebuilt for *-unknown-linux-gnu), so the
# runtime image only needs glibc and libstdc++, both already in debian slim.

FROM rust:1.98-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/pr-guard /pr-guard

# The weights at the revision this release pins, each file checked by SHA-256 (scripts/fetch-model.sh; it pins the same
# revision as `pr-guard model pull`, and a build fails rather than ship a file that does not match).
FROM debian:trixie-slim AS model
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
COPY scripts/fetch-model.sh /fetch-model.sh
RUN sh /fetch-model.sh /model

FROM debian:trixie-slim
# git: pr-guard reads the change from the repository; ca-certificates: the host APIs over HTTPS.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/* \
 # CI mounts the checkout owned by another user; git would refuse it as "dubious ownership".
 && git config --system --add safe.directory '*'
COPY --from=build /pr-guard /usr/local/bin/pr-guard
# The default model (pr-guard:1) as a cache hit; another `model` in the policy downloads next to it.
COPY --from=model /model /opt/pr-guard/cache/pr-guard-1
ENV PR_GUARD_CACHE=/opt/pr-guard/cache
LABEL org.opencontainers.image.source="https://github.com/typestrict-lab/pr-guard" \
      org.opencontainers.image.description="pr-guard with the TS-PRGuard weights" \
      org.opencontainers.image.licenses="Apache-2.0"
WORKDIR /repo
ENTRYPOINT ["pr-guard"]
CMD ["--help"]
