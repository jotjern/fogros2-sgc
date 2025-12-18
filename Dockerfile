# Build stage with Rust, ROS2, and cargo-chef for caching
FROM osrf/ros:humble-desktop AS chef
RUN apt update && apt install -y build-essential curl pkg-config libssl-dev protobuf-compiler clang
RUN curl https://sh.rustup.rs -sSf | bash -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
RUN cargo install cargo-chef --locked
RUN apt-get update && apt-get install -y ros-humble-rmw-cyclonedds-cpp iproute2
WORKDIR /app

# Planner stage - analyze dependencies
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Builder stage - compile with cached dependencies
FROM chef AS builder
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json

# Build dependencies (cached)
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/app/target \
    . /opt/ros/humble/setup.sh && cargo chef cook --recipe-path recipe.json

# Copy source and build
COPY . .
RUN mkdir /app/bins

# Build main SGC binary
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/app/target \
    . /opt/ros/humble/setup.sh && cargo build && cp /app/target/debug/sgc /app/bins/sgc

# Build signaling server (release mode)
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/app/target \
    cd /app/signaling && cargo build --release && \
    cp /app/target/release/sgc_signaling_server /app/bins/sgc_signaling_server

# Runtime image
FROM chef
WORKDIR /

# Copy binaries and configs
COPY --from=builder /app/bins/sgc_signaling_server /signaling_server
COPY --from=builder /app/bins/sgc /sgc
COPY --from=builder /app/bench /fog_ws
COPY --from=builder /app/src /src
COPY --from=builder /app/scripts /scripts

# Create secrets directory for demo
RUN mkdir -p /secrets/demo && \
    head -c 32 /dev/urandom > /secrets/demo/secret.key && \
    chmod 600 /secrets/demo/secret.key

CMD ["bash", "-c", "source /opt/ros/humble/setup.bash && /sgc router"]
