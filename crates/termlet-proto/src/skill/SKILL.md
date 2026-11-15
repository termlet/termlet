---
name: termlet
description: Run commands and upload/download files on servers the user has saved in Termlet, via the `termlet` CLI, instead of calling ssh/scp/sftp directly. Use when the user asks to run something on a remote host, check a server, or transfer files to/from a server. 在用户已保存在 termlet 中的服务器上执行命令、上传下载文件时使用，替代直接调用 ssh/scp。
---

# termlet CLI

termlet 是用户的 SSH 客户端。用户已在 termlet 中保存了服务器及其凭据，`termlet` 命令行工具让你通过这些已保存的主机执行命令、传输文件，**凭据始终留在 termlet 内，你看不到也不需要密码**。

需要操作远程服务器时，优先使用 `termlet`，不要直接调用 `ssh`、`scp`、`sftp`。

## 命令

```bash
termlet ls                                  # 列出允许 CLI 访问的主机
termlet ls -q web                           # 按关键词筛选（名称、地址、标签等）
termlet exec <主机> "<命令>"                 # 执行一条远程命令
termlet exec <主机> --stdin                  # 从标准输入读取命令
termlet exec --terminal <主机> "<命令>"      # 在用户已在 Termlet 中打开的这台主机的终端里执行
termlet upload <主机> <本地文件> <远程路径>    # 上传单个文件
termlet download <主机> <远程文件> <本地路径>  # 下载单个文件
termlet help                                # 完整用法
```

`<主机>` 可以是 `termlet ls` 返回的 `id`，也可以是主机名称（`name`）。先运行 `termlet ls` 确认可用主机，不要猜测。

示例：

```bash
termlet ls
termlet exec web-1 "uptime"
termlet exec web-1 "cd /var/log && tail -n 100 nginx/error.log"
termlet upload web-1 ./dist/app.tar.gz /tmp/
termlet download web-1 /var/log/nginx/access.log .
```

## 输出格式

标准输出被管道或程序捕获时（你调用时通常就是这种情况），输出 **JSON**；在终端中直接运行时输出可读文本。JSON 字段名与错误码固定为英文。

`termlet ls`：

```json
[{"id": "h_1a2b", "name": "web-1", "host": "10.0.0.5", "port": 22, "username": "deploy", "protocol": "SSH"}]
```

`termlet exec`：

```json
{"exitCode": 0, "stdout": "...", "stderr": "...", "truncated": false, "durationMs": 182}
```

`termlet upload` / `termlet download`（`local`、`remote` 是实际写入的完整路径；`protocol` 是 `sftp`，服务器没有 SFTP 时为 `scp`，结果相同）：

```json
{"local": "D:\\work\\app.tar.gz", "remote": "/tmp/app.tar.gz", "bytes": 1048576, "durationMs": 930, "protocol": "sftp"}
```

出错时（退出码非 0，且不是远程命令自身的退出码）：

```json
{"error": "host_not_found", "message": "..."}
```

`error` 是稳定的错误码，可据此判断；`message` 是给人看的说明（跟随系统语言，命中的规则、路径等也在其中），仅供排查。常见错误码：`cli_disabled`、`app_not_running`、`host_not_found`、`host_not_allowed`、`command_blocked`、`command_denied`、`app_locked`、`hostkey_unknown`、`proxy_command_unconfirmed`、`auth_failed`、`auth_interaction_required`、`credential_missing`、`path_not_found`、`permission_denied`、`connect_failed`、`vault_locked`、`terminal_not_open`、`terminal_busy`。

## 退出码

| 退出码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 用法错误（参数不对、本地文件不存在等） |
| 2 | 主机不存在 |
| 3 | 主机未允许 CLI 访问 |
| 4 | 命令命中危险命令规则：被拦截（`command_blocked`），或在 Termlet 中未获批准（`command_denied`，用户拒绝或等待超时） |
| 5 | 连接或执行失败（网络、认证、指纹、SFTP 等） |
| 6 | Termlet 已锁定（应用锁 `app_locked`，或凭据保险库 `vault_locked`），需要用户在 Termlet 中解锁 |
| 7 | 外部 CLI 已关闭（`cli_disabled`），或 Termlet 没有运行且用户不允许自动启动（`app_not_running`），需要用户在 Termlet 中开启或先打开 Termlet |
| 其他 | `exec` 连接成功后，透传远程命令自身的退出码 |

