# CLAUDE.md

Termlet 用 Rust + GPUI（gpui-kit 0.8.1）编写，以 AGPL-3.0-or-later 开源（`LICENSE`；贡献流程见 `CONTRIBUTING.md`，贡献者签 `docs/CLA.md`，名称与标志的使用见 `TRADEMARK.md`）。设计见 `docs/rust/技术方案.md`。维护者的内部文档（开发计划 `docs/rust/开发计划.md`、PRD、竞品分析，旧版 Electron + Go 时期的 `docs/legacy/` 与旧代码的标签 `backup/electron-go-final`）只在维护者的私有仓库中，公开仓库没有：工作区里有时可以参照，没有时不依赖它们，以代码、本文件与技术方案为准。

## 常用命令

```bash
cargo run                                          # 启动桌面端（调试构建的数据目录是 termlet-dev）
TERMLET_DATA_DIR=<临时目录> TERMLET_VAULT_PROVIDER=file TERMLET_UPDATE_URL=off TERMLET_CRASH_URL=off DO_NOT_TRACK=1 cargo run
                                                   # 截图、实测用临时数据目录：文件保管（不在系统凭据管理器留条目），从第一次启动起不检查更新、
                                                   # 不取远程配置、不发使用统计与崩溃报告（这些设置在数据库里，第一次启动前写不进去）；
                                                   # 加 TERMLET_TEXT_SCALE=125 模拟系统的文字大小（界面大小跟随系统时），不用改系统设置
cargo test --workspace                             # 全部测试；UI 集成测试在 crates/termlet-desktop/tests/
cargo test -p termlet-core                         # 只测核心（不编译界面）
cargo nextest run --workspace                      # 与 CI 相同的测试运行器（每个测试一个进程；cargo install cargo-nextest --locked）；文档测试另用 cargo test --workspace --doc
TERMLET_TEST_SSH=127.0.0.1:2222 cargo test -p termlet-core ssh   # 连真实 sshd 的集成测试（先开 Docker Desktop，docker start termlet-sshd；镜像见 testing/sshd）
TERMLET_TEST_TELNET=127.0.0.1:2323 cargo test -p termlet-core telnet   # 同一个容器里的 busybox telnetd（含 Telnet 上的 ZModem）
TERMLET_TEST_PROXY_COMMAND="docker exec -i termlet-sshd nc 127.0.0.1 22" cargo nextest run -p termlet-core -E 'test(/proxy_command/)'
                                                   # 经 ProxyCommand 连 termlet-sshd；X11 转发另用 testing/x11 的临时容器（TERMLET_TEST_X11，用法见其 Dockerfile 注释）
                                                   # 没有 SFTP 时的 SCP 模式与服务器禁止转发：testing/nosftp 的临时容器（TERMLET_TEST_NOSFTP，用法同上）
TERMLET_TEST_CERT=127.0.0.1:2227 cargo nextest run -p termlet-core -E 'test(/cert/)'
                                                   # OpenSSH 证书登录与主机证书：testing/sshcert 的临时容器 termlet-cert-sshd（信任用户 CA、出示主机证书，
                                                   # 测试经 docker exec 在容器里现场签发证书；建、删容器见其 Dockerfile 注释）；不设时只跑进程内的证书测试
cargo clippy --workspace --all-targets -- -D warnings   # 必须无警告
cargo fmt --all --check
cargo deny check                                   # 依赖许可（只允许宽松协议）、来源与安全公告（见 deny.toml）
gitleaks git .                                     # 扫描提交历史中的秘密（确认过的误报记在 .gitleaksignore，写明理由）
docker build -t termlet-linux-build:1.99.1 testing/linux   # 本机在 Docker 中验证 Linux（编译、clippy、测试，与 CI 相同）；起容器、同步源码、运行的命令见 Dockerfile 的注释
                                                   # Git Bash 中先 export MSYS_NO_PATHCONV=1，否则 /src、/target 等参数会被改写成 Windows 路径
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/capture-window.ps1 -Process termlet-desktop -Out shot.png
                                                   # 截取窗口画面检查界面（锁屏时也能用）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/post-input.ps1 -Process termlet-desktop -Click 640,451
                                                   # 向窗口投递点击（-Click / -DoubleClick / -RightClick / -Drag x1,y1,x2,y2）、只移动指针（-Move，看悬停提示）、
                                                   # 文字（-Text）、按键（-Key Enter）、滚轮（-Wheel x,y,格数，负数向下），改窗口大小（-Resize 宽,高，用窗口管理接口）
                                                   # 带修饰键的快捷键投递不了，用 UI 测试；截图坐标换算成客户区坐标：x 减 8
                                                   # 两个脚本按进程名找时取第一个有窗口的：用户开着自己的 Termlet、或同时开着几个测试实例时，改用 -Pid <进程号>
                                                   # （代替 -Process），免得截错、投错窗口；Git Bash 中直接 `程序 &` 启动的，进程号是 `cat /proc/$!/winpid`
cargo test -p termlet-terminal --release throughput -- --ignored --nocapture   # 终端解析吞吐
TERMLET_TEST_SSH=127.0.0.1:2222 cargo test -p termlet-core --release --lib sftp::tests::bench -- --ignored --nocapture --test-threads 1
                                                   # SFTP / SCP / 命令行传输的吞吐（同一文件连续传 5 次，打印每次的 MB/s；TERMLET_BENCH_MB 等见 bench.rs）
node scripts/release.mjs conpty --into target/debug/deps && TERMLET_TEST_CONPTY=1 cargo test -p termlet-core --lib localpty
                                                   # Windows 安装包附带的 ConPTY（版本与摘要固定在 release.mjs 的 CONPTY）：放到测试程序旁边，核对鼠标上报能传到终端；
                                                   # 放进 target/debug 则 cargo run 也用它。测完删掉这两个文件，否则之后的本机测试都经它（CI 的 Windows 测试都经它）
python scripts/file-icons.py                       # 重新生成 SFTP 的彩色文件图标（Catppuccin vscode-icons，版本与校验和写在脚本里；生成的表与 mocha.txt 不要手改）
python scripts/brand-icons.py                      # 重新生成系统与品牌标志（Simple Icons，版本与校验和、收录名单写在脚本里；生成的 SVG、LICENSE 与 brand_icons/table.rs 不要手改）
```

