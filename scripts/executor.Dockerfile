FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends bash ca-certificates git ripgrep python3 curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 executor && useradd --uid 10001 --gid 10001 --home-dir /workspace executor
# Build the binary off the production server and put it in this build context.
COPY --chmod=0755 peri-executor /usr/local/bin/peri-executor
USER 10001:10001
ENV HOME=/workspace
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/peri-executor", "--state-dir", "/state", "--listen", "0.0.0.0:42371", "--container-listen"]
