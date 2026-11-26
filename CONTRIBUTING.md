# 参与 Termlet

**简体中文** · [English](CONTRIBUTING.en.md)

感谢你愿意帮助 Termlet！Termlet 目前由一人维护，精力有限，现阶段**主要接受 issue 与讨论**：问题报告、功能建议、使用中的反馈都很欢迎。想提交代码的话，请**先开 issue 说明想改什么**，确认方向后再动手，免得白费功夫。

参与本项目的交流请遵守[行为准则](CODE_OF_CONDUCT.md)。

## 报告问题

- 在 [Issues](https://github.com/termlet/termlet/issues) 中按“问题报告”模板新建，先搜索有没有重复的。
- 附上版本信息：设置的“关于”页中点“复制版本信息”，粘贴到 issue 里。问题不好说清时，再附上“诊断与崩溃报告”中“复制诊断信息”的内容。
- **不要贴出密码、私钥、令牌、TOTP 密钥**；日志、截图中的主机地址与用户名可以先遮掉。
- **安全漏洞不要公开提 issue**，按 [SECURITY.md](SECURITY.md) 私下报告（GitHub 的私下漏洞报告，或发邮件到 hi@termlet.dev）。

## 功能建议与讨论

- 具体的功能建议按“功能建议”模板开 issue，一个 issue 只提一件事。
- 还不成形的想法、使用上的疑问，到[讨论区](https://github.com/termlet/termlet/discussions)。

## 提交代码

1. 先开 issue（或在已有的 issue 中）说明要改什么、打算怎么改，等维护者回复。
2. fork 后在新分支上修改；一个合并请求只做一件事，附上测试。
3. 提交前在本机跑通下面“检查”中的命令。
4. 提交合并请求，写明对应的 issue。第一次提交时需要签署贡献者许可协议（见下）。

代码注释、文档与提交说明沿用项目的习惯，用简体中文；不方便时用英文也可以，维护者合并时会整理。

### 贡献者许可协议（CLA）

Termlet 以 [AGPL-3.0-or-later](LICENSE) 开源。为了保留以后另行授权（例如商业许可）的余地，贡献需要签署[个人贡献者许可协议](docs/CLA.md)：你保留自己贡献的版权，同时授予维护者使用、修改、再许可（包括以 AGPL 以外的条款）你的贡献的权利，以及相应的专利许可；维护者承诺发布时你的贡献同样按本项目的开源许可提供。协议以英文版为准，中文译文仅供参考。

签署通过 [CLA assistant](https://cla-assistant.io/) 完成：第一次提交合并请求时，它会在合并请求中留言，点其中的链接、用 GitHub 账号登录后接受协议即可；签一次，之后的贡献都适用。没有签署的合并请求不能合并。

## 开发环境

- Rust：经 [rustup](https://rustup.rs) 安装；`rust-toolchain.toml` 固定了版本（1.99.1），第一次构建时自动安装。首次全量编译约 5 分钟。
- Windows：Visual Studio 生成工具（MSVC 与 Windows SDK）。
- Linux：`libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libfontconfig-dev libfreetype-dev`（Debian / Ubuntu 的包名）；也可以用 `testing/linux/Dockerfile` 在 Docker 中构建、测试（用法见其中的注释）。
- macOS：Xcode 命令行工具。macOS、Linux 维护者没有真机，欢迎在这两个平台上帮忙测试。
- 连真实服务器的集成测试需要 Docker；打 Windows 安装包另需 Node.js 与 NSIS 3（makensis）。

## 项目结构

```
crates/termlet-proto/     CLI 与应用之间的协议（请求、响应、错误码、退出码）、Agent Skill；termlet 命令只依赖它
crates/termlet-io/        终端会话的字节接口，核心与终端共用
crates/termlet-core/      核心：数据目录、存储、保险库、SSH、SFTP、转发、Telnet、串口、本地终端、备份、同步……不依赖 GPUI
crates/termlet-terminal/  终端引擎（alacritty_terminal）与 GPUI 终端视图，只认字节传输
crates/termlet-desktop/   桌面应用（Termlet.exe），文案在 locales/*.yml，UI 集成测试在 tests/
crates/termlet-cli/       termlet 命令
packaging/                Windows 安装程序（NSIS 脚本、许可说明）与便携版的标记 portable.txt、macOS 的 Info.plist 与图标集、Linux 的菜单项、图标、install.sh 与 deb / rpm / AppImage 的打包（package.sh、nfpm.yaml、AppRun）、随包分发的许可文本、远程配置的原文，以及生成图标的脚本
testing/sshd/             集成测试用的 OpenSSH 与 Telnet 服务器（Docker 镜像）
testing/x11/ nosftp/ sshcert/   X11 转发、没有 SFTP 时的 SCP、OpenSSH 证书的测试容器（用法见各自 Dockerfile 的注释）
testing/linux/            本机在 Docker 中验证 Linux 版的构建环境（与 CI 相同）
.github/workflows/        GitHub Actions，见下面的“CI”
scripts/                  发布脚本、截取窗口画面与向窗口投递输入（检查界面用）、生成文件图标与品牌标志的脚本
docs/                     用户指南（guide.md）、贡献者许可协议、品牌素材与截图
docs/rust/                Rust 版的技术方案、使用统计方案与繁体中文、日文韩文的用语表
```

## 常用命令

```bash
cargo run                                          # 启动桌面端（调试构建的数据目录是 termlet-dev，不碰正式数据）
TERMLET_DATA_DIR=<临时目录> TERMLET_VAULT_PROVIDER=file TERMLET_UPDATE_URL=off TERMLET_CRASH_URL=off DO_NOT_TRACK=1 cargo run
                                                   # 用临时数据目录试用：凭据用文件保管（不在系统凭据管理器留条目），不检查更新、
                                                   # 不取远程配置、不发使用统计与崩溃报告
cargo test --workspace                             # 全部测试；UI 集成测试在 crates/termlet-desktop/tests/
cargo test -p termlet-core                         # 只测核心（不编译界面）
cargo nextest run --workspace                      # 与 CI 相同的测试运行器（cargo install cargo-nextest --locked）；文档测试另用 cargo test --workspace --doc
cargo test -p termlet-terminal --release throughput -- --ignored --nocapture   # 终端解析吞吐
node scripts/release.mjs conpty --into target/debug    # Windows：把安装包附带的 ConPTY 放到 cargo run 的程序旁边（默认用系统自带的；删掉这两个文件即换回）
                                                   # CI 的 Windows 测试放进 target/debug/deps，并设 TERMLET_TEST_CONPTY=1 核对鼠标上报能传到终端
node packaging/render-icons.mjs                    # 修改 logo 后重新生成图标（需要本机有 Chrome 或 Edge）
```

## 连真实服务器的测试

这些测试由环境变量开启，没有设置时跳过：

| 环境变量 | 测试的对象 |
|---|---|
| `TERMLET_TEST_SSH=127.0.0.1:2222` | 测试容器中的 sshd（见下） |
| `TERMLET_TEST_TELNET=127.0.0.1:2323` | 同一个容器中的 Telnet |
| `TERMLET_TEST_WSL=<发行版>` | WSL |
| `TERMLET_TEST_WEBDAV=<目录地址>` | WebDAV（另设 `TERMLET_TEST_WEBDAV_USER`、`TERMLET_TEST_WEBDAV_PASSWORD`） |
| `TERMLET_TEST_TRASH=1` | 移到回收站（会在回收站里留下测试文件） |

建测试容器并运行：

```bash
docker build -t termlet-e2e-sshd testing/sshd
docker run -d --name termlet-sshd -p 2222:22 -p 2323:23 termlet-e2e-sshd
TERMLET_TEST_SSH=127.0.0.1:2222 cargo test -p termlet-core ssh
TERMLET_TEST_TELNET=127.0.0.1:2323 cargo test -p termlet-core telnet
```

## 检查

合并请求要通过下面这些检查（CI 也会跑）：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings   # 必须无警告
cargo test --workspace                                  # 或 cargo nextest run --workspace
cargo deny check                                        # 依赖的许可、来源与安全公告（见 deny.toml）
gitleaks git .                                          # 不要提交秘密
```

## CI

- 每次推送：在 Linux 上检查格式、clippy、测试、依赖与秘密。
- 推送到 main 与合并请求时另跑：Windows 的全量 clippy 与测试、Linux 上连真实 sshd 的集成测试、arm64 上的核心测试。
- 只改文档时都不跑。
- 测试用 cargo-nextest，本机也可以用（见[常用命令](#常用命令)）。

| 工作流 | 作用 |
|---|---|
| `ci.yml` | 每次推送在 Linux 上检查；推送到 main 与合并请求时另调用下面三个 |
| `windows.yml` | Windows 的 clippy 与测试 |
| `sshd.yml` | 连真实 sshd 的集成测试 |
| `arm64.yml` | arm64 上的核心测试 |
| `release.yml` | 打版本标签时测试、三个平台的发布构建，批准后签名与发布 |
| `remote-config.yml` | 远程配置的签名与发布 |

## 约定

更完整的约定见 [CLAUDE.md](CLAUDE.md)（也是给 AI 编程助手看的项目说明），要点：

- 依赖只用宽松许可（MIT、Apache-2.0、BSD 等），`cargo deny check` 检查；**不复制 GPL 等 copyleft 项目的代码**（例如 ShellRS、OxideTerm、Zed 的 terminal_view，只参考思路）。
- 界面基于 gpui-kit 0.8.1：写界面前读 `.claude/skills/gpui-kit/` 与 `.claude/skills/gpui-kit-design-guides/`，API 以 crates.io 上的源码为准；颜色、尺寸用主题与 rem，不写死。
- 文案在 `crates/termlet-desktop/locales/*.yml`，简体中文是源语言，同一个键的英文、繁體中文、日文、韩文一起改（繁体按 [`docs/rust/繁体中文用语.md`](docs/rust/繁体中文用语.md)，日文、韩文按 [`docs/rust/日文韩文用语.md`](docs/rust/日文韩文用语.md)）。
- 秘密不进日志、不进 `Debug` 输出、不以明文进数据库。
- 用户能感受到的改动（新增、改进、修复、安全），在 [CHANGELOG.md](CHANGELOG.md) 与 [CHANGELOG.en.md](CHANGELOG.en.md) 最上面标着“未发布”的一节各写一条；发版时维护者把这一节改成版本号与日期，GitHub Release 与官网的更新日志都取自这里。
- 功能的用法写在[用户指南](docs/guide.md)（中英文 `docs/guide.md`、`docs/guide.en.md` 同步改）；README 只放概览。
- 设计见[技术方案](docs/rust/技术方案.md)。

### 新功能与新选项

Termlet 的宗旨是“功能都在，用时随手可得，不用时绝不打扰——让你沉浸在终端里”，因此刻意保持小而清楚，加东西前请先开 issue 讨论：

- 说明谁需要、和现有的功能有没有重叠、能不能做成现有功能的一个选项；“别的软件有”不足以作为理由。
- 能用合适的默认值解决的，不加选项；新功能默认关闭或用到时才出现，不在启动时、后台常驻做事。
- 界面有数量上限（设置页的控件数、说明的长度、右键菜单的项数、默认显示的工具栏按钮），`cargo test` 中的 `ui_limits_tests` 会检查；超了请合并、折叠或删减，不要改大上限。
- 沿用界面上已有的叫法，不造同义词。
- 新增依赖写明理由，锁文件中只多出新加的包。

完整的规矩见 [CLAUDE.md](CLAUDE.md) 的“防臃肿”。

## 发布（维护者）

构建、签名、发布都在 GitHub Actions 上，见 `.github/workflows/release.yml` 开头的说明。

### 发布的三步

1. **准备**：`node scripts/release.mjs prepare <版本>` 改版本号，并把 `CHANGELOG.md` / `CHANGELOG.en.md` 最上面“未发布”的一节改成版本号与日期；核对后提交、推送。
   - 使用统计的接收端（`api.termlet.dev`）按白名单校验各项的取值。
   - 客户端要发新的取值（如界面语言的 `ja`、`ko`）时，先改好接收端并部署，再发布客户端，否则新客户端的统计会被整条拒收。
2. **推送标签 `v<版本>`**：`release.yml` 先核对标签、版本号与更新日志，跑 Windows 的全量测试与连 sshd 的集成测试。都通过后打未签名的包：
   - Windows x64、arm64 的安装包与便携版 zip（同一架构的两个包用同一个暂存目录）；
   - Linux x64、arm64 的 tar.gz、deb、rpm、AppImage（`packaging/linux/package.sh`；deb、rpm 用 nfpm，AppImage 用 appimagetool，工具按架构固定版本并核对校验和）；
   - macOS 的 Termlet.app（zip，ad-hoc 签名）；
   - 各带版本描述文件（`latest.yml`、`latest-arm64.yml`、`latest-linux.yml`、`latest-linux-arm64.yml`、`latest-mac.yml`），作为构建产物保留 14 天。

   这些步骤不接触任何秘密。arm64 的打包先在 arm64 运行器上跑核心测试；它是实验版，失败不挡 x64 与 macOS 的发布，这一版不出那个 arm64 的清单与包（下载站上原有的不动，arm64 的客户端停在旧版本）。
3. **批准**：`release` 环境要维护者在网页或手机上批准，之后签名与发布的 job 才拿到 Secrets 开始运行：
   - 先用构建 job 报告的摘要核对产物（这之前不碰任何秘密）；
   - 用在用的密钥给更新清单与 `SHA256SUMS` 签名，生成构建来源证明；
   - 传到下载站 `dl.termlet.dev`（先传各平台的包、最后传 `latest*.yml`），清 CDN 缓存后经 https 取回核对；
   - 在本仓库建 GitHub Release（说明取更新日志中这一版的中英文两节）；
   - 最后让官网重建（下载页与更新日志换成新版本）。

   中途失败时在网页上重新运行这个 job。

### 远程配置

- 远程配置是 `dl.termlet.dev/update.json`，原文在 `packaging/remote-config/update.json`。
- 改好提交后，或者签名快到期时，手动运行 Remote config 工作流（同样在 `release` 环境、要批准）。

### 在本机发布（后备）

- CI 不可用、或者轮换密钥要用离线备用的密钥签名时，可以在本机 `fetch`、`sign --key`、`publish` 发布。
- 默认用登录的 Cloudflare 命令行 `cf` 上传，见 `scripts/release.mjs` 开头的说明。

### release.mjs 的命令

```bash
node scripts/release.mjs prepare <版本>             # 发版前的准备：改 Cargo.toml（与 Cargo.lock）的版本号，把更新日志最上面“未发布”的一节改成版本号与今天的日期（--dry-run 先看一遍）
node scripts/release.mjs notes <版本>               # 核对并打印这一版的 GitHub Release 说明（更新日志中这一版的中英文两节）
node scripts/release.mjs build [--arch x64|arm64]  # 在本机发布构建并打安装包 termlet-<版本>-win-x64.exe、便携版 termlet-<版本>-win-x64-portable.zip 与 latest.yml（依次列出这两个），输出到 target/dist；arm64 是 -win-arm64 的两个包与 latest-arm64.yml（暂存目录 stage-arm64；与本机工具链的架构不同时交叉编译，要装好 Rust 目标与 MSVC 的 ARM64 生成工具）；程序旁附带的 ConPTY 按 release.mjs 的 CONPTY 固定的版本从 nuget.org 下载并核对摘要
node scripts/release.mjs conpty [--arch x64|arm64] --into <目录>   # 单独取附带的 ConPTY（conpty.dll、OpenConsole.exe），下载的包缓存在 target/conpty
node scripts/release.mjs manifest --file <包> [--file <包>]...   # 给 Linux、macOS 的包写版本描述文件 latest-linux.yml / latest-linux-arm64.yml（依次列出 tar.gz、deb、rpm、AppImage）、latest-mac.yml（CI 打包后调用）
node scripts/release.mjs fetch --tag v<版本>        # 本机发布（后备）：下载 CI 为这个标签构建的产物（各平台、各架构的包与清单）到 target/dist/v<版本>（要先 gh auth login）
node scripts/release.mjs sign --key <密钥文件> [--dir <目录>]   # 给 latest*.yml 签名并生成 SHA256SUMS
node scripts/release.mjs publish [--dir <目录>] [--dry-run]   # 发布签过名的文件：传到下载站 dl.termlet.dev（R2）、清 CDN 缓存并取回核对，在本仓库建这个版本的 GitHub Release，调用部署钩子让官网重建
node scripts/release.mjs sign-config --key <密钥文件> [--days 90] <update.json>   # 给远程配置签名，带有效期（平时由 remote-config.yml 做）
node scripts/release.mjs publish-config [--dry-run] <update.json>   # 把签过名的远程配置传到下载站
node scripts/release.mjs newkey --out <仓库外的目录>   # 轮换更新签名的密钥（步骤见 crates/termlet-core/src/update/keys.rs）
node scripts/release.mjs licenses                  # 单独写出 Rust 依赖的许可文本（打包时自动生成，放进安装目录的 licenses）
```

## 文档

- [用户指南](docs/guide.md)：安装与各项功能的用法
- [技术方案](docs/rust/技术方案.md)：架构与设计取舍
- [使用统计方案](docs/rust/使用统计方案.md)
- [繁体中文用语](docs/rust/繁体中文用语.md)、[日文韩文用语](docs/rust/日文韩文用语.md)
- [品牌素材](docs/brand/README.md)

## 许可

你提交的贡献按[贡献者许可协议](docs/CLA.md)授权；Termlet 整体以 [AGPL-3.0-or-later](LICENSE) 发布。“Termlet”名称与标志的使用见 [TRADEMARK.md](TRADEMARK.md)。
