# OpenSSH 证书登录的测试服务器：信任一个用户 CA（TrustedUserCAKeys），出示由主机 CA 签发的主机证书
# （HostCertificate，签给 127.0.0.1 与 localhost）。两个 CA 的私钥在构建镜像时生成，只在容器里；
# 测试用 docker exec 调容器里的 sign-user 现场签发短期用户证书（ssh-keygen -s）。
# 账号 termlet 只能用证书登录（关闭了密码与键盘交互；设置密码只为解锁账号，Alpine 的 sshd 不让锁定的账号登录）。
# 打开了 TCP 转发（Alpine 默认 AllowTcpForwarding no，与 testing/sshd 相同地改成 yes）：可以当跳板机，测试经跳板机
# （ProxyJump）登录时跳板机与目标主机的证书。
# 仅用于本地测试容器，不对外暴露。
#
# 用法（测试完删除容器与镜像）：
#   docker build -t termlet-cert-sshd testing/sshcert
#   docker run -d --name termlet-cert-sshd -p 2227:22 termlet-cert-sshd
#   TERMLET_TEST_CERT=127.0.0.1:2227 cargo nextest run -p termlet-core -E 'test(/cert/)'
#   docker rm -f termlet-cert-sshd && docker rmi termlet-cert-sshd
# 容器名不同时用 TERMLET_TEST_CERT_CONTAINER 指定。
FROM alpine:3.22

# PerSourcePenalties no：测试中故意的认证失败（过期、用户名不对的证书）不让之后的连接被暂时拒绝
# AllowTcpForwarding yes：当跳板机时要经它转发到目标主机
RUN apk add --no-cache openssh \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh termlet \
 && echo "termlet:termlet-e2e" | chpasswd \
 && ssh-keygen -q -t ed25519 -N '' -C termlet-test-user-ca -f /etc/ssh/user_ca \
 && ssh-keygen -q -t ed25519 -N '' -C termlet-test-host-ca -f /etc/ssh/host_ca \
 && ssh-keygen -q -s /etc/ssh/host_ca -I termlet-cert-sshd -h -n 127.0.0.1,localhost /etc/ssh/ssh_host_ed25519_key.pub \
 && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config \
 && sed -i 's/^#\?KbdInteractiveAuthentication.*/KbdInteractiveAuthentication no/' /etc/ssh/sshd_config \
 && sed -i 's/^#\?AllowTcpForwarding.*/AllowTcpForwarding yes/' /etc/ssh/sshd_config \
 && echo 'TrustedUserCAKeys /etc/ssh/user_ca.pub' >> /etc/ssh/sshd_config \
 && echo 'HostKey /etc/ssh/ssh_host_ed25519_key' >> /etc/ssh/sshd_config \
 && echo 'HostCertificate /etc/ssh/ssh_host_ed25519_key-cert.pub' >> /etc/ssh/sshd_config \
 && echo 'PerSourcePenalties no' >> /etc/ssh/sshd_config \
 && echo 'LogLevel VERBOSE' >> /etc/ssh/sshd_config

# sign-user <允许的用户名，逗号分隔> <有效期，ssh-keygen -V 的写法>：从标准输入读公钥，输出用户证书
RUN printf '%s\n' \
      '#!/bin/sh' \
      'set -e' \
      'dir=$(mktemp -d)' \
      'cat > "$dir/key.pub"' \
      'ssh-keygen -q -s /etc/ssh/user_ca -I termlet-test -z "$(date +%s)" -n "$1" -V "$2" "$dir/key.pub"' \
      'cat "$dir/key-cert.pub"' \
      'rm -rf "$dir"' \
      > /usr/local/bin/sign-user \
 && chmod 755 /usr/local/bin/sign-user

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
