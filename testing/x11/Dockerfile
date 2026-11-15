# X11 转发的测试服务器：开启了 X11Forwarding 的 OpenSSH，带 xauth（sshd 用它把伪造的 cookie 写进用户的 Xauthority）
# 与 X 程序 xdpyinfo。账号密码与 testing/sshd 相同，仅用于本地测试容器，不对外暴露。
# AddressFamily inet：容器里没有 IPv6 时，sshd 按默认值监听 X11 显示（localhost:6010）会失败。
#
# 用法（测试完删除容器与镜像）：
#   docker build -t termlet-conn-x11 testing/x11
#   docker run -d --name termlet-conn-x11 -p 2224:22 termlet-conn-x11
#   TERMLET_TEST_X11=127.0.0.1:2224 cargo nextest run -p termlet-core -E 'test(/x11/)'
#   docker rm -f termlet-conn-x11 && docker rmi termlet-conn-x11
FROM alpine:3.22

RUN apk add --no-cache openssh xauth xdpyinfo \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh termlet \
 && echo "termlet:termlet-e2e" | chpasswd \
 && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config \
 && sed -i 's/^#\?X11Forwarding.*/X11Forwarding yes/' /etc/ssh/sshd_config \
 && echo 'AddressFamily inet' >> /etc/ssh/sshd_config \
 && echo 'PerSourcePenalties no' >> /etc/ssh/sshd_config

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