首次全量编译约 5 分钟。不要随手 `cargo update`：GPUI 一族互相精确固定版本（gpui-kit 0.8.1 固定 gpui-pre 0.3.10；可选的 GPUI Fast 后端 gpui-fast 0.1.12 没有开，但锁文件按全部特性解析，仍列在其中，它又精确依赖 taffy 0.15.0、wgpu 30.0.1、unicode-properties 0.1.3、objc2 0.3.2 一系等），gpui-base 精确依赖 ropey 2.0.0-beta.1。升级 gpui-kit 时改根 `Cargo.toml` 中的版本，再 `cargo update -p gpui-kit --precise <版本>`，逐项核对锁文件的变化。

## 结构

- `crates/termlet-proto`：CLI 与应用之间的协议；`termlet` 命令只依赖它。
- `crates/termlet-io`：终端会话的字节接口，核心与终端共用（核心因此不依赖 GPUI）。
- `crates/termlet-core`：核心，不依赖 GPUI。
- `crates/termlet-terminal`：终端引擎与 GPUI 终端视图，只认字节传输，不依赖 core。
- `crates/termlet-desktop`：桌面应用（`lib.rs` + `main.rs`，UI 测试驱动生产环境的视图）。
- `crates/termlet-cli`：`termlet` 命令。

## 约定