注意：`exec` 的远程命令自身也可能返回 1~7，此时以 JSON 中是否有 `exitCode` 字段（成功执行）还是 `error` 字段（termlet 自身出错）来区分。

## 使用要点

### 每次执行都是无状态的

每次 `termlet exec` 都新建一个连接，执行完即关闭。`cd`、`export`、`source` 等对后续调用**不保留**。需要在同一上下文中执行多步时，用 `&&` 串联在一条命令里：

```bash
termlet exec web-1 "cd /srv/app && git pull && make build"
termlet exec web-1 "export NODE_ENV=production && cd /srv/app && npm run migrate"
```

### 在已打开的终端中执行（`--terminal`）

经堡垒机（JumpServer 等）登录的服务器，往往不允许新建连接（`exec` 每次都新建连接），或者需要的环境只在用户已登录的那个终端里（`sudo -i` 之后、`kubectl exec` 进入的容器等）。这时加 `--terminal`，命令会写进用户在 Termlet 中**已打开**的这台主机的终端（最新打开的那个）执行，用户看得见：

```bash
termlet exec --terminal web-1 "kubectl get pods -n prod"
```

- 与普通 `exec` 不同，命令**继承终端的状态**：在终端的当前目录、当前用户与已导出的环境变量下执行；但命令在子进程中运行，其中的 `cd`、`export` 不会改变终端本身。
- 远程需要 bash 或 sh；标准输入为空（不会卡在交互提示上），标准错误并入标准输出（JSON 中 `stderr` 为空）。
- 终端正在运行程序或全屏程序（`vim`、`top`）时返回 `terminal_busy`；没有打开的终端时返回 `terminal_not_open`。不要替用户打开或关闭程序，请用户回到 shell 提示符后重试。
- 用户为这台主机设置了 CLI 专用凭据（服务器上的低权限账号）时不能使用 `--terminal`，返回 `bad_request`（退出码 1）：终端以用户本人的身份运行，会绕过这道权限限制。去掉 `--terminal` 改用普通 `exec`；普通 `exec` 也不行时，请用户自己在终端中执行。
- 只在用户明确要求、或普通 `exec` 连不上（堡垒机）时使用；能用普通 `exec` 时优先用它，不打扰用户的终端。

### 输出上限 1MB

stdout、stderr 各最多返回 1MB，超出部分被截断，JSON 中 `truncated` 为 `true`。查看大文件或长日志时先自行过滤：`tail -n 200`、`grep`、`head`、`wc -l` 等。

### 长时间运行的任务

单次 `exec` 会等待命令结束才返回，不要直接运行耗时很长或不会退出的命令（例如 `tail -f`、前台启动服务）。应交给 `nohup` 或 `tmux` 托管，再分次轮询结果：

```bash
termlet exec web-1 "cd /srv/app && nohup ./build.sh > /tmp/build.log 2>&1 & echo started"
termlet exec web-1 "tail -n 50 /tmp/build.log"

termlet exec web-1 "tmux new-session -d -s build 'cd /srv/app && ./build.sh > /tmp/build.log 2>&1'"
termlet exec web-1 "tmux has-session -t build 2>/dev/null && echo running || echo done"
```

### 危险命令会被拦截

Termlet 内置并允许用户自定义危险命令规则（例如 `rm -rf /`、`mkfs`、`shutdown` 等）。多行脚本（包括 `--stdin`）的每一行，以及 `;`、`&&`、`|`、`if … then`、`do`、`{ … }`、`!`、`case` 分支（`stop) …`）之后的每条命令都会检查，`sudo`、`timeout 5` 等前缀之后也一样。检查按行进行、不解析 shell 语法：多行引号字符串或 heredoc 正文中位于行首的词同样算作命令（例如提交说明的某一行以 `reboot` 开头），引号或注释中 `;`、`&&` 之后的词也一样（例如 `echo 'a; reboot'`），会被误拦；这时同样不要改写命令，向用户说明情况，请用户批准或自己执行。命中规则时：

