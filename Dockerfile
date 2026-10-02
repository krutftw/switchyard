# Builds Switchyard from source.
#
#   docker build -t switchyard .
#   docker run -p 8317:8317 -v switchyard-data:/data \
#     -e SWITCHYARD_ADMIN_SECRET=change-me switchyard
#
# The config file lives at /data/switchyard.toml and is created on first start.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p switchyard

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin switchyard \
    && mkdir -p /data && chown switchyard:switchyard /data
COPY --from=build /src/target/release/switchyard /usr/local/bin/switchyard
USER switchyard
WORKDIR /data
VOLUME /data
EXPOSE 8317
# Inside a container every client is "remote"; the admin secret is still required.
ENV SWITCHYARD_ADMIN_ALLOW_REMOTE=true
ENTRYPOINT ["/usr/local/bin/switchyard"]
CMD ["serve", "--config", "/data/switchyard.toml", "--host", "0.0.0.0"]
