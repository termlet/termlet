# 没有 SFTP 的测试服务器：OpenSSH 的 sshd 去掉了 sftp 子系统（不装 sftp-server，sshd_config 中没有 Subsystem），
# 文件管理改用 SCP；ls 是 BusyBox 的（不装 coreutils）。同时关闭端口转发（AllowTcpForwarding no），测试“通过本机代理”
# 被服务器拒绝时的说明。账号密码与 testing/sshd 相同，仅用于本地测试容器，不对外暴露：
# - termlet：有 scp（openssh-client-default 中的 /usr/bin/scp）
# - noscp：登录 shell 的 PATH 中没有 /usr/bin，找不到 scp（测试“连 scp 也没有”的说明）
#
# 用法（测试完删除容器与镜像）：
#   docker build -t termlet-proxy-nosftp testing/nosftp
#   docker run -d --name termlet-proxy-nosftp -p 2225:22 termlet-proxy-nosftp
#   TERMLET_TEST_NOSFTP=127.0.0.1:2225 cargo nextest run -p termlet-core -E 'test(/nosftp/)'
#   docker rm -f termlet-proxy-nosftp && docker rmi termlet-proxy-nosftp
FROM alpine:3.22

RUN apk add --no-cache openssh-server openssh-client-default \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh termlet \
 && echo "termlet:termlet-e2e" | chpasswd \
 && printf '%s\n' '#!/bin/sh' 'PATH=/bin:/sbin' 'export PATH' 'exec /bin/sh "$@"' > /usr/local/bin/noscp-shell \
 && chmod 755 /usr/local/bin/noscp-shell \
 && echo /usr/local/bin/noscp-shell >> /etc/shells \
 && adduser -D -s /usr/local/bin/noscp-shell noscp \
 && echo "noscp:termlet-e2e" | chpasswd \
 && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config \
 && sed -i 's/^#\?AllowTcpForwarding.*/AllowTcpForwarding no/' /etc/ssh/sshd_config \
 && sed -i '/^Subsystem/d' /etc/ssh/sshd_config \
 && echo 'PerSourcePenalties no' >> /etc/ssh/sshd_config \
 && ! test -e /usr/lib/ssh/sftp-server

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