- 用户开着 Termlet 时，Termlet 会弹窗请用户批准，命令会等待最多 60 秒；批准后正常执行，拒绝或超时返回退出码 **4**（错误码 `command_denied`）。
- Termlet 没有打开窗口、等待期间窗口都被关闭，或用户设置为直接拦截时，命令不会执行，返回退出码 **4**（错误码 `command_blocked`）。
- 等待批准期间 Termlet 被锁定时立即返回退出码 **6**（错误码 `app_locked`），命令不会执行。

`message` 中是命中的规则。

**不要尝试改写、编码或拆分命令来绕过拦截。** 应向用户说明要执行的命令和原因，请用户自己在终端中手动执行。规则可在 Termlet 设置中的「命令行工具」中查看。所有 CLI 调用都会记入审计日志。

### 引号与 `--stdin`

命令作为一个参数传入，整体用引号包住。命令里包含引号、`$`、反引号等，本地 shell 转义变得复杂时，改用 `--stdin` 从标准输入传入命令，避免本地 shell 先展开：

```bash
termlet exec web-1 --stdin <<'EOF'
awk -F: '$3 >= 1000 {print $1}' /etc/passwd
echo "home is $HOME"
EOF
```

注意 heredoc 定界符要加引号（`<<'EOF'`），否则 `$HOME` 会在本地被展开。标准输入最多 1MB，超出时报错（退出码 1）且不会执行；更大的脚本先用 `termlet upload` 传到服务器再执行。

### 文件传输

- V1 只支持**单个文件**。需要传目录时先打包：远程 `tar czf /tmp/x.tar.gz dir` 后下载，或本地打包后上传再远程解压。
- 语义与 scp 相同：上传时远程路径是已存在的目录（或以 `/` 结尾）则放入该目录，否则作为目标文件名；下载时本地路径是 `.` 或已存在的目录则放入该目录，否则作为目标文件名。**已存在的同名文件会被覆盖。**
- 本地路径可以是相对路径，按当前目录解析。

## 常见问题

| 错误码 | 原因与处理 |
|---|---|
| `cli_disabled`（7） | 用户没有开启外部 CLI（默认关闭）。请用户在 Termlet 设置中的「命令行工具」中开启外部 CLI，不要尝试其他方式连接 |
| `app_not_running`（7） | Termlet 没有运行，且用户设置了不自动启动。请用户先打开 Termlet，不要尝试自己启动它或用其他方式连接 |
| `host_not_found`（2） | 主机名写错。运行 `termlet ls` 查看可用主机 |
| `host_not_allowed`（3） | 该主机没有开启"允许 CLI 访问"。请用户在 Termlet 中编辑主机并开启，不要尝试其他方式连接 |
| `command_blocked`（4） | 命中危险命令规则。不要绕过，请用户在终端中手动执行 |
| `hostkey_unknown`（5） | 首次连接该主机需要确认指纹，CLI 无法弹窗。请用户先在 Termlet 桌面端连接一次并确认指纹 |
| `proxy_command_unconfirmed`（5） | 主机经代理命令（ProxyCommand）连接，这条命令还没有在这台电脑上确认过，CLI 不能替用户允许执行本机程序。请用户先在 Termlet 桌面端连接一次该主机并允许 |
| `hostkey_mismatch`（5） | 主机指纹与记录不一致，可能存在中间人攻击。停止操作并告知用户 |
| `auth_interaction_required`（5） | 服务器要求验证码等交互认证，CLI 无法完成。请用户在 Termlet 桌面端中操作 |
| `auth_failed` / `credential_missing`（5） | 凭据错误或缺失。请用户在 Termlet 中编辑主机凭据 |
| `vault_locked`（6） | Termlet 凭据已锁定。请用户打开 Termlet 输入主密码 |
| `terminal_not_open`（5） | 使用了 `--terminal`，但这台主机没有在 Termlet 中打开的终端。请用户先在 Termlet 中打开 |
| `terminal_busy`（5） | 使用了 `--terminal`，但终端正在运行程序。请用户回到 shell 提示符后重试 |

`termlet ls` 返回空列表时，说明用户还没有为任何主机开启 CLI 访问，请用户在 termlet 中开启。
