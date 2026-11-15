# termlet 端到端测试与性能基线使用的 OpenSSH 服务器。
# 账号密码仅用于本地和 CI 测试容器，不对外暴露。
# AcceptEnv LANG LC_* 与 Debian / Ubuntu 的默认配置相同（SFTP 跟随终端目录用到的会话标记 LC_TERMLET_SESSION）。
# rz / sz（lrzsz）与 trz / tsz（trzsz）用于测试 ZModem 与 trzsz 传输，只在测试容器中，不随 Termlet 发布。
# 同时在 23 端口运行 busybox 的 telnetd（测试 Telnet 主机），与 SSH 使用同一个测试账号。

# lrzsz：Alpine 的软件仓库里没有，从源码编译（校验源码包的 SHA-256）；老代码按 gnu89 编译
FROM alpine:latest AS lrzsz
RUN apk add --no-cache build-base curl \
 && curl -fsSL https://ohse.de/uwe/releases/lrzsz-0.12.20.tar.gz -o /tmp/lrzsz.tgz \
 && echo "c28b36b14bddb014d9e9c97c52459852f97bd405f89113f30bee45ed92728ff1  /tmp/lrzsz.tgz" | sha256sum -c - \
 && tar xzf /tmp/lrzsz.tgz -C /tmp \
 && cd /tmp/lrzsz-0.12.20 \
 && CFLAGS="-O2 -std=gnu89 -Wno-implicit-function-declaration -Wno-implicit-int -Wno-incompatible-pointer-types" ./configure --disable-nls \
 && make \
 && install -m 755 src/lrz /usr/local/bin/rz \
 && install -m 755 src/lsz /usr/local/bin/sz

# trzsz：编译 trzsz-go 的 trz、tsz（MIT）
FROM golang:1.26-alpine AS trzsz
RUN CGO_ENABLED=0 go install github.com/trzsz/trzsz-go/cmd/trz@v1.2.0 github.com/trzsz/trzsz-go/cmd/tsz@v1.2.0

FROM alpine:latest

# PerSourcePenalties no：OpenSSH 9.8 起默认在认证失败后暂时拒绝该来源的连接（10.2 为 15 秒以上），
# 测试中故意的认证失败会让之后的用例连不上（所有连接都来自 Docker 网关的同一个地址），测试容器中关闭。
# MaxStartups：默认同时只接受 10 个还没认证完的连接（超过的随机断开），并行跑的测试会偶尔连不上，测试容器中放宽
RUN apk add --no-cache openssh bash coreutils tmux vim htop busybox-extras \
 && ssh-keygen -A \
 && adduser -D -s /bin/bash termlet \
 && echo "termlet:termlet-e2e" | chpasswd \
 && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config \
 && sed -i 's/^#\?AllowTcpForwarding.*/AllowTcpForwarding yes/' /etc/ssh/sshd_config \
 && echo 'AcceptEnv LANG LC_*' >> /etc/ssh/sshd_config \
 && echo 'PerSourcePenalties no' >> /etc/ssh/sshd_config \
 && echo 'MaxStartups 100:30:200' >> /etc/ssh/sshd_config

# 以 root 身份打开 SFTP（sudo）的测试用户，密码同上：sudo-nopw 只能免密执行 sftp-server，sudo-pw 要密码，
# sudo-srv 要密码且只能执行 sftp-server，sudo-deny 不在 sudoers 中。sudo 移到不在默认 PATH 中的目录，只有这些
# 用户的登录 shell 把它加进 PATH：termlet 用户仍然"没有 sudo"（主机工具的测试依赖这一点）
RUN apk add --no-cache sudo \
 && mkdir -p /usr/local/libexec/termlet-sudo \
 && mv /usr/bin/sudo /usr/local/libexec/termlet-sudo/sudo \
 && printf '%s\n' '#!/bin/sh' 'PATH=/usr/local/libexec/termlet-sudo:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin' 'export PATH' 'exec /bin/sh "$@"' > /usr/local/bin/termlet-sudo-shell \
 && chmod 755 /usr/local/bin/termlet-sudo-shell \
 && for u in sudo-nopw sudo-pw sudo-srv sudo-deny; do adduser -D -s /usr/local/bin/termlet-sudo-shell "$u" && echo "$u:termlet-e2e" | chpasswd; done \
 && printf '%s\n' 'sudo-nopw ALL=(root) NOPASSWD: /usr/lib/ssh/sftp-server' 'sudo-pw ALL=(ALL) ALL' 'sudo-srv ALL=(root) /usr/lib/ssh/sftp-server' > /etc/sudoers.d/termlet-test \
 && chmod 440 /etc/sudoers.d/termlet-test

COPY --from=lrzsz /usr/local/bin/rz /usr/local/bin/sz /usr/local/bin/
COPY --from=trzsz /go/bin/trz /go/bin/tsz /usr/local/bin/

EXPOSE 22 23
CMD ["sh", "-c", "telnetd -p 23 -l /bin/login && exec /usr/sbin/sshd -D -e"]
