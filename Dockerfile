# ---- build stage ----
FROM rust:1-bookworm AS build
WORKDIR /app
COPY . .
# Build only the server crate. firmware-rs is a SEPARATE (xtensa) workspace, excluded
# via .dockerignore, so this host build never tries to compile it.
RUN cargo build -p server --release

# ---- runtime stage ----
FROM debian:bookworm-slim
# ca-certificates + libssl3 for reqwest's HTTPS calls to ESPN (native-tls/OpenSSL).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/server /usr/local/bin/server
# OTA firmware dists (committed firmware.bin + firmware.json per release
# channel under {stable,beta,dev}/, built by scripts/release-firmware.sh
# <channel>). The server serves them at GET /firmware/<channel>/latest +
# GET /firmware/<channel>.bin; an absent channel dir just means that channel
# is off (no offers from it, 404s on its routes).
COPY --from=build /app/firmware-dist /firmware-dist
ENV FIRMWARE_DIR=/firmware-dist
# Railway injects $PORT and mounts a volume here; set STATE_DIR=/data in the service
# so pairings/configs survive redeploys. The dummy service overrides the start
# command to `server --demo`; the live service uses the default below.
RUN mkdir -p /data
ENV STATE_DIR=/data
EXPOSE 7878
CMD ["server"]
