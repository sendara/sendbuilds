FROM rust:1.88-bookworm AS sendbuilds-builder

ARG SENDBUILDS_GIT_REF=master

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates \
    git \
    libssl-dev \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /src/sendbuilds

RUN git clone https://github.com/Sendara/sendbuilds.git . \
  && git checkout "${SENDBUILDS_GIT_REF}" \
  && cargo build --release

FROM ubuntu:24.04

ARG TARGETARCH
ARG SENDBUILDS_GIT_REF=master
ARG NODE_MAJOR=22
ARG DOTNET_CHANNEL=8.0
ARG GLEAM_VERSION=v1.15.2
ARG DOCKER_BUILDX_VERSION=v0.25.0

ENV DEBIAN_FRONTEND=noninteractive
ENV DOTNET_ROOT=/usr/share/dotnet
ENV CARGO_HOME=/usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV DENO_INSTALL=/opt/deno
ENV PATH=/usr/local/go/bin:/usr/local/cargo/bin:/usr/share/dotnet:/opt/deno/bin:${PATH}

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    bash \
    build-essential \
    ca-certificates \
    clang \
    cmake \
    composer \
    curl \
    elixir \
    erlang \
    g++ \
    gcc \
    git \
    gnupg \
    gradle \
    jq \
    libffi-dev \
    libpq-dev \
    libsqlite3-dev \
    libssl-dev \
    libyaml-dev \
    make \
    maven \
    ninja-build \
    openjdk-21-jdk \
    php-cli \
    php-curl \
    php-mbstring \
    php-xml \
    php-zip \
    pkg-config \
    python3 \
    python3-pip \
    python3-venv \
    ruby-full \
    sqlite3 \
    unzip \
    xz-utils \
    zip \
    zlib1g-dev \
  && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | bash - \
  && apt-get update \
  && apt-get install -y --no-install-recommends docker.io golang-go nodejs \
  && npm install -g corepack@latest \
  && corepack enable \
  && ln -sf /usr/bin/python3 /usr/local/bin/python \
  && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /usr/local/libexec/docker/cli-plugins \
  && case "${TARGETARCH}" in \
    "amd64") buildx_arch="linux-amd64" ;; \
    "arm64") buildx_arch="linux-arm64" ;; \
    *) \
      echo "Unsupported TARGETARCH for docker buildx: ${TARGETARCH}." >&2; \
      exit 1; \
      ;; \
  esac \
  && curl -fsSL "https://github.com/docker/buildx/releases/download/${DOCKER_BUILDX_VERSION}/buildx-${DOCKER_BUILDX_VERSION}.${buildx_arch}" -o /usr/local/libexec/docker/cli-plugins/docker-buildx \
  && chmod +x /usr/local/libexec/docker/cli-plugins/docker-buildx

RUN curl -fsSL https://dot.net/v1/dotnet-install.sh -o /tmp/dotnet-install.sh \
  && chmod +x /tmp/dotnet-install.sh \
  && /tmp/dotnet-install.sh --channel "${DOTNET_CHANNEL}" --install-dir "${DOTNET_ROOT}" \
  && ln -sf "${DOTNET_ROOT}/dotnet" /usr/local/bin/dotnet \
  && rm -f /tmp/dotnet-install.sh

RUN curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh \
  && chmod +x /tmp/rustup-init.sh \
  && /tmp/rustup-init.sh -y --profile minimal --default-toolchain stable \
  && ln -sf /usr/local/cargo/bin/cargo /usr/local/bin/cargo \
  && ln -sf /usr/local/cargo/bin/rustc /usr/local/bin/rustc \
  && rm -f /tmp/rustup-init.sh

RUN case "${TARGETARCH}" in \
    "amd64") gleam_asset="gleam-${GLEAM_VERSION}-x86_64-unknown-linux-musl.tar.gz" ;; \
    *) \
      echo "Unsupported TARGETARCH for gleam: ${TARGETARCH}." >&2; \
      exit 1; \
      ;; \
  esac \
  && curl -fsSL "https://github.com/gleam-lang/gleam/releases/download/${GLEAM_VERSION}/${gleam_asset}" -o /tmp/gleam.tar.gz \
  && mv /tmp/gleam.tar.gz "/tmp/${gleam_asset}" \
  && curl -fsSL "https://github.com/gleam-lang/gleam/releases/download/${GLEAM_VERSION}/${gleam_asset}.sha256" -o "/tmp/${gleam_asset}.sha256" \
  && cd /tmp \
  && sha256sum -c "${gleam_asset}.sha256" \
  && tar -xzf "/tmp/${gleam_asset}" -C /usr/local/bin gleam \
  && chmod +x /usr/local/bin/gleam \
  && rm -f "/tmp/${gleam_asset}" "/tmp/${gleam_asset}.sha256"

RUN curl -fsSL https://deno.land/install.sh -o /tmp/deno-install.sh \
  && chmod +x /tmp/deno-install.sh \
  && /tmp/deno-install.sh \
  && ln -sf /opt/deno/bin/deno /usr/local/bin/deno \
  && rm -f /tmp/deno-install.sh

COPY --from=sendbuilds-builder /src/sendbuilds/target/release/sendbuilds /usr/local/bin/sendbuilds

RUN chmod +x /usr/local/bin/sendbuilds

RUN node -v \
  && docker buildx version \
  && npm -v \
  && corepack --version \
  && python3 --version \
  && ruby --version \
  && go version \
  && javac -version \
  && mvn -version \
  && gradle --version \
  && php --version \
  && composer --version \
  && rustc --version \
  && cargo --version \
  && dotnet --info \
  && elixir --version \
  && gleam --version \
  && deno --version \
  && gcc --version \
  && g++ --version \
  && make --version \
  && sendbuilds info

ENTRYPOINT ["/bin/sh"]
CMD ["-c", "sendbuilds info"]