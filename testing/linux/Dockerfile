# 本机在 Docker 中验证 Linux 版：编译、clippy、测试（与 .github/workflows/ci.yml 的 check job 相同的系统库与工具链）。
# 基于官方镜像 rust:1.99.1-bookworm（与 rust-toolchain.toml 的版本一致；升级工具链时这里一起改，再重建镜像），装上
# rustfmt、clippy、cargo-nextest 与 GPUI 在 Linux 上编译时链接的系统库：xkbcommon（含 x11）、xcb、fontconfig、freetype；
# wayland、vulkan 在运行时加载，编译时不需要。以普通用户 builder 运行：以 root 运行时，改权限为 000 的几个测试会跳过。
#
# 建镜像、起容器（cargo 的注册表与 target 目录放在卷中，容器删掉重建后不用重新下载、重新编译）：
#   docker build -t termlet-linux-build:1.99.1 testing/linux
#   docker run -d --name termlet-linux-build -v termlet-linux-cargo:/usr/local/cargo/registry \
#     -v termlet-linux-target:/target termlet-linux-build:1.99.1 sleep infinity
# 同步工作区（已跟踪与未忽略的文件；Windows 目录的绑定挂载太慢，所以复制一份。改了代码后重新同步）：
#   git ls-files -co --exclude-standard -z | tar --null --ignore-failed-read -T - -cf - \
#     | docker exec -i termlet-linux-build tar -xf - -C /src
# 运行（与 CI 相同：不生成调试信息、不增量编译；连测试容器的 sshd 时地址用 host.docker.internal）：
#   docker exec termlet-linux-build cargo clippy --workspace --all-targets --locked -- -D warnings
#   docker exec termlet-linux-build cargo nextest run --workspace --locked
#   docker exec termlet-linux-build cargo test --workspace --locked --doc
#   docker exec -e TERMLET_TEST_SSH=host.docker.internal:2222 -e TERMLET_TEST_TELNET=host.docker.internal:2323 \
#     termlet-linux-build cargo nextest run --locked -p termlet-core -p termlet-cli
# 在 Git Bash 中先 export MSYS_NO_PATHCONV=1，否则 /src、/target 等参数会被改写成 Windows 路径。
# 用完 docker rm -f termlet-linux-build；卷留着下次用。
# 固定到镜像的摘要，与 .github/workflows/release.yml 的 Linux 容器相同（升级时一起改）
FROM rust:1.99.1-bookworm@sha256:9181b56c2a21ab1d5e73cffe04fc762426d30aa7fac3c91141dd033f49a7fc8b

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
    libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libfontconfig-dev libfreetype-dev \
 && rm -rf /var/lib/apt/lists/*

# 官方镜像只有最小的工具链；测试运行器 cargo-nextest（与 CI 的版本相同）从 crates.io 编译，注册表缓存不留在镜像中
RUN rustup component add rustfmt clippy \
 && cargo install cargo-nextest --version 0.9.150 --locked \
 && rm -rf /usr/local/cargo/registry

# 卷第一次挂载时沿用这些目录的属主
RUN useradd -m -u 1000 builder \
 && mkdir -p /src /target /usr/local/cargo/registry \
 && chown builder /src /target /usr/local/cargo/registry

USER builder
WORKDIR /src
ENV CARGO_TARGET_DIR=/target/ci \
    CARGO_PROFILE_DEV_DEBUG=0 \
    CARGO_INCREMENTAL=0 \
    LANG=C.UTF-8