- 写界面前读 `.claude/skills/gpui-kit/`（Coding Guides）与 `.claude/skills/gpui-kit-design-guides/`；不凭记忆臆造 gpui-kit API，以 `$CARGO_HOME/registry/src/*/gpui-component-0.8.1/`、`gpui-base-0.8.1/`、`gpui-pre-0.3.10/` 的源码为准。
- 快捷键按系统给出数据表，不用 `#[cfg(target_os = "macos")]` 分开写：各模块的 `key_bindings(os: OperatingSystem)` 返回 `KeyBinding` 列表，`init` 按 `cx.platform_info().os` 注册；可自定义的动作与默认值在核心的 `keymap.rs`（Windows 与 Linux 一套，macOS 一套；改过的也分开存：macOS 用 `mac_bindings`）。macOS 上应用的快捷键都用 ⌘，Ctrl 组合留给终端。固定的用 `KeyBinding::new` 写明 `ctrl-` / `cmd-`（gpui-pre 0.3.10 起它不认 `secondary-`，注册时 panic）；macOS 上 Shift 加数字、符号键上报的是打出来的字符且不带 Shift，写成 `cmd-}`（⇧⌘]）、`cmd-+`，写 `cmd-shift-]` 永远触发不了。用户设置的、动态拼出的组合用 `KeyBinding::load(…, cx)`；解析按键用 `Keystroke::parse_with_os(…, os)`。桌面端 `keymap.rs` 的测试按三个系统注册全部快捷键、逐条解析并查冲突（CI 不跑 macOS 的测试，macOS 那张表靠它在 Windows 上测）。
- 对话框（`open_dialog` 的 `child`）的内容区是 gpui-kit 的滚动区域：内容比窗口高时，gpui-kit 0.8 起滚轮只滚对话框本身，里面普通的 `overflow_y_scroll()` 区域滚不动。对话框内自己带滚动区域的，高度按窗口收（如设置、主机对话框的 `min(…, 窗口高 - 8rem)`），或者内层用 `overflow_y_scrollbar()`（指针下最里层的 gpui-kit 滚动区域独占滚轮）。对话框的宽度按 rem 给（`rems(…).to_pixels(window.rem_size())`），gpui-kit 的默认宽度写死像素、不随界面大小；确认框、提示框用 `crate::ui_scale::open_alert` 打开（`clippy.toml` 不许直接调 `open_alert_dialog`）。
- 滚动条一直显示（放不下时才画），所以：滚动区用 `crate::scrolling`（`overflow_y_scrollbar()`、要程序滚动的用 `tracked_y` / `tracked_x`），右端有按钮或右对齐数字的内容加 `scrollbar_gutter()` 让出轨道（轨道宽 16px、会接住点击）；下拉用 `crate::choice::select`，长菜单经 `crate::menu_fit`，二者都按窗口收高度。`clippy.toml` 禁了裸的 `overflow_*_scroll` 与 `Select::new`。
- 界面代码不写死颜色与 `px(...)`（窗口尺寸等物理边界除外），用 `cx.theme()` 与 rem 尺度；重复元素的 `ElementId` 用领域 ID，不用下标。
- 设置（`settings/`）新加的设置项要在 `settings/search.rs` 的 `ITEMS` 登记（分类、所在的节或页签、锚点、标题与说明的文案键），并在渲染处用 `Hits::mark` 标上锚点，否则设置搜索找不到它；测试 `every_item_is_drawn_where_it_says` 核对登记与界面一致。
- 会换行的中文句子用 `crate::prose::prose(...)` 放进界面（`div().child(prose(t!("…")))`、对话框与表单字段的说明同样），不直接放字符串：GPUI 的换行没有避头尾规则，会把“。”等标点甩到行首。
- 标题栏（`TitleBar`，在 `workspace/title_bar.rs`）整条是拖动窗口的区域：其中的按钮要放进 `workspace.rs` 的 `no_drag()`（`occlude`）容器，否则 Windows 上真实的鼠标点击会被当成拖动窗口。右边的按钮是可自定义的工具栏：新按钮加进 `toolbar.rs` 的 `ToolbarItem`（ID 发布后不能改），放不下时收进“更多”菜单。`scripts/post-input.ps1` 投递的点击绕过 WM_NCHITTEST，验证不了这一点；不要用真实的鼠标键盘输入测试（用户在用这台电脑）。
- 文案在 `crates/termlet-desktop/locales/*.yml`（rust-i18n `_version: 2`，键的全名在最外层，下面 `zh-CN`、`en`、`zh-TW`、`ja`、`ko` 五种语言并排），代码用 `crate::t!`；中文（简体）是源语言，改中文时同一个键的其余语言一起改：繁体用台湾用语，按 `docs/rust/繁体中文用语.md`（引号用「」，人称照原文用“你”，指标签页的 Tab 译作“分頁”），日文、韩文按 `docs/rust/日文韩文用语.md`（日文です・ます体、引号用「」，韩文합니다체、半角标点，都不写人称）；键说用途，一句话一个键，不拼片段。`i18n.rs` 的 `locales_are_complete` 检查各语言的键、插值一致。gpui-kit 自己的文案没有日文、韩文，在 `locales/gpui_kit.yml` 中补（键以 `gpui_component.` 开头）。命令行工具 `termlet` 只有简体中文与英文。
- 注释、文档、提交说明用简体中文；标识符用英文。
- 提交说明要简洁：第一行一句话说清做了什么，约 50 字以内；确有必要时空一行，再写几行要点（行为的变化、不兼容之处、取舍的理由、改大上限的理由）。实现细节、测试清单、给用户看的完整说明分别写进代码注释、测试与更新日志，不在提交说明里复述。
- 秘密不进日志、不进 `Debug` 输出、不以明文进数据库。
- ShellRS、OxideTerm、Zed 的 terminal_view 是 GPL：只看思路，不复制代码。贡献按 CLA 可由维护者另行授权，代码只能是自己写的或宽松许可的；依赖同样只用宽松许可（`deny.toml`）。
- 终端内核（现在是 alacritty_terminal）只经 `termlet-terminal` 的 `engine/kernel.rs`（`trait Kernel`）使用，它的类型只出现在 `engine/alacritty.rs` 与 `engine/alacritty/` 下；视图、查找、高亮、shell 集成、按键与鼠标编码只认 `grid.rs` 等引擎自己的类型，绘制读 `TerminalEngine::snapshot` 的快照（为以后换 libghostty-vt 留的接口）。内核只在引擎线程（`engine/worker.rs`）上创建与使用，不需要 `Send`（测试构建中特意是 `!Send`）：界面要读内核时加命令或 `TerminalEngine::call` 的闭包，不要让内核跨线程。
- 含反斜杠、引号嵌套的文件内容用 Write / Edit 工具写，不用 heredoc；批量修改时把 Python 脚本写成文件再执行（Python 普通字符串里的 `\x00` 会变成真的控制字符，用原始字符串或 `chr(92)`）。
- UI 测试（`tests/ui.rs`）按文案查找、输入时先 `i18n::set_language(Language::ZhCn)`：那里界面语言是进程全局的，测试之间会互相影响，所以其中不切换到别的语言（会让并行的测试按中文找不到要点的项）；切换语言的测试写成桌面端的单元测试（如 `sidebar/mod.rs` 中的）。桌面端的单元测试中 Termlet 的文案按本线程设定的语言（默认简体中文），切换不影响并行的其他测试；gpui-kit 自己的文案仍是进程全局的，不要按它们查找。
- UI 测试中等网络（SFTP 等在核心运行时上的操作）用 `tests/ui.rs` 的 `wait_until` 轮询，`run_until_parked` 不等后台线程；右键菜单、对话框在下一帧才建好，点击前先 `run_until_parked`。
- UI 测试中用到真实的伪终端或终端解析线程时，先调用 `cx.executor().allow_parking()`，否则会被测试调度器判为不确定。
- 测试容器（`testing/sshd`，镜像 `termlet-e2e-sshd`，端口 2222 / 2323）：改了 Dockerfile 要重建镜像并重新创建容器（`docker build -t termlet-e2e-sshd testing/sshd`，再删掉旧的 termlet-sshd 容器按相同端口重建）。其中有 SFTP 以 root 打开的测试用户 sudo-nopw、sudo-pw、sudo-srv、sudo-deny（密码与 termlet 相同）；termlet 自己没有 sudo（主机工具的测试依赖这一点）。没有重建过的旧镜像不带 `MaxStartups`，全量并行跑 SSH 测试时偶发 `Connect(Disconnected)`，加 `-- --test-threads=4`。
- 并行开发核心的新模块时，各自放在 `wip-<模块>` 特性开关后（在 `crates/termlet-core/Cargo.toml` 的 `[features]` 中加），用各自的 `CARGO_TARGET_DIR` 编译，完成后去掉开关。

