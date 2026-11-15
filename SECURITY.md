# 安全策略 / Security Policy

[简体中文](#简体中文) · [English](#english)

## 简体中文

### 报告漏洞

请不要在 issue、讨论区或其他公开的地方报告安全漏洞。请用 GitHub 的私下漏洞报告：在本仓库的 **Security** 页点 **Report a vulnerability**，只有你和维护者能看到报告的内容。

没有 GitHub 账号或不方便用它时，也可以发邮件到 hi@termlet.dev，标题写明“安全漏洞”。邮件在传输中不保证加密，细节较多时请优先用 GitHub 的私下漏洞报告。

请尽量写明：

- 受影响的 Termlet 版本与系统；
- 问题出在哪里（例如 SSH 连接、SFTP、命令行工具 `termlet`、更新、同步、备份文件）；
- 复现步骤或概念验证，以及你认为的影响；
- 是否已经在别处公开。

报告中不要附带真实的密码、私钥或服务器地址；需要样例时请用测试用的密钥与服务器。

### 支持的版本

只为最新发布的版本提供安全修复，修复随新版本发布、经自动更新推送。用旧版本发现的问题，请先确认最新版本中是否仍然存在。

| 版本 | 安全修复 |
|---|---|
| 最新发布的版本 | 提供 |
| 更早的版本 | 不提供，请升级 |

### 处理流程

1. 确认收到：Termlet 由一人维护，通常在 7 天内回复。
2. 核实与评估：复现问题、评估影响，在这份私下报告（或邮件）中与你沟通进展。
3. 修复与发布：修复后发布新版本，并发布安全公告，写明受影响的版本与修复的版本；你愿意的话，在公告中致谢。
4. 公开：请在修复发布之前不要公开细节。90 天内无法修复时，会与你商量公开的时间。

### 不算安全漏洞的

下面这些是已经写明的设计边界，绕过它们的写法请提普通 issue：

- 危险命令规则只用于防误操作，可被绕过，不是安全边界；
- 应用锁用于防止离开时别人使用这台已登录的电脑，不防御在本机以你的身份运行的程序。

### 发布文件的签名

每个版本的版本描述文件 `latest.yml` 与校验和文件 `SHA256SUMS` 都有签名（[minisign](https://jedisct1.github.io/minisign/) 格式，分别是 `latest.yml.minisig` 与 `SHA256SUMS.minisig`），其中写着安装包的哈希。自动更新用 Termlet 内置的下面两把公钥校验；手动核对的方法见 README 的[核对下载的文件](README.md#核对下载的文件)。

| 用途 | 公钥 | 密钥 ID |
|---|---|---|
| 在用（发布时用它签名） | `RWRSYxplUItTcPz4ZAEBJVcK0n3Sy1xPjhO3CNJIVFxbazT7cXsPacBG` | `70538B50651A6352` |
| 离线备用（只在轮换密钥时使用） | `RWR0ioBzciN0EjIEaOwb2jw2Xujxz3oxGYgF8nh1HrPVLkgkqH9dsmog` | `1274237273808A74` |

SHA-256 一致只说明文件下载完整；签名通过才说明文件来自 Termlet 的发布者。私钥不进任何仓库：在用的那把加密后存在本仓库 GitHub Actions 的 `release` 环境中，只有维护者批准的发布任务才能使用；离线备用的那把由发布者离线保管，不进 CI。以后轮换密钥（例如在用的密钥丢失或泄露）时，会在这里与[更新日志](CHANGELOG.md)中公告。见到自称来自 Termlet、却不能用这里的公钥核对通过的文件，请按上面的方式私下报告。

每个版本的包都由本仓库的 GitHub Actions 从版本标签构建，并附有 GitHub 的构建来源证明，可以用 `gh attestation verify <文件> -R termlet/termlet` 核对（0.3.0 起）。

## English

### Reporting a vulnerability

Please do not report security vulnerabilities in issues, Discussions or anywhere else in public. Use GitHub's private vulnerability reporting instead: on this repository's **Security** tab, click **Report a vulnerability**. Only you and the maintainer can see the report.

If you don't have a GitHub account or would rather not use it, you can also email hi@termlet.dev with "Security vulnerability" in the subject. Email is not guaranteed to be encrypted in transit, so for detailed reports please prefer GitHub's private vulnerability reporting.

If you can, include:

- the affected Termlet version and operating system;
- where the problem is (for example SSH connections, SFTP, the `termlet` command-line tool, updates, sync or backup files);
- steps to reproduce or a proof of concept, and the impact you expect;
- whether it has been disclosed anywhere else.

Do not include real passwords, private keys or server addresses in the report; use test keys and servers for samples.

### Supported versions

Security fixes are made for the latest release only; they ship in a new version, delivered through automatic updates. If you found a problem in an older version, please check whether it still happens in the latest one.

| Version | Security fixes |
|---|---|
| Latest release | Yes |
| Earlier versions | No, please upgrade |

### Process

1. Acknowledgement: Termlet is maintained by one person; you will usually get a reply within 7 days.
2. Assessment: the problem is reproduced and its impact assessed, with progress shared with you in the private report (or by email).
3. Fix and release: a new version with the fix is released along with a security advisory listing the affected and fixed versions; you are credited in the advisory if you wish.
4. Disclosure: please keep the details private until the fix is released. If it cannot be fixed within 90 days, we will agree on a disclosure date with you.

### Not considered vulnerabilities

These are documented design limits; please open a regular issue for ways around them:

- the dangerous command rules only prevent mistakes; they can be bypassed and are not a security boundary;
- the app lock keeps others from using this logged-in computer while you are away; it does not defend against programs running on this computer as you.

### Release signing keys

Each release's description file `latest.yml` and checksum file `SHA256SUMS` are signed ([minisign](https://jedisct1.github.io/minisign/) format, as `latest.yml.minisig` and `SHA256SUMS.minisig`), and they contain the installer's hash. Automatic updates are verified with the two public keys below, which are built into Termlet; to check a download by hand, see [Checking your download](README.en.md#checking-your-download) in the README.

| Use | Public key | Key ID |
|---|---|---|
| Current (signs releases) | `RWRSYxplUItTcPz4ZAEBJVcK0n3Sy1xPjhO3CNJIVFxbazT7cXsPacBG` | `70538B50651A6352` |
| Offline backup (only used when rotating keys) | `RWR0ioBzciN0EjIEaOwb2jw2Xujxz3oxGYgF8nh1HrPVLkgkqH9dsmog` | `1274237273808A74` |

A matching SHA-256 only shows the download is complete; a valid signature is what shows the file comes from Termlet's publisher. The private keys never go into any repository: the current one is stored encrypted in the `release` environment of this repository's GitHub Actions, usable only by release jobs the maintainer approves; the offline backup is kept offline by the publisher and never goes into CI. If the keys are ever rotated (for example because the current key was lost or leaked), it will be announced here and in the [changelog](CHANGELOG.en.md). If you come across a file claiming to be from Termlet that does not verify with these keys, please report it privately as described above.

Every release's packages are built from the version tag by this repository's GitHub Actions and come with GitHub build provenance attestations, which you can check with `gh attestation verify <file> -R termlet/termlet` (from 0.3.0 on).