### 防臃肿

产品的宗旨：**功能都在，用时随手可得，不用时绝不打扰——让你沉浸在终端里**（英文：Everything you need, at hand when you want it and out of your way when you don't, so you can stay immersed in the terminal）。下面的规矩都从这句话出发：新功能默认不启动、新按钮默认隐藏、菜单与设置项有上限，是为了“不用时不打扰”；入口放进命令面板、现有菜单，是为了“用时找得到”。拿不准要不要加、加在哪时，先拿这句话衡量。

加功能、选项、按钮、菜单项之前先过这一节。能量化的由 `crates/termlet-desktop/src/ui_limits_tests.rs` 守住：已经超过目标的，上限先定在现状，只降不升；超了先合并、折叠或删掉，不要改大上限，确实要加时在提交说明中写明理由。

- **先回答这几问**（写在方案或合并请求的开头）：谁要、几个人独立提过；和现有的哪项重叠，能不能做成它的一个选项、一条预设；要不要新名词；入口能不能放进已有的位置（命令面板、现有菜单、现有设置页的一节），新的工具栏按钮、侧边栏页、设置页、面板要单独说明理由；不做任何设置能不能用；维护成本（五种语言、两种 Tab 布局、三个平台、要追的外部格式、新的测试环境）；以后怎么拿掉。“别的软件有”只是候选，不是理由。
- **默认值优先**：新增选项前先问能不能用一个合适的默认值解决；只有少数人要的，先做成预设或放进命令面板。
- **默认不启动、用到才加载**：新功能默认关闭或按需出现（像主机工具、运行中的转发按钮那样）；用到时才启动，不常驻线程与定时器，不给启动加事。
- **上限**（窗口 1280×800、界面 100%）：
  - 设置页默认可见的控件（复选框、开关、下拉框、输入框，单选组算一个）不超过 12 个，超过就分小节、低频项折叠；一页不超过 1.5 屏，列表页要有搜索或筛选；分类不超过 12 个。
  - 说明一条一行：zh-CN 去掉空白不超过 40 字，只写作用范围、不可逆的后果、前提与怎样恢复；实现细节（间隔、转义序列、退出码）写进用户指南。警告色只给要用户处理的风险。
  - 右键菜单不超过 12 项（硬上限 16）、5 组，低频项收进子菜单（只一层，每个子菜单至少 3 项）。
  - 一行纯图标按钮不超过 7 个；标题栏工具栏默认显示的按钮不再增加，新按钮默认隐藏。
  - 对话框不超过 5 页，每页至少 3 个控件；表单的标签在字段上方，占位文字只放示例或状态，不代替标签。
- **用词对齐**：新概念、新叫法先对照界面上已有的用语（locales 与用户指南），同一个东西各处同一个叫法，不造同义词；改名时文案、用户指南、更新日志一起改。
- **依赖**：加依赖要写明理由（为什么不用已有的依赖或自己写），只用宽松许可；锁文件里只应多出它和它的依赖，不顺带升级别的包（不跑 `cargo update`，逐项核对锁文件的变化）。
- **退出**：长期没人用、已被更通用的功能覆盖、要追的外部格式失效的，逐级拿掉：入口降级（只留命令面板）→ 冻结（只修严重问题）→ 更新日志预告 → 删除（数据照常保留，同步与备份不丢字段）。
