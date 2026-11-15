// 发布：发布构建、打 Windows 安装包、生成版本描述文件、更新签名（minisign 格式）与发布到下载站和 GitHub Releases。
// 作者：ruan
//
// 发布流程（2026-11-11 起构建、签名、发布都在公开仓库 termlet/termlet 的 GitHub Actions 上，见 .github/workflows/release.yml）：
//   1. 准备：node scripts/release.mjs prepare <版本>  改 Cargo.toml 的版本号（连同 Cargo.lock 中本工作区的包），把两份更新日志
//      CHANGELOG.md / CHANGELOG.en.md 最上面“未发布”的一节改成这个版本与当天的日期；核对 git diff 后提交、推送。
//   2. 推送标签 v<版本>：release.yml 先核对标签、版本号与更新日志（notes），跑 Windows 的 clippy 与测试、连 sshd 的测试，
//      通过后打 Windows x64、arm64 的安装包与便携版 zip（未签名）、Linux x64、arm64 的 tar.gz、deb、rpm、AppImage 与
//      macOS 的 .app（zip），各平台、各架构带版本描述文件。这些 job 不接触任何秘密。
//   3. 用户在网页或手机上批准 release 环境的部署后，签名与发布的 job 才运行（Secrets 只在这个环境中）：只下载上一步的产物，
//      collect 先用构建 job 报告的摘要核对、再合并核对内容（这之前不碰任何秘密），sign 签更新清单与 SHA256SUMS（私钥由
//      这一步写进 $RUNNER_TEMP 下权限 600 的临时文件、用完即删，口令从环境变量读），生成构建来源证明，publish 依次传到
//      下载站 dl.termlet.dev（R2）、清 CDN 缓存、取回核对，在 GitHub 上建这个版本的 Release（说明取更新日志中这一版的
//      中英文两节），最后调用 Cloudflare Pages 的部署钩子让官网重建（下载页与更新日志换成新版本）。
//   远程配置 update.json（packaging/remote-config/update.json）由手动运行的 remote-config.yml 签名（sign-config）并发布
//   （publish-config），同样在 release 环境、要批准（环境的部署规则是 v* 标签加 main 分支）。
// 本脚本只用 Node 自带的模块，不依赖任何第三方包；CI 的签名与发布 job 不运行 npm / pnpm install 与 cargo。
//
// 本机发布（后备：CI 的签名或发布出了问题，或者轮换密钥时要用离线备用的密钥签名）：
//   node scripts/release.mjs fetch --tag v<版本>                  把 release.yml 这次运行的产物下载到 target/dist/v<版本>
//   node scripts/release.mjs sign --key <密钥文件> --dir target/dist/v<版本>
//   node scripts/release.mjs publish --dir target/dist/v<版本>     （先加 --dry-run 看一遍）
//   本机上传到 R2、清缓存默认用登录的 Cloudflare 命令行 cf（npm install -g cf，cf auth login）；设了 R2_ENDPOINT 等环境变量时
//   与 CI 一样走 R2 的 S3 接口与 Cloudflare 的接口（见 publish 的说明）。不经 CI 时用 build 在本机构建、打包 Windows 的包。
// 以后有了 Windows 代码签名证书：程序要先签名再打进安装包与便携版 zip，安装包也要签名。到时在 fetch 与 sign 之间用
// signtool 给 stage\Termlet.exe、stage\bin\termlet.exe 签名（arm64 的在 stage-arm64 中），再 build --stage <目录>\stage
// （与 <目录>\stage-arm64）用它们重新打安装包与便携版 zip、重写 latest.yml（与 latest-arm64.yml；installer.nsi 加
// !finalize、!uninstfinalize 给安装包与卸载程序签名，要 NSIS 3.08 以上）；CI 打的安装包与 zip 中是没签名的程序。
//
// 架构：Windows、Linux 各出 x64 与 arm64（arm64 是实验版：没有真机，只在 CI 上编译、测试），macOS 只出 arm64。
// 包名带架构（termlet-<版本>-win-arm64.exe、-linux-arm64.deb 等，与 x64 的写法相同）；版本描述文件按架构分开，
// x64 的沿用最初的 latest.yml、latest-linux.yml（已发布的 x64 客户端读它们，内容也只列 x64 的包），arm64 的是
// latest-arm64.yml、latest-linux-arm64.yml（见 manifestName，与应用的 metadata_file_for 相同）。
//
// 用法：
//   node scripts/release.mjs prepare <版本> [--date YYYY-MM-DD] [--dry-run]
//       发版前的准备：Cargo.toml 的 [workspace.package] version 与 Cargo.lock 中本工作区各个包的版本改成 <版本>（不能比
//       现在的旧），两份更新日志最上面的“未发布”一节（## 0.3.0（未发布） / ## 0.3.0 (unreleased)）改成
//       “## <版本>（<日期>）” / “## <版本> (<日期>)”，日期默认是本机的今天。“未发布”一节不在最上面、没有内容、已有同版本的
//       一节时报错；已经准备过（标题就是这个版本与日期）时不改。不提交：核对 git diff 后自己提交。--dry-run 只列出要改的。
//   node scripts/release.mjs notes (--tag v<版本> | <版本>) [--out <文件>]
//       核对并输出这个版本的 GitHub Release 说明：更新日志中这一版的中文与英文两节（标题要是这个版本与发布日期，还是
//       “未发布”或缺了哪一份时报错），中文在前、英文在后，各接一段核对下载的方法（SHA256SUMS 的签名与构建来源证明）；
//       相对链接改成这个标签下的仓库地址。--tag 时另核对标签与 Cargo.toml 的版本一致（release.yml 打包前先用它核对）。
//       不给 --out 时打印到标准输出。
//   node scripts/release.mjs build [--arch x64|arm64] [--out <目录>] [--skip-cargo] [--stage <暂存目录>]
//       cargo build --release（termlet-desktop 与 termlet-cli，--locked），确认程序内置了更新公钥、是这个架构的程序，
//       准备暂存目录（x64 是 stage，arm64 是 stage-arm64），调用 makensis 打出安装包 termlet-<版本>-win-<架构>.exe，
//       再从同一个暂存目录打便携版 termlet-<版本>-win-<架构>-portable.zip（其中一个 Termlet 文件夹：Termlet.exe、
//       bin\termlet.exe、licenses、许可协议与便携标记 portable.txt，见 packaging/windows/portable.txt），计算哈希并写出
//       这个架构的版本描述文件（x64 是 latest.yml，arm64 是 latest-arm64.yml；先列安装程序，它是 path；再列便携版，
//       应用只按 -win-<架构>.exe 找安装包，官网从中取便携版的大小与 SHA-512）。
//       --arch 默认为本机 Rust 工具链的架构；与工具链不同时加 --target 交叉编译（要装好这个 Rust 目标与 MSVC 对应
//       架构的生成工具）。输出目录默认为 <cargo 的 target 目录>/dist；makensis 按 MAKENSIS 环境变量、PATH、NSIS 的
//       默认安装位置、electron-builder 的缓存依次查找。--skip-cargo 用已有的发布构建（例如刚构建过）。
//       --stage 用现成的暂存目录打包（例如 fetch 下载的、其中的程序签过名的）：不构建、不重新准备暂存目录，
//       暂存目录中记的版本要与 Cargo.toml 的相同，架构按其中的 Termlet.exe 定（给了 --arch 时要一致）；输出目录
//       默认为暂存目录的上一级。
//   node scripts/release.mjs collect --from <产物目录> --out <目录> --digest <产物名>=<SHA-256>... [--tag v<版本>]
//                                    [--require-arm64]
//       CI 的签名与发布 job 用，在碰任何秘密之前运行：actions/download-artifact 以 skip-decompress 把这次运行的产物原样
//       （zip）下载到 <产物目录>/<产物名>/；--digest 是构建 job 经 job 的输出报告的各产物摘要（upload-artifact 的
//       artifact-digest，值为空的当作没有报告）。先核对每个 zip 的 SHA-256 与报告的一致、下载到的都有报告、报告了的都
//       下载到，再按 CI_ARTIFACTS 取各平台（arm64 缺了时警告后跳过，见 fetch），解开、合进 <目录>，做与 fetch 相同的
//       核对（包与清单齐全、版本一致，--tag 时还要与标签一致，暂存目录中的程序是那个架构的）。核对不过时报错，不写出文件。
//   node scripts/release.mjs fetch (--tag v<版本> | --run <运行 ID>) [--out <目录>] [--repo <仓库>] [--require-arm64]
//       本机发布用：用 GitHub CLI（gh，要先登录）下载 release.yml 一次成功运行的产物（CI_ARTIFACTS）：Windows x64、arm64
//       的安装包、便携版、latest.yml / latest-arm64.yml 与暂存目录（stage / stage-arm64），Linux x64、arm64 的 tar.gz、
//       deb、rpm、AppImage 与 latest-linux.yml / latest-linux-arm64.yml，macOS 的包与 latest-mac.yml，都放进输出目录
//       （默认 <cargo 的 target 目录>/dist/<标签>）。仓库默认是 GITHUB_REPOSITORY 或 termlet/termlet。arm64 是实验版，
//       不挡 x64 与 macOS 的发布：release.yml 中 arm64 的 job 失败不算运行失败（continue-on-error），这时没有它的产物，
//       fetch 警告后只取其余的，这一版就不出那个 arm64 的清单与包（sign、publish 照目录中有的做；下载站上原有的 arm64
//       清单不动，arm64 的客户端停在旧版本）；加 --require-arm64 时 arm64 的产物也必需。核对：取到的各平台的包
//       （PACKAGES）与清单齐全、版本一致、每份清单列出的正是它该列的包，暂存目录中的程序是它那个架构的；运行用的工作流是
//       release.yml、调用的可复用工作流取自同一个提交；按标签找时还要是推送这个标签触发的、构建的提交就是本机的这个
//       标签（GitHub 按触发的提交取工作流文件，用的就是这个提交中的 release.yml）；每个产物的 zip 与 GitHub 记录的
//       SHA-256 摘要一致。--run 用于手动运行的：不核对触发方式、标签与本机的提交。核对不过时报错，不写出文件。
//   node scripts/release.mjs manifest --file <包> [--file <同一平台的其他包>]... [--out <目录>]
//       给一个平台的包写版本描述文件（与 build 写的 latest.yml 格式相同），文件名按包名中的平台取，与应用读取的一致
//       （crates/termlet-core/src/update/manifest.rs 的 metadata_file_for）：-win-x64.exe → latest.yml，
//       -win-arm64.exe → latest-arm64.yml，-mac-arm64.zip → latest-mac.yml，-linux-x64.* → latest-linux.yml，
//       -linux-arm64.* → latest-linux-arm64.yml。files 按给出的顺序列出各个包，第一个同时写进 path 与顶层的 sha512
//       （Windows 是安装程序，再列便携版；Linux 是 tar.gz，再列 deb、rpm、AppImage：Linux 的应用只比较版本、不读
//       files，官网按扩展名取各自的下载链接与 SHA-512）。包名中的版本要与 Cargo.toml 的相同，要列的包见 PACKAGES。
//       输出目录默认为第一个包所在的目录。CI 的 release.yml 打 Linux、macOS 的包后调用（Windows 的由 build 写）。
//   node scripts/release.mjs sign --key <在用的密钥文件> [--dir <目录>] [--no-app-check] [--require-arm64]
//       给目录中的 latest*.yml 签名（.minisig），生成 SHA256SUMS（本版本的全部包与清单）及其签名。本版本的包没有
//       列在它那个平台的清单中时报错。没有 arm64 的清单时提示这一版不出它（--require-arm64 时报错）。
//       签名前核对：这把密钥的公钥在 crates/termlet-core/src/update/keys.rs 中、每份 Windows 清单对应的暂存目录
//       （latest.yml → stage，latest-arm64.yml → stage-arm64）中的 Termlet.exe 是那个架构的程序并内置了它
//       （只有安装包时加 --no-app-check）、清单中列出的文件都在目录中且 SHA-512 一致。
//   node scripts/release.mjs sign-config --key <在用的密钥文件> [--days 90] <update.json>
//       给远程配置签名（update.json.minisig），签名带签发时间与有效期（默认 90 天，最长 366 天）：
//       过期后应用不再采用它（改用内置的更新源），到期前要重新签名。字段见 crates/termlet-core/src/remote_config.rs：
//       feeds、disabled、message、crashReportUrl、usageUrl（使用统计的接收地址，"off" 为所有客户端停发），例如
//       {"crashReportUrl":"https://api.termlet.dev/v1/crash","usageUrl":"https://api.termlet.dev/v1/stats"}
//       正在用的这份在 packaging/remote-config/update.json，由 remote-config.yml 签名、发布。
//   node scripts/release.mjs publish [--dir <目录>] [--dry-run] [--skip-r2] [--skip-github] [--skip-website]
//                                    [--notes <文件>] [--require-arm64]
//       发布 sign 签过名的文件：SHA256SUMS 中的安装包与 Linux、macOS 的包、latest*.yml、SHA256SUMS 与它们的签名
//       （没有 arm64 的清单时提示这一版不出它、下载站上原有的不动；--require-arm64 时报错）。先核对签名
//       （公钥在 keys.rs 中，可信注释与文件名、版本一致）与 SHA256SUMS 中每个文件的哈希，然后
//       1. 传到 R2 的桶（R2_BUCKET，默认 termlet-dl，对外是 https://dl.termlet.dev）：先传安装包，再传 SHA256SUMS 与签名、
//          清单的签名，最后传 latest*.yml，客户端不会先看到新清单、后才有安装包；每个文件核对 R2 记下的 MD5。
//          设了 R2_ENDPOINT（https://<账号 ID>.r2.cloudflarestorage.com）、R2_ACCESS_KEY_ID、R2_SECRET_ACCESS_KEY（只能读写
//          这个桶的对象的 R2 令牌）时走 R2 的 S3 接口（CI 用这个），否则用登录的 cf。
//       2. 清这些地址的 CDN 缓存（新版本立即生效），再经 https 取回核对。设了 CLOUDFLARE_PURGE_TOKEN（只能清 termlet.dev
//          缓存的 API 令牌）与 CLOUDFLARE_ZONE_ID 时调用 Cloudflare 的接口，否则用登录的 cf。
//       3. 在 GitHub 仓库（GITHUB_REPOSITORY，默认 termlet/termlet）上用 gh 建 Release v<版本>（--verify-tag：标签要已推送），
//          附件与 R2 上的相同，标为最新；说明是 notes 的输出（更新日志中这一版的中英文两节；--notes 可改为给出的 Markdown
//          文件），这一节还是“未发布”或缺了时报错。已有这个 Release 时补传、覆盖同名文件，上次没传完留下的草稿改为正式。
//          仓库读不到、标签不在仓库中、说明有问题时，在上传任何文件之前报错。--skip-github 不建 Release。
//       4. 设了 TERMLET_PAGES_DEPLOY_HOOK（Cloudflare Pages 的部署钩子）时调用它，官网重新构建（读下载站上的新清单与公开
//          仓库的更新日志）；没设时提示怎样手动重建。--skip-website 不调用。
//       在 GitHub Actions 中（GITHUB_ACTIONS=true）不用 cf：R2 与清缓存的环境变量、部署钩子（没加 --skip-website 时）缺了
//       就在上传之前报错。--dry-run 只核对并列出要做的事。中途失败时重新运行即可（同名的文件覆盖）。
//   node scripts/release.mjs publish-config [--dry-run] <update.json>
//       把 sign-config 签过的远程配置（update.json 与 update.json.minisig，签名要没过期）传到下载站、清缓存并核对
//       （R2 与清缓存的方式同 publish）。
//   测试 publish、publish-config：--prefix termlet-test-<任意>- 给对象名加前缀，传完、核对后删掉（不碰 GitHub，也不调用
//       部署钩子）；--test-pubkey <公钥> 另外信任一把测试用的公钥（只能与 --prefix 一起用）。
//   node scripts/release.mjs newkey --out <仓库外的目录> [--name next]
//       轮换密钥时生成一把新密钥（步骤见 crates/termlet-core/src/update/keys.rs 的说明）。
//   node scripts/release.mjs licenses [--out <文件>] [--target <Rust 目标>]
//       写出发布构建用到的 Rust 依赖的许可文本（打包时放进安装目录的 licenses\third-party-rust.txt），
//       默认输出到 <cargo 的 target 目录>/dist/third-party-rust.txt，用来在打包前检查。--target 默认为 Windows x64。
//   node scripts/release.mjs notices --target <Rust 目标> --out <目录>
//       把随程序分发的许可文本（许可协议 license_*.txt 与 licenses 目录）写进目录，供其他平台打包
//       （CI 的 .github/workflows/release.yml 调用）；与 Windows 安装包中的相同，只是不带 BOM、用 LF 换行。
//   node scripts/release.mjs show <密钥文件>       显示密钥文件中的公钥与密钥 ID
//   node scripts/release.mjs selftest             用固定样例核对签名的实现（与应用的校验一致）与 S3 请求的签名
//
// 私钥：密钥文件中的 Ed25519 私钥用口令经 scrypt 派生的密钥以 AES-256-GCM 加密，用 --key 或环境变量 TERMLET_UPDATE_KEY
// 给出密钥文件的路径。在用的那把的密钥文件内容放在 GitHub release 环境的 Secrets 中
// （TERMLET_UPDATE_KEY_JSON，口令另存为 TERMLET_UPDATE_KEY_PASSPHRASE），CI 中签名的那一步把它写进 $RUNNER_TEMP 下权限 600
// 的临时文件、签完删掉；离线备用的那把只在仓库外离线保管、不进 CI。口令先读环境变量 TERMLET_UPDATE_KEY_PASSPHRASE
// （读过随即从本进程的环境中删掉，不传给子进程，也不打印），没有时在终端中输入、不回显（Git Bash 的 mintty 不行，
// 用 PowerShell 或 Windows Terminal）。
import { execFileSync, spawnSync } from 'node:child_process'
import {
  createCipheriv,
  createDecipheriv,
  createHash,
  createHmac,
  createPrivateKey,
  createPublicKey,
  generateKeyPairSync,
  randomBytes,
  scryptSync,
  sign,
  verify,
} from 'node:crypto'
import {
  closeSync,
  copyFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, dirname, isAbsolute, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { parseArgs } from 'node:util'

const root = join(fileURLToPath(import.meta.url), '../..')
const KEYS_FILE = join(root, 'crates/termlet-core/src/update/keys.rs')
const NSIS_SCRIPT = join(root, 'packaging/windows/installer.nsi')
/**
 * Windows 的两种架构：Rust 目标（许可文本也按它列出依赖）、程序 PE 头中的机器类型（核对构建出的是不是这个架构的
 * 程序）、暂存目录名（x64 沿用最初的 stage；fetch 把两个架构的产物合进一个目录，名字不能相同）。
 */
const WINDOWS_ARCHES = {
  x64: { target: 'x86_64-pc-windows-msvc', machine: 0x8664, stage: 'stage' },
  arm64: { target: 'aarch64-pc-windows-msvc', machine: 0xaa64, stage: 'stage-arm64' },
}
/** licenses 命令默认的 Rust 目标 */
const WINDOWS_TARGET = WINDOWS_ARCHES.x64.target
/** 暂存目录中记录版本号的文件（build --stage 核对；安装程序不装它） */
const STAGE_VERSION = 'version.txt'
/**
 * 源码仓库：CI 构建发布版（fetch 从这里下载产物）、发布 GitHub Releases（客户端的备用更新源与海外的下载）。
 * 在 GitHub Actions 中是运行所在的仓库（GITHUB_REPOSITORY）
 */
const REPO = process.env.GITHUB_REPOSITORY || 'termlet/termlet'
const CI_WORKFLOW = 'release.yml'
const CI_WORKFLOW_PATH = `.github/workflows/${CI_WORKFLOW}`
/**
 * release.yml 的产物与其中的包所属的平台（PACKAGES 中按它分）。experimental 的是实验版（arm64）：它的 job 失败时
 * 不算运行失败、没有产物，fetch 警告后跳过（--require-arm64 时也必需）；其余的都必需。
 */
const CI_ARTIFACTS = [
  { name: 'windows-x64', platform: 'win-x64' },
  { name: 'windows-arm64', platform: 'win-arm64', experimental: true },
  { name: 'linux-x64', platform: 'linux-x64' },
  { name: 'linux-arm64', platform: 'linux-arm64', experimental: true },
  { name: 'macos-arm64', platform: 'mac-arm64' },
]
/**
 * 发布的包（文件名 termlet-<版本>-<后缀>，下载站的防火墙只放行 /termlet-*），都列进它那个平台、那个架构的版本描述
 * 文件（manifestName）。同一份清单中先列的是清单的 path：Windows 是安装程序（安装版据此自动更新），便携版 zip 跟在
 * 后面（应用只按 -win-<架构>.exe 找安装包；便携版只提示新版本）；Linux 是 tar.gz，deb、rpm、AppImage 跟在后面
 * （应用只提示新版本、不读 files）。官网按扩展名从清单中取各自的下载链接与 SHA-512。fetch 要求它们都在。
 */
const PACKAGES = [
  'win-x64.exe',
  'win-x64-portable.zip',
  'win-arm64.exe',
  'win-arm64-portable.zip',
  'linux-x64.tar.gz',
  'linux-x64.deb',
  'linux-x64.rpm',
  'linux-x64.AppImage',
  'linux-arm64.tar.gz',
  'linux-arm64.deb',
  'linux-arm64.rpm',
  'linux-arm64.AppImage',
  'mac-arm64.zip',
]
/** 包名：版本（可以带 -beta.1 这类后缀）、平台（系统-架构）、便携版、扩展名 */
const PACKAGE_NAME =
  /^termlet-(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)-((win|mac|linux)-([a-z0-9]+))(-portable)?\.(exe|tar\.gz|zip|deb|rpm|AppImage)$/
/** 便携版 zip 中的顶层文件夹与便携标记（标记的文件名与 termlet-proto 的 endpoint::PORTABLE_MARKER 相同） */
const PORTABLE_FOLDER = 'Termlet'
const PORTABLE_MARKER = 'portable.txt'
const MANIFEST = 'latest.yml'
const KEY_FORMAT = 'termlet-update-key-1'
// scrypt 参数：约 0.5 秒、128MB 内存，增加离线猜口令的成本
const SCRYPT = { N: 2 ** 17, r: 8, p: 1, maxmem: 256 * 1024 * 1024 }
const MIN_PASSPHRASE = 6
/** 远程配置签名的有效期（天） */
const CONFIG_DAYS = { default: 90, max: 366 }
/** 随安装包分发的第三方许可文本：[仓库中的位置, 安装目录 licenses 下的文件名] */
const LICENSE_FILES = [
  ['crates/termlet-terminal/assets/fonts/JetBrainsMono-OFL.txt', 'JetBrainsMono-OFL.txt'],
  ['crates/termlet-core/src/termxfer/LICENSE-trzsz-go.txt', 'trzsz-go-MIT.txt'],
  ['crates/termlet-core/src/termxfer/LICENSE-go-zmodem.txt', 'go-zmodem.txt'],
  // 内置终端配色的来源与许可（由 termlet-core 的测试生成并核对）
  ['packaging/licenses/terminal-color-schemes.txt', 'terminal-color-schemes.txt'],
  // SFTP 文件列表的彩色文件图标的来源与许可（由 scripts/file-icons.py 生成）
  ['crates/termlet-desktop/assets/file-icons/LICENSE', 'catppuccin-vscode-icons-MIT.txt'],
  // 系统与品牌标志（Simple Icons）的来源、许可与商标说明（由 scripts/brand-icons.py 生成）
  ['crates/termlet-desktop/assets/brand-icons/LICENSE', 'simple-icons.txt'],
]
/** Rust 依赖的许可文本写在这个文件中（安装目录 licenses 下） */
const RUST_LICENSES = 'third-party-rust.txt'
/**
 * Termlet 的许可说明（安装向导的许可页用中文简体、繁体或英文的那份，都放进安装目录）：packaging/windows 中的说明
 * 后面接上仓库根部 LICENSE 的许可证全文（AGPL-3.0，英文原文），随程序分发的每一份都带着全文
 */
const LICENSE_AGREEMENTS = ['license_zh_CN.txt', 'license_zh_TW.txt', 'license_en.txt']
const LICENSE_TEXT = 'LICENSE'
/** 没有附带许可文件的 crate 用的标准全文：SPDX 标识 → 仓库中的位置 */
const STANDARD_LICENSES = {
  MIT: 'packaging/licenses/standard-MIT.txt',
  'Apache-2.0': 'packaging/licenses/standard-Apache-2.0.txt',
}
/** crate 目录中算作许可文本的文件 */
const LICENSE_FILE_NAME = /^(licen[cs]e|copying|copyright|notice|unlicense)([-._].*)?$/i
const UNTRUSTED = 'untrusted comment: '
const TRUSTED = 'trusted comment: '
/**
 * 下载站：R2 的桶、绑定的地址与所在的 Cloudflare 站点（cf 清缓存时用站点名；走 Cloudflare 的接口时用
 * CLOUDFLARE_ZONE_ID）。桶与地址可用环境变量 R2_BUCKET、TERMLET_DL_ORIGIN 换掉（测试用）
 */
const R2_BUCKET = process.env.R2_BUCKET || 'termlet-dl'
const DL_ORIGIN = (process.env.TERMLET_DL_ORIGIN || 'https://dl.termlet.dev').replace(/\/+$/, '')
const DL_ZONE = 'termlet.dev'
/**
 * 更新日志：中文与英文两份，在仓库根部。平时写进最上面标着“未发布”的一节，发版时 prepare 把标题改成版本号与日期；
 * GitHub Release 的说明取这一版的两节（notes）。标题的写法与官网读取时认的相同
 */
const CHANGELOGS = [
  {
    name: 'CHANGELOG.md',
    unreleased: '未发布',
    title: (version, date) => `## ${version}（${date}）`,
  },
  {
    name: 'CHANGELOG.en.md',
    unreleased: 'unreleased',
    title: (version, date) => `## ${version} (${date})`,
  },
]
/** 版本标题：“## 1.2.3（2026-11-20）”“## 1.2.3（未发布）”，英文 “## 1.2.3 (2026-11-20)”“## 1.2.3 (unreleased)” */
const CHANGELOG_HEADING = /^##[ \t]+(\S+?)[ \t]*[（(][ \t]*([^（()）]+?)[ \t]*[）)][ \t]*$/
const UNRELEASED = /^(?:未发布|unreleased)$/i
/** 版本号（可以带 -beta.1 这类后缀） */
const VERSION = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/
/** 官网：Cloudflare Pages 的部署钩子（POST 一次就重新构建）的地址都在这个前缀下，钩子的地址本身是秘密，不打印 */
const PAGES_HOOK_PREFIX = 'https://api.cloudflare.com/client/v4/pages/webhooks/deploy_hooks/'
/** 在 GitHub Actions 中运行：上传、清缓存只用环境变量给出的令牌（运行器上没有登录的 cf），缺了就在上传之前报错 */
const IN_CI = process.env.GITHUB_ACTIONS === 'true'
/** --prefix（测试用）只能是这样：经 https 访问得到（防火墙只放行 /termlet-* 等），不会与正式文件重名 */
const TEST_PREFIX = /^termlet-test-[a-z0-9-]*$/
/** 一次清缓存最多的地址数（免费版的上限） */
const PURGE_BATCH = 30
/** 不超过这个大小的文件经 https 取回比较内容，更大的（安装包）只比较大小与 ETag */
const FETCH_COMPARE_MAX = 1024 * 1024

function fail(message) {
  console.error(`错误：${message}`)
  process.exit(1)
}

// ---------------------------------------------------------------- minisign 格式

/** 密钥 ID 的显示形式，与 minisign 相同（按小端整数显示的十六进制） */
function keyIdHex(id) {
  return Buffer.from(id).reverse().toString('hex').toUpperCase()
}

/** 公钥编码为 minisign 格式（base64 的那一行） */
function encodePublicKey(id, key) {
  const x = key.export({ format: 'jwk' }).x
  if (key.asymmetricKeyType !== 'ed25519' || !x) fail('不是 Ed25519 公钥')
  return Buffer.concat([Buffer.from('Ed'), id, Buffer.from(x, 'base64url')]).toString('base64')
}

/** 解析 minisign 格式的公钥（base64 的那一行） */
function parsePublicKey(line) {
  const raw = Buffer.from(line, 'base64')
  if (raw.length !== 42 || raw.subarray(0, 2).toString() !== 'Ed') fail(`公钥的格式不对：${line}`)
  const key = createPublicKey({
    key: { kty: 'OKP', crv: 'Ed25519', x: raw.subarray(10).toString('base64url') },
    format: 'jwk',
  })
  return { id: raw.subarray(2, 10), key }
}

function blake2b(message) {
  return createHash('blake2b512').update(message).digest()
}

/** 签名，返回 minisign 格式的签名文件内容（对 BLAKE2b-512 摘要签名，算法标记 ED） */
function createSignature(
  message,
  secret,
  trustedComment,
  untrustedComment = 'signature from termlet release key',
) {
  if (/[\r\n]/.test(trustedComment + untrustedComment)) fail('注释不能换行')
  const signature = sign(null, blake2b(message), secret.key)
  const global = sign(
    null,
    Buffer.concat([signature, Buffer.from(trustedComment, 'utf8')]),
    secret.key,
  )
  const line = Buffer.concat([Buffer.from('ED'), secret.id, signature]).toString('base64')
  return `${UNTRUSTED}${untrustedComment}\n${line}\n${TRUSTED}${trustedComment}\n${global.toString('base64')}\n`
}

/** 用受信任的公钥校验签名（签名后自检、发布前核对），返回可信注释；`what` 是出错时说明的开头 */
function verifySignature(message, signatureText, keys, what = '签名自检失败') {
  const lines = signatureText.split('\n')
  const raw = Buffer.from(lines[1] ?? '', 'base64')
  const trusted = (lines[2] ?? '').slice(TRUSTED.length)
  const global = Buffer.from(lines[3] ?? '', 'base64')
  const key = keys.map(parsePublicKey).find((k) => k.id.equals(raw.subarray(2, 10)))
  if (!key) fail(`${what}：签名所用的密钥不在受信任的公钥中`)
  const signature = raw.subarray(10)
  if (!verify(null, blake2b(message), key.key, signature)) fail(`${what}：签名与内容不符`)
  if (!verify(null, Buffer.concat([signature, Buffer.from(trusted, 'utf8')]), key.key, global)) {
    fail(`${what}：可信注释的签名无效`)
  }
  return trusted
}

function manifestComment(file, version) {
  return `termlet-manifest ${file} ${version}`
}

function isoSeconds(d) {
  return d.toISOString().replace(/\.\d{3}Z$/, 'Z')
}

function remoteConfigComment(issued, expires) {
  return `termlet-remote-config ${isoSeconds(issued)} ${isoSeconds(expires)}`
}

function checksumsComment(version) {
  return `termlet-sha256sums ${version}`
}

// ---------------------------------------------------------------- 口令与密钥文件

/** 在终端中读一行，不回显 */
function readHidden(prompt) {
  const stdin = process.stdin
  process.stdout.write(prompt)
  stdin.setRawMode(true)
  stdin.resume()
  stdin.setEncoding('utf8')
  return new Promise((done) => {
    let value = ''
    const onData = (chunk) => {
      for (const ch of chunk) {
        if (ch === '\r' || ch === '\n') {
          stdin.off('data', onData)
          stdin.setRawMode(false)
          stdin.pause()
          process.stdout.write('\n')
          done(value)
          return
        }
        if (ch === '\u0003') {
          stdin.setRawMode(false)
          process.stdout.write('\n')
          process.exit(130)
        }
        if (ch === '\u007f' || ch === '\b') value = value.slice(0, -1)
        else if (ch >= ' ') value += ch
      }
    }
    stdin.on('data', onData)
  })
}

/**
 * 读环境变量中的秘密，随即从本进程的环境中删掉（之后运行的 cargo、gh、cf 等子进程拿不到）；没有设时为 undefined
 */
function takeEnvSecret(name) {
  const value = process.env[name]
  delete process.env[name]
  return value || undefined
}

/** 口令：先读环境变量 TERMLET_UPDATE_KEY_PASSPHRASE（CI），没有时在终端中输入 */
async function askPassphrase(prompt) {
  const value = takeEnvSecret('TERMLET_UPDATE_KEY_PASSPHRASE')
  if (value) return value
  if (process.stdin.isTTY) return readHidden(prompt)
  fail(
    '没有终端，无法输入口令：请在 PowerShell 或 Windows Terminal 中运行（Git Bash 的 mintty 不支持隐藏输入），' +
      '或设置环境变量 TERMLET_UPDATE_KEY_PASSPHRASE',
  )
}

async function askNewPassphrase(label) {
  if (process.env.TERMLET_UPDATE_KEY_PASSPHRASE || !process.stdin.isTTY) {
    const value = await askPassphrase('')
    if ([...value].length < MIN_PASSPHRASE) fail(`口令太短，至少 ${MIN_PASSPHRASE} 个字符`)
    return value
  }
  for (;;) {
    const first = await readHidden(`为${label}设置口令（至少 ${MIN_PASSPHRASE} 个字符）：`)
    if ([...first].length < MIN_PASSPHRASE) {
      console.log(`口令太短，至少 ${MIN_PASSPHRASE} 个字符`)
      continue
    }
    if ((await readHidden('再输入一次：')) === first) return first
    console.log('两次输入不一致，请重新输入')
  }
}

function kek(passphrase, salt, params) {
  return scryptSync(passphrase.normalize('NFC'), salt, 32, { ...SCRYPT, ...params })
}

function aad(publicKey) {
  return Buffer.from(`${KEY_FORMAT}\n${publicKey}`)
}

function encryptKey(name, privateKey, id, passphrase) {
  const publicKey = encodePublicKey(id, createPublicKey(privateKey))
  const salt = randomBytes(16)
  const iv = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', kek(passphrase, salt), iv)
  cipher.setAAD(aad(publicKey))
  const der = privateKey.export({ type: 'pkcs8', format: 'der' })
  const secret = Buffer.concat([cipher.update(der), cipher.final()])
  return {
    format: KEY_FORMAT,
    name,
    keyId: keyIdHex(id),
    publicKey,
    created: new Date().toISOString(),
    kdf: { name: 'scrypt', N: SCRYPT.N, r: SCRYPT.r, p: SCRYPT.p, salt: salt.toString('base64') },
    cipher: {
      name: 'aes-256-gcm',
      iv: iv.toString('base64'),
      tag: cipher.getAuthTag().toString('base64'),
    },
    secret: secret.toString('base64'),
  }
}

function readKeyFile(path) {
  if (!existsSync(path)) fail(`找不到密钥文件 ${path}`)
  let data
  try {
    data = JSON.parse(readFileSync(path, 'utf8'))
  } catch {
    fail(`${path} 不是密钥文件`)
  }
  if (data?.format !== KEY_FORMAT) fail(`${path} 不是发布脚本生成的密钥文件`)
  return data
}

/** 解密密钥文件，返回签名用的私钥 */
function decryptKey(data, passphrase) {
  let der
  try {
    const { N, r, p } = data.kdf
    const decipher = createDecipheriv(
      'aes-256-gcm',
      kek(passphrase, Buffer.from(data.kdf.salt, 'base64'), { N, r, p }),
      Buffer.from(data.cipher.iv, 'base64'),
    )
    decipher.setAAD(aad(data.publicKey))
    decipher.setAuthTag(Buffer.from(data.cipher.tag, 'base64'))
    der = Buffer.concat([decipher.update(Buffer.from(data.secret, 'base64')), decipher.final()])
  } catch {
    fail('口令不对，或密钥文件已损坏')
  }
  const key = createPrivateKey({ key: der, format: 'der', type: 'pkcs8' })
  const { id } = parsePublicKey(data.publicKey)
  if (encodePublicKey(id, createPublicKey(key)) !== data.publicKey)
    fail('密钥文件中的公钥与私钥不符')
  return { id, key }
}

/** 应用内置的公钥（crates/termlet-core/src/update/keys.rs 中 UPDATE_PUBLIC_KEYS 的字符串） */
function builtinKeys() {
  const text = readFileSync(KEYS_FILE, 'utf8')
  const body = /UPDATE_PUBLIC_KEYS[^=]*=\s*&\[([\s\S]*?)\];/.exec(text)?.[1] ?? ''
  const code = body
    .split('\n')
    .map((l) => l.replace(/\/\/.*$/, ''))
    .join('\n')
  return [...code.matchAll(/"(RW[A-Za-z0-9+/]{54})"/g)].map((m) => m[1])
}

/**
 * 签名用的密钥：读密钥文件（--key 或 TERMLET_UPDATE_KEY 给出路径；CI 中是签名那一步从 Secrets 写到 $RUNNER_TEMP 的
 * 临时文件），并确认应用会接受它
 */
async function loadSigningKey(values) {
  const keyPath = values.key ?? process.env.TERMLET_UPDATE_KEY
  if (!keyPath) fail('用 --key 或环境变量 TERMLET_UPDATE_KEY 指定在用的密钥文件')
  const keys = builtinKeys()
  if (keys.length === 0) fail(`${relative(root, KEYS_FILE)} 中没有公钥`)
  const data = readKeyFile(resolve(keyPath))
  if (!keys.includes(data.publicKey)) {
    fail(
      `这把密钥（${data.keyId}）的公钥不在 ${relative(root, KEYS_FILE)} 中，应用不会接受它签的文件`,
    )
  }
  const secret = decryptKey(data, await askPassphrase(`${data.name}（${data.keyId}）的口令：`))
  return { data, keys, secret }
}

/** 保存密钥的目录：必须在仓库外 */
function keyDir(out) {
  if (!out) fail('用 --out 指定保存密钥的目录（放在仓库外）')
  const dir = resolve(out)
  const rel = relative(root, dir)
  if (!rel.startsWith('..') && !isAbsolute(rel)) fail('密钥目录不能在仓库里（避免误提交私钥）')
  return dir
}

// ---------------------------------------------------------------- 构建与打包

function cargoTargetDir() {
  const meta = JSON.parse(
    execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps'], {
      cwd: root,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    }),
  )
  return meta.target_directory
}

/** workspace 的版本号（Cargo.toml 的 [workspace.package] version） */
function workspaceVersion() {
  const text = readFileSync(join(root, 'Cargo.toml'), 'utf8')
  const section = /\[workspace\.package\]([\s\S]*?)(?:\n\[|$)/.exec(text)?.[1] ?? ''
  const version = /^version\s*=\s*"([^"]+)"/m.exec(section)?.[1]
  if (!version) fail('Cargo.toml 的 [workspace.package] 中没有 version')
  return version
}

/**
 * 发布构建用到的 Rust 依赖（termlet-desktop 与 termlet-cli 在 target 平台上的普通依赖，不含 workspace 自己的 crate）
 * 的许可文本：每个 crate 列出名称、版本、许可表达式与仓库地址，并附上它的源码目录中的许可文件全文。
 */
function rustLicenses(target) {
  const run = (args) =>
    execFileSync('cargo', args, { cwd: root, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 })
  const tree = run([
    'tree',
    '--locked',
    '-p',
    'termlet-desktop',
    '-p',
    'termlet-cli',
    '-e',
    'normal',
    '--target',
    target,
    '--prefix',
    'none',
    '--format',
    '{p}',
  ])
  const used = new Set()
  for (const line of tree.split(/\r?\n/)) {
    const m = /^(\S+) v(\S+)/.exec(line.trim())
    if (m) used.add(`${m[1]} ${m[2]}`)
  }
  const meta = JSON.parse(run(['metadata', '--format-version', '1', '--locked']))
  const workspace = new Set(meta.workspace_members)
  const packages = meta.packages
    .filter((p) => used.has(`${p.name} ${p.version}`) && !workspace.has(p.id))
    .sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version))
  if (packages.length === 0) fail('cargo tree 没有列出任何依赖')
  const rule = '='.repeat(78)
  const parts = [
    'Termlet 用到的 Rust 开源组件及其许可',
    'Open-source Rust components used by Termlet and their licenses',
    '',
    `共 ${packages.length} 个 crate；本文件由 scripts/release.mjs 在打包时生成。`,
    '',
  ]
  let missing = 0
  // 许多 crate 带的是一字不差的同一份许可全文（尤其是 Apache-2.0）：只写第一次，之后指向它
  const seen = new Map()
  // 没有附带许可文件的 crate 用到的标准全文
  const needed = new Set()
  for (const p of packages) {
    const dir = dirname(p.manifest_path)
    const files = readdirSync(dir)
      .filter((f) => LICENSE_FILE_NAME.test(f) && statSync(join(dir, f)).isFile())
      .sort()
    parts.push(rule, `${p.name} ${p.version}`, `License: ${p.license ?? p.license_file ?? '(未注明)'}`)
    if (p.repository) parts.push(p.repository)
    parts.push('')
    if (files.length === 0) {
      // 没有附带许可文件：按许可表达式指向文末的标准全文（选其中有标准全文的）
      missing += 1
      const ids = (p.license ?? '').split(/\s+OR\s+|\//).map((id) => id.trim())
      const known = ids.filter((id) => STANDARD_LICENSES[id])
      if (known.length === 0)
        fail(`${p.name} ${p.version} 没有附带许可文件，许可 ${p.license} 也没有标准全文`)
      for (const id of known) needed.add(id)
      parts.push(`（这个 crate 没有附带许可文件；许可全文见文末的 ${known.join(' / ')} 标准全文）`, '')
    }
    for (const f of files) {
      const body = readFileSync(join(dir, f), 'utf8').replace(/\r\n/g, '\n').trimEnd()
      const first = seen.get(body)
      if (first) {
        parts.push(`--- ${f}：与上文 ${first} 的全文相同 ---`, '')
      } else {
        seen.set(body, `${p.name} ${p.version} 的 ${f}`)
        parts.push(`--- ${f} ---`, body, '')
      }
    }
  }
  for (const id of [...needed].sort()) {
    const body = readFileSync(join(root, STANDARD_LICENSES[id]), 'utf8').replace(/\r\n/g, '\n').trimEnd()
    parts.push(rule, `${id} 标准全文`, '', body, '')
  }
  return { text: `${parts.join('\n')}\n`, count: packages.length, missing }
}

function writeRustLicenses(path, target) {
  const { text, count, missing } = rustLicenses(target)
  writeFileSync(path, text)
  console.log(`Rust 依赖的许可：${count} 个 crate（${missing} 个没有附带许可文件）→ ${path}`)
}

/**
 * 随程序分发的许可文本写进 dir：许可说明 license_*.txt（接上许可证全文），licenses 下的第三方许可（Rust 依赖按 target 平台列出）。
 * windows 为真时许可协议带 BOM、用 CRLF（NSIS 的许可页按 BOM 识别 UTF-8）。
 */
function writeNotices(dir, target, windows) {
  mkdirSync(join(dir, 'licenses'), { recursive: true })
  for (const [from, to] of LICENSE_FILES) {
    const path = join(root, from)
    if (!existsSync(path)) fail(`找不到许可文本 ${from}`)
    copyFileSync(path, join(dir, 'licenses', to))
  }
  writeRustLicenses(join(dir, 'licenses', RUST_LICENSES), target)
  const fullText = readFileSync(join(root, LICENSE_TEXT), 'utf8').replace(/\r\n/g, '\n').trimEnd()
  for (const name of LICENSE_AGREEMENTS) {
    const notice = readFileSync(join(root, 'packaging/windows', name), 'utf8')
      .replace(/^\uFEFF/, '')
      .replace(/\r\n/g, '\n')
      .trimEnd()
    const text = `${notice}\n\n${'='.repeat(78)}\n\n${fullText}\n`
    writeFileSync(join(dir, name), windows ? windowsBytes(text, true) : text)
  }
}

function findMakensis() {
  const candidates = []
  if (process.env.MAKENSIS) candidates.push(process.env.MAKENSIS)
  try {
    const found = execFileSync('where', ['makensis'], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    })
    candidates.push(...found.split(/\r?\n/).filter(Boolean))
  } catch {
    // PATH 中没有
  }
  candidates.push('C:/Program Files (x86)/NSIS/makensis.exe', 'C:/Program Files/NSIS/makensis.exe')
  const cache = join(process.env.LOCALAPPDATA ?? '', 'electron-builder/Cache')
  if (existsSync(cache)) {
    for (const name of readdirSync(cache).filter(
      (n) => n.startsWith('nsis-') && !n.startsWith('nsis-resources'),
    )) {
      for (const sub of readdirSync(join(cache, name))) {
        candidates.push(join(cache, name, sub, 'Bin/makensis.exe'))
      }
    }
  }
  return candidates.find((p) => existsSync(p))
}

function sha(path, algorithm, encoding) {
  return createHash(algorithm).update(readFileSync(path)).digest(encoding)
}

/** 带 UTF-8 BOM、CRLF 换行的文本（NSIS 的许可页按 BOM 识别 UTF-8；PowerShell 5.1 读没有 BOM 的脚本时按系统代码页） */
function windowsText(path, crlf) {
  return windowsBytes(readFileSync(path, 'utf8'), crlf)
}

/** 同上，内容是给出的文本 */
function windowsBytes(text, crlf) {
  text = text.replace(/^\uFEFF/, '')
  if (crlf) text = text.replace(/\r?\n/g, '\r\n')
  return Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), Buffer.from(text, 'utf8')])
}

function formatSize(bytes) {
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

/** 程序要内置受信任的公钥，否则装上的版本不认之后签的更新 */
function checkAppKeys(app) {
  const exe = readFileSync(app)
  for (const key of builtinKeys()) {
    if (!exe.includes(Buffer.from(key))) fail(`${app} 中没有公钥 ${key}：先确认 keys.rs 后重新构建`)
  }
}

/**
 * Windows 程序的架构（WINDOWS_ARCHES 的键）：按 PE 文件头中的机器类型（IMAGE_FILE_HEADER.Machine）判断。
 * 不是 x64、arm64 的程序时抛出（fetch 要先清理临时目录）；其他地方用 windowsArchOf。
 */
function readWindowsArch(path) {
  const exe = readFileSync(path)
  const pe = exe.length >= 0x40 ? exe.readUInt32LE(0x3c) : -1
  const valid =
    exe.toString('latin1', 0, 2) === 'MZ' &&
    pe >= 0 &&
    pe + 6 <= exe.length &&
    exe.toString('latin1', pe, pe + 4) === 'PE\0\0'
  if (!valid) throw new Error(`${path} 不是 Windows 程序`)
  const machine = exe.readUInt16LE(pe + 4)
  const arch = Object.keys(WINDOWS_ARCHES).find((a) => WINDOWS_ARCHES[a].machine === machine)
  if (!arch) throw new Error(`${path} 的机器类型是 0x${machine.toString(16)}，不是 x64 或 arm64 的程序`)
  return arch
}

function windowsArchOf(path) {
  try {
    return readWindowsArch(path)
  } catch (e) {
    fail(e.message)
  }
}

/** 程序是这个架构的（防止把别的架构的程序打进安装包、签进清单） */
function checkWindowsArch(path, arch) {
  const actual = windowsArchOf(path)
  if (actual !== arch) fail(`${path} 是 ${actual} 的程序，应为 ${arch}`)
}

/**
 * 现成的暂存目录（build --stage）：安装程序要的文件都在，记的版本与 Cargo.toml 的相同，程序内置了公钥；
 * 返回其中的程序的架构（两个程序要相同）。
 */
function checkStage(stage, version) {
  const needed = ['Termlet.exe', 'bin/termlet.exe', 'helper.ps1', 'licenses', ...LICENSE_AGREEMENTS]
  for (const f of needed) if (!existsSync(join(stage, f))) fail(`暂存目录中没有 ${f}：${stage}`)
  const marker = join(stage, STAGE_VERSION)
  const staged = existsSync(marker) ? readFileSync(marker, 'utf8').trim() : '(没有记录)'
  if (staged !== version) {
    fail(`暂存目录的版本 ${staged} 与 Cargo.toml 的 ${version} 不同：先检出同一个版本的标签`)
  }
  checkAppKeys(join(stage, 'Termlet.exe'))
  const arch = windowsArchOf(join(stage, 'Termlet.exe'))
  checkWindowsArch(join(stage, 'bin/termlet.exe'), arch)
  return arch
}

/**
 * 平台（包名中的 win-x64、win-arm64、mac-arm64、linux-x64、linux-arm64）的版本描述文件名，与应用读取的相同
 * （termlet-core 的 update::manifest::metadata_file_for）：x64 沿用最初的 latest.yml、latest-linux.yml（已发布的
 * x64 客户端读它们），其他架构在后面加 -<架构>（latest-arm64.yml、latest-linux-arm64.yml）；macOS 都是
 * latest-mac.yml。
 */
function manifestName(platform) {
  const [os, arch] = platform.split('-')
  if (os === 'mac') return 'latest-mac.yml'
  const base = os === 'win' ? 'latest' : 'latest-linux'
  return arch === 'x64' ? `${base}.yml` : `${base}-${arch}.yml`
}

/**
 * 这一版没有的实验版（arm64）清单：提示不出它们与其中的包（下载站上原有的不动，那个架构的客户端停在旧版本）；
 * --require-arm64 时报错。sign、publish 调用。
 */
function checkArm64Manifests(manifests, values) {
  const missing = CI_ARTIFACTS.filter((a) => a.experimental)
    .map((a) => manifestName(a.platform))
    .filter((m) => !manifests.includes(m))
  if (missing.length === 0) return
  if (values['require-arm64']) fail(`没有 ${missing.join('、')}（加了 --require-arm64）`)
  console.log(
    `注意：这一版没有 ${missing.join('、')}（arm64 是实验版）：不出它们与其中的包，下载站上原有的不动，` +
      '那个架构的客户端停在旧版本',
  )
}

/** 这个文件名是不是这个版本发布的包（termlet-<版本>-<PACKAGES 中的后缀>） */
function isPackage(name, version) {
  const prefix = `termlet-${version}-`
  return name.startsWith(prefix) && PACKAGES.includes(name.slice(prefix.length))
}

/**
 * 给一个平台的包写版本描述文件（electron-builder 的 latest.yml 格式），返回文件名。应用据此比较版本；
 * Windows 安装版还据此下载并核对安装包，Linux、macOS 只提示新版本（照样校验签名）。files 按给出的顺序列出
 * 各个包，第一个同时是 path 与顶层的 sha512。
 */
function writeManifest(files, out) {
  const expected = workspaceVersion()
  const entries = files.map((file) => {
    const name = basename(file)
    const m = PACKAGE_NAME.exec(name)
    if (!m) fail(`${name} 不是发布的包名（termlet-<版本>-<平台>.<扩展名>）`)
    const [, version, platform] = m
    if (version !== expected) fail(`${name} 的版本 ${version} 与 Cargo.toml 的 ${expected} 不同`)
    if (!isPackage(name, version)) fail(`${name} 不是要发布的包（见 PACKAGES）`)
    if (!existsSync(file)) fail(`找不到 ${file}`)
    return { name, platform, sha512: sha(file, 'sha512', 'base64'), size: statSync(file).size }
  })
  const manifest = manifestName(entries[0].platform)
  const other = entries.find((e) => manifestName(e.platform) !== manifest)
  if (other) fail(`${other.name} 与 ${entries[0].name} 不属于同一份版本描述文件`)
  const names = new Set(entries.map((e) => e.name))
  if (names.size !== entries.length) fail('同一个包给了两次')
  const text = [
    `version: ${expected}`,
    'files:',
    ...entries.flatMap((e) => [`  - url: ${e.name}`, `    sha512: ${e.sha512}`, `    size: ${e.size}`]),
    `path: ${entries[0].name}`,
    `sha512: ${entries[0].sha512}`,
    `releaseDate: '${new Date().toISOString()}'`,
    '',
  ].join('\n')
  mkdirSync(out, { recursive: true })
  writeFileSync(join(out, manifest), text)
  // 旧的签名不再对应这份清单
  rmSync(join(out, `${manifest}.minisig`), { force: true })
  return manifest
}

/**
 * 便携版 zip：一个 Termlet 文件夹，其中是暂存目录中的程序、第三方许可与许可协议（与安装目录相同：命令行工具在
 * bin\termlet.exe，按上一级的 Termlet.exe 找应用；关于页按程序旁的 licenses 找许可），加上便携标记 portable.txt
 * （有它时数据放在旁边的 data 中）。不放安装程序用的 helper.ps1、记版本的 version.txt，也没有卸载程序与 data。
 * 用系统自带的 tar.exe（bsdtar）打 zip：Git Bash 的 tar 不认 zip。
 */
function buildPortableZip(stage, zip) {
  const tmp = mkdtempSync(join(tmpdir(), 'termlet-portable-'))
  try {
    const dir = join(tmp, PORTABLE_FOLDER)
    mkdirSync(join(dir, 'bin'), { recursive: true })
    copyFileSync(join(stage, 'Termlet.exe'), join(dir, 'Termlet.exe'))
    copyFileSync(join(stage, 'bin/termlet.exe'), join(dir, 'bin/termlet.exe'))
    cpSync(join(stage, 'licenses'), join(dir, 'licenses'), { recursive: true })
    for (const name of LICENSE_AGREEMENTS) copyFileSync(join(stage, name), join(dir, name))
    // 与许可协议一样带 BOM、用 CRLF（老版本的记事本按 BOM 认 UTF-8）
    writeFileSync(
      join(dir, PORTABLE_MARKER),
      windowsText(join(root, 'packaging/windows', PORTABLE_MARKER), true),
    )
    rmSync(zip, { force: true })
    const tar = join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'tar.exe')
    execFileSync(tar, ['--format', 'zip', '-c', '-f', zip, '-C', tmp, PORTABLE_FOLDER], {
      stdio: 'inherit',
    })
  } finally {
    rmSync(tmp, { recursive: true, force: true })
  }
}

/** 本机 Rust 工具链（按 rust-toolchain.toml）的主机目标，例如 x86_64-pc-windows-msvc */
function rustHost() {
  const info = execFileSync('rustc', ['-vV'], { cwd: root, encoding: 'utf8' })
  const host = /^host:\s*(\S+)/m.exec(info)?.[1]
  if (!host) fail('rustc -vV 没有给出 host')
  return host
}

/** --arch 的值（x64、arm64），没给时为 undefined */
function archOption(values) {
  const arch = values.arch
  if (arch !== undefined && !Object.hasOwn(WINDOWS_ARCHES, arch))
    fail(`--arch 应为 ${Object.keys(WINDOWS_ARCHES).join(' 或 ')}：${arch}`)
  return arch
}

function build(values) {
  if (process.platform !== 'win32') fail('目前只打 Windows 安装包，请在 Windows 上运行')
  const version = workspaceVersion()
  const numeric = /^(\d+)\.(\d+)\.(\d+)/.exec(version)
  if (!numeric) fail(`版本号 ${version} 不是 x.y.z 的形式`)
  let arch = archOption(values)
  let stage
  let out
  if (values.stage) {
    stage = resolve(values.stage)
    out = resolve(values.out ?? dirname(stage))
    const staged = checkStage(stage, version)
    if (arch && arch !== staged) fail(`暂存目录中是 ${staged} 的程序，与 --arch ${arch} 不同`)
    arch = staged
  } else {
    // 没给 --arch 时按本机工具链；与工具链的架构不同时交叉编译（产物在 target/<目标>/release）
    const host = rustHost()
    arch ??= Object.keys(WINDOWS_ARCHES).find((a) => WINDOWS_ARCHES[a].target === host) ?? 'x64'
    const { target: triple } = WINDOWS_ARCHES[arch]
    const cross = triple !== host
    if (!values['skip-cargo']) {
      // --locked：发布构建不改动 Cargo.lock
      execFileSync(
        'cargo',
        [
          'build',
          '--release',
          '--locked',
          '-p',
          'termlet-desktop',
          '-p',
          'termlet-cli',
          ...(cross ? ['--target', triple] : []),
        ],
        {
          cwd: root,
          stdio: 'inherit',
        },
      )
    }
    const target = cargoTargetDir()
    const bin = join(target, cross ? triple : '', 'release')
    const app = join(bin, 'termlet-desktop.exe')
    const cli = join(bin, 'termlet.exe')
    for (const f of [app, cli])
      if (!existsSync(f)) fail(`没有发布构建的 ${f}（去掉 --skip-cargo 重新构建）`)
    checkAppKeys(app)
    for (const f of [app, cli]) checkWindowsArch(f, arch)

    out = resolve(values.out ?? join(target, 'dist'))
    stage = join(out, WINDOWS_ARCHES[arch].stage)
    rmSync(stage, { recursive: true, force: true })
    mkdirSync(join(stage, 'bin'), { recursive: true })
    copyFileSync(app, join(stage, 'Termlet.exe'))
    copyFileSync(cli, join(stage, 'bin/termlet.exe'))
    writeNotices(stage, triple, true)
    writeFileSync(
      join(stage, 'helper.ps1'),
      windowsText(join(root, 'packaging/windows/helper.ps1'), false),
    )
    writeFileSync(join(stage, STAGE_VERSION), `${version}\n`)
  }
  const platform = `win-${arch}`

  const makensis = findMakensis()
  if (!makensis) {
    fail(
      '找不到 makensis：安装 NSIS 3（https://nsis.sourceforge.io，或 scoop install nsis），' +
        '或用环境变量 MAKENSIS 指定 makensis.exe 的路径',
    )
  }
  const installerName = `termlet-${version}-${platform}.exe`
  const installer = join(out, installerName)
  rmSync(installer, { force: true })
  execFileSync(
    makensis,
    [
      '/V2',
      '/INPUTCHARSET',
      'UTF8',
      `/DVERSION=${version}`,
      `/DVERSION_NUMERIC=${numeric[1]}.${numeric[2]}.${numeric[3]}.0`,
      `/DARCH=${arch}`,
      `/DSTAGE=${stage}`,
      `/DOUTFILE=${installer}`,
      NSIS_SCRIPT,
    ],
    { stdio: 'inherit' },
  )
  // 便携版与安装包用同一个暂存目录（以后在本机签名程序后 build --stage，两者一起换成签过名的程序）
  const portableName = `termlet-${version}-${platform}-portable.zip`
  const portable = join(out, portableName)
  buildPortableZip(stage, portable)

  const manifest = writeManifest([installer, portable], out)

  console.log(`\n已生成（${out}）：`)
  console.log(`  ${installerName}  ${formatSize(statSync(installer).size)}`)
  console.log(`  ${portableName}  ${formatSize(statSync(portable).size)}`)
  console.log(`  ${manifest}`)
  const exeSize = (f) => formatSize(statSync(join(stage, f)).size)
  console.log(`  Termlet.exe ${exeSize('Termlet.exe')}，termlet.exe ${exeSize('bin/termlet.exe')}`)
  const dirArg = values.out || values.stage ? ` --dir ${out}` : ''
  console.log(`下一步：node scripts/release.mjs sign --key <在用的密钥文件>${dirArg}`)
}

// ---------------------------------------------------------------- 下载 CI 的产物

/** 运行 gh（GitHub CLI），返回标准输出 */
function gh(args) {
  try {
    return execFileSync('gh', args, {
      cwd: root,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
      stdio: ['ignore', 'pipe', 'inherit'],
    })
  } catch {
    fail(`gh ${args.slice(0, 2).join(' ')} 失败：要先安装 GitHub CLI 并 gh auth login，账号要能读取仓库`)
  }
}

/** GitHub 的 REST 接口（经 gh，用它登录的账号），返回解析后的 JSON */
function ghApi(path) {
  return JSON.parse(gh(['api', path]))
}

/** release.yml 的一次运行（REST 的运行记录）：按标签找推送这个标签触发的最近一次，或按运行 ID */
function findReleaseRun(values, repo) {
  if (values.run) {
    if (!/^\d+$/.test(values.run)) fail(`运行 ID 应为数字：${values.run}`)
    return ghApi(`repos/${repo}/actions/runs/${values.run}`)
  }
  const query = new URLSearchParams({ branch: values.tag, event: 'push', per_page: '1' })
  const runs = ghApi(`repos/${repo}/actions/workflows/${CI_WORKFLOW}/runs?${query}`).workflow_runs
  if (!runs?.length) fail(`${repo} 中没有推送标签 ${values.tag} 触发的 ${CI_WORKFLOW} 运行`)
  return runs[0]
}

/**
 * 核对这次运行可以拿来发布：成功结束；用的工作流是 release.yml，调用的可复用工作流（windows.yml、sshd.yml）取自
 * 同一个提交；按标签找时，还要是推送这个标签触发的、构建的是本机这个标签的提交。GitHub 按触发的提交取工作流文件，
 * 所以运行用的就是本机这个标签中的 release.yml。
 */
function checkReleaseRun(run, values, repo) {
  if (run.status !== 'completed') fail(`这次运行还没结束（${run.status}）：${run.html_url}`)
  if (run.conclusion !== 'success') fail(`这次运行没有成功（${run.conclusion}）：${run.html_url}`)
  // path 形如 .github/workflows/release.yml（可能带 @<ref>）
  if (run.path?.split('@')[0] !== CI_WORKFLOW_PATH)
    fail(`运行 ${run.id} 用的工作流不是 ${CI_WORKFLOW_PATH}（${run.path}）：${run.html_url}`)
  // 调用的工作流：path 形如 termlet/termlet/.github/workflows/windows.yml@<ref>
  for (const called of run.referenced_workflows ?? []) {
    if (!called.path.startsWith(`${repo}/.github/workflows/`) || called.sha !== run.head_sha) {
      fail(`这次运行调用的工作流 ${called.path}（${called.sha}）不是构建的提交 ${run.head_sha} 中的`)
    }
  }
  if (!values.tag) {
    console.log(
      `注意：--run 不核对触发方式与标签（这次是 ${run.event}，${run.head_branch}），也不核对本机的提交；` +
        `构建的提交是 ${run.head_sha}`,
    )
    return
  }
  if (run.event !== 'push' || run.head_branch !== values.tag)
    fail(`运行 ${run.id} 不是推送标签 ${values.tag} 触发的（${run.event}，${run.head_branch}）`)
  // 签名、发布的应当是本机这个标签的代码构建出来的
  let local = ''
  try {
    local = execFileSync('git', ['rev-parse', `${values.tag}^{commit}`], {
      cwd: root,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim()
  } catch {
    fail(`本机没有标签 ${values.tag}`)
  }
  if (local !== run.head_sha)
    fail(`CI 构建的提交 ${run.head_sha} 不是本机标签 ${values.tag} 的提交 ${local}`)
}

/** 解开产物的 zip：Windows 上用系统自带的 bsdtar（Git Bash 的 tar 不认 zip），其他系统用 unzip。都拒绝 .. 路径 */
function extractZip(zip, dir) {
  mkdirSync(dir, { recursive: true })
  if (process.platform === 'win32') {
    const tar = join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'tar.exe')
    execFileSync(tar, ['-xf', zip, '-C', dir], { stdio: 'inherit' })
  } else {
    execFileSync('unzip', ['-q', zip, '-d', dir], { stdio: 'inherit' })
  }
}

/**
 * 这次运行要下载的产物：必需的都要有；实验版（arm64）的缺了（它的 job 失败或被跳过）时跳过，加 --require-arm64 时
 * 也必需。返回 { chosen: [{ spec, artifact }], skipped: [spec] }；缺了必需的时抛出。
 */
function chooseArtifacts(listed, values) {
  const byName = new Map(listed.map((a) => [a.name, a]))
  const chosen = []
  const skipped = []
  for (const spec of CI_ARTIFACTS) {
    const artifact = byName.get(spec.name)
    if (artifact) chosen.push({ spec, artifact })
    else if (spec.experimental && !values['require-arm64']) skipped.push(spec)
    else {
      const why = spec.experimental ? '（加了 --require-arm64）' : ''
      throw new Error(`这次运行没有产物 ${spec.name}${why}`)
    }
  }
  return { chosen, skipped }
}

/** 跳过的实验版产物：明确警告这一版不出它们的清单与包 */
function warnSkipped(skipped) {
  for (const spec of skipped) {
    console.log(
      `警告：这次运行没有 ${spec.name} 的产物（arm64 是实验版，它的 job 失败或被跳过时不挡发布）：` +
        `这一版不出 ${spec.platform} 的包与 ${manifestName(spec.platform)}，下载站上原有的这份清单不动，` +
        `${spec.platform} 的客户端停在旧版本。要求 arm64 时加 --require-arm64`,
    )
  }
}

/**
 * 下载并核对产物：每个产物的 zip 与 GitHub 记录的 SHA-256 摘要（upload-artifact v4 起有）一致才解开，各平台的内容
 * 合进 dir。出错时抛出（调用方清理临时目录）。
 */
function downloadArtifacts(chosen, repo, tmp, dir) {
  for (const { spec, artifact } of chosen) {
    const { name } = spec
    if (artifact.expired) throw new Error(`产物 ${name} 已过期（保留 14 天），要重新运行`)
    const expected = /^sha256:([0-9a-f]{64})$/.exec(artifact.digest ?? '')?.[1]
    if (!expected) throw new Error(`产物 ${name} 没有 SHA-256 摘要（${artifact.digest}）`)
    const zip = join(tmp, `${name}.zip`)
    const fd = openSync(zip, 'w')
    try {
      execFileSync('gh', ['api', `repos/${repo}/actions/artifacts/${artifact.id}/zip`], {
        stdio: ['ignore', fd, 'inherit'],
      })
    } finally {
      closeSync(fd)
    }
    const actual = sha(zip, 'sha256', 'hex')
    if (actual !== expected) throw new Error(`产物 ${name} 的 SHA-256 是 ${actual}，GitHub 记录的是 ${expected}`)
    const extracted = join(tmp, name)
    extractZip(zip, extracted)
    cpSync(extracted, dir, { recursive: true, force: false, errorOnExist: true })
    console.log(`${name}：SHA-256 与 GitHub 记录的一致`)
  }
}

/**
 * 下载的内容齐全：取到的平台（platforms，如 win-x64、linux-arm64）在 PACKAGES 中的包都在，它们的清单列出的正是
 * 该列的包（顺序相同），版本都与 latest.yml 的相同（按标签找时还要与标签一致）；取到的 Windows 各架构的暂存目录
 * （版本相同、程序是这个架构的）。没取到的平台（跳过的 arm64）不该有它的包与清单。返回版本号。包的哈希由 sign 核对。
 */
function checkFetched(dir, values, platforms) {
  const manifest = join(dir, MANIFEST)
  if (!existsSync(manifest)) throw new Error(`产物中没有 ${MANIFEST}`)
  const version = manifestVersion(readFileSync(manifest, 'utf8'))
  if (values.tag && version !== values.tag.slice(1))
    throw new Error(`${MANIFEST} 的版本 ${version} 与标签 ${values.tag} 不一致`)
  // 每份清单该列的包
  const expected = new Map()
  for (const suffix of PACKAGES) {
    const pkg = `termlet-${version}-${suffix}`
    const platform = PACKAGE_NAME.exec(pkg)[2]
    const present = existsSync(join(dir, pkg))
    if (!platforms.includes(platform)) {
      if (present) throw new Error(`产物中有 ${pkg}，但没有取它那个平台的产物`)
      continue
    }
    if (!present) throw new Error(`产物中没有 ${pkg}`)
    const name = manifestName(platform)
    expected.set(name, [...(expected.get(name) ?? []), pkg])
  }
  for (const [name, pkgs] of expected) {
    if (!existsSync(join(dir, name))) throw new Error(`产物中没有 ${name}`)
    const text = readFileSync(join(dir, name), 'utf8')
    if (manifestVersion(text) !== version)
      throw new Error(`${name} 的版本 ${manifestVersion(text)} 与 ${MANIFEST} 的 ${version} 不一致`)
    const files = manifestFiles(text).map((f) => f.url)
    if (files.join('\n') !== pkgs.join('\n'))
      throw new Error(`${name} 列出的是 ${files.join('、') || '（没有文件）'}，应为 ${pkgs.join('、')}`)
  }
  // 取到的 Windows 各架构的暂存目录：版本相同，其中的程序是这个架构的
  for (const [arch, { stage }] of Object.entries(WINDOWS_ARCHES)) {
    if (!platforms.includes(`win-${arch}`)) continue
    for (const f of [`${stage}/Termlet.exe`, `${stage}/${STAGE_VERSION}`]) {
      if (!existsSync(join(dir, f))) throw new Error(`产物中没有 ${f}`)
    }
    const staged = readFileSync(join(dir, stage, STAGE_VERSION), 'utf8').trim()
    if (staged !== version)
      throw new Error(`${stage} 的版本 ${staged} 与 ${MANIFEST} 的 ${version} 不一致`)
    const actual = readWindowsArch(join(dir, stage, 'Termlet.exe'))
    if (actual !== arch) throw new Error(`${stage}/Termlet.exe 是 ${actual} 的程序，应为 ${arch}`)
  }
  return version
}

async function fetchRelease(values) {
  if (!values.tag === !values.run) fail('用 --tag v<版本> 或 --run <运行 ID> 指定要下载的运行（二选一）')
  if (values.tag && !/^v\d+\.\d+\.\d+/.test(values.tag))
    fail(`标签应为 v<版本>，例如 v0.1.0：${values.tag}`)
  const repo = values.repo ?? REPO
  const run = findReleaseRun(values, repo)
  checkReleaseRun(run, values, repo)
  const name = values.tag ?? `run-${run.id}`
  const out = resolve(values.out ?? join(cargoTargetDir(), 'dist', name))
  if (existsSync(out) && readdirSync(out).length > 0)
    fail(`${out} 已存在且不是空的：换一个 --out，或先删掉它`)

  const listed = ghApi(`repos/${repo}/actions/runs/${run.id}/artifacts?per_page=100`).artifacts
  let choice
  try {
    choice = chooseArtifacts(listed, values)
  } catch (e) {
    fail(`${e.message}：${run.html_url}`)
  }
  warnSkipped(choice.skipped)

  // 先在临时目录中下载、核对、合并，全部通过才写到输出目录
  const tmp = mkdtempSync(join(tmpdir(), 'termlet-fetch-'))
  const merged = join(tmp, 'merged')
  let version = ''
  let error = null
  try {
    downloadArtifacts(choice.chosen, repo, tmp, merged)
    const platforms = choice.chosen.map((c) => c.spec.platform)
    version = checkFetched(merged, values, platforms)
    cpSync(merged, out, { recursive: true })
  } catch (e) {
    error = e
  }
  rmSync(tmp, { recursive: true, force: true })
  if (error) fail(`${error.message}：${run.html_url}`)

  console.log(`已下载 ${run.html_url} 的产物（版本 ${version}）到 ${out}：`)
  for (const f of readdirSync(out).sort()) console.log(`  ${f}`)
  if (choice.skipped.length > 0) {
    console.log(`这一版没有：${choice.skipped.map((s) => s.name).join('、')}（见上面的警告）`)
  }
  console.log(`下一步：node scripts/release.mjs sign --key <在用的密钥文件> --dir ${out}`)
}

/**
 * --digest <产物名>=<SHA-256>（可以给多次）：构建 job 经 job 的输出报告的产物摘要（upload-artifact 的 artifact-digest，
 * 产物 zip 的 SHA-256）。值为空的（那个 job 没有上传，例如失败的 arm64）当作没有报告。返回 Map<产物名, 十六进制摘要>
 */
function reportedDigests(values) {
  const digests = new Map()
  for (const item of values.digest ?? []) {
    const m = /^([a-z0-9-]+)=(?:sha256:)?([0-9a-f]{64})?$/.exec(item)
    if (!m) fail(`--digest 应为 <产物名>=<SHA-256 十六进制>：${item}`)
    const [, name, hex] = m
    if (!CI_ARTIFACTS.some((a) => a.name === name)) fail(`--digest 中的 ${name} 不是 release.yml 的产物`)
    if (digests.has(name)) fail(`--digest 给了两次 ${name}`)
    if (hex) digests.set(name, hex)
  }
  return digests
}

/**
 * CI 的签名与发布 job 用，在碰任何秘密之前运行：actions/download-artifact 以 skip-decompress 把这次运行的产物原样（zip）
 * 下载到 <from>/<产物名>/（它自己也按 GitHub 记录的摘要核对）。这里先用构建 job 报告的摘要（--digest）核对每个 zip 的
 * SHA-256：下载到的产物都要有报告的摘要且一致，报告了摘要的产物都要下载到；再按 CI_ARTIFACTS 取各平台（arm64 缺了时
 * 警告后跳过，见 fetch），解开、合进 out，做与 fetch 相同的内容核对。核对不过时报错，不写出文件。
 */
function collect(values) {
  const usage =
    '用法：collect --from <产物目录> --out <目录> --digest <产物名>=<SHA-256>... [--tag v<版本>] [--require-arm64]'
  if (!values.from || !values.out || !values.digest?.length) fail(usage)
  if (values.tag && !/^v\d+\.\d+\.\d+/.test(values.tag))
    fail(`标签应为 v<版本>，例如 v0.1.0：${values.tag}`)
  const from = resolve(values.from)
  const out = resolve(values.out)
  if (!existsSync(from)) fail(`目录不存在：${from}`)
  if (existsSync(out) && readdirSync(out).length > 0)
    fail(`${out} 已存在且不是空的：换一个 --out，或先删掉它`)
  const digests = reportedDigests(values)
  const listed = readdirSync(from, { withFileTypes: true })
    .filter((d) => d.isDirectory())
    .map((d) => ({ name: d.name }))
  const known = new Set(CI_ARTIFACTS.map((a) => a.name))
  for (const a of listed) {
    if (!known.has(a.name)) console.log(`注意：${from} 中的 ${a.name} 不是 release.yml 的产物，不用它`)
  }
  for (const name of digests.keys()) {
    if (!listed.some((a) => a.name === name)) fail(`构建 job 报告上传了 ${name}，下载的产物中却没有它`)
  }
  let choice
  try {
    choice = chooseArtifacts(listed, values)
  } catch (e) {
    fail(`${e.message}（${from} 中有：${listed.map((a) => a.name).join('、') || '（没有）'}）`)
  }
  warnSkipped(choice.skipped)

  const tmp = mkdtempSync(join(tmpdir(), 'termlet-collect-'))
  const merged = join(tmp, 'merged')
  let version = ''
  let error = null
  try {
    for (const { spec } of choice.chosen) {
      const dir = join(from, spec.name)
      const files = readdirSync(dir, { withFileTypes: true })
      if (files.length !== 1 || !files[0].isFile()) {
        throw new Error(`${dir} 中应只有产物的 zip 一个文件（download-artifact 要加 skip-decompress）`)
      }
      const expected = digests.get(spec.name)
      if (!expected) throw new Error(`构建 job 没有报告 ${spec.name} 的摘要，不用它`)
      const actual = sha(join(dir, files[0].name), 'sha256', 'hex')
      if (actual !== expected) {
        throw new Error(`${spec.name} 的 SHA-256 是 ${actual}，构建 job 报告的是 ${expected}`)
      }
      const extracted = join(tmp, spec.name)
      extractZip(join(dir, files[0].name), extracted)
      cpSync(extracted, merged, { recursive: true, force: false, errorOnExist: true })
      console.log(`${spec.name}：SHA-256 与构建 job 报告的一致`)
    }
    version = checkFetched(
      merged,
      values,
      choice.chosen.map((c) => c.spec.platform),
    )
    cpSync(merged, out, { recursive: true })
  } catch (e) {
    error = e
  }
  rmSync(tmp, { recursive: true, force: true })
  if (error) fail(error.message)

  console.log(`已合并 ${choice.chosen.map((c) => c.spec.name).join('、')} 的产物（版本 ${version}）到 ${out}：`)
  for (const f of readdirSync(out).sort()) console.log(`  ${f}`)
}

// ---------------------------------------------------------------- 签名

/** 版本描述文件中 files 列出的文件（url 与 sha512） */
function manifestFiles(text) {
  const files = []
  let current = null
  for (const line of text.split(/\r?\n/)) {
    const url = /^\s+-\s+url:\s*['"]?([^'"]+?)['"]?\s*$/.exec(line)
    if (url) {
      current = { url: url[1], sha512: '' }
      files.push(current)
      continue
    }
    const hash = /^\s+sha512:\s*['"]?([^'"\s]+)/.exec(line)
    if (hash && current && !current.sha512) current.sha512 = hash[1]
  }
  return files
}

function manifestVersion(text) {
  return /^version:[ \t]*['"]?([^'"\s]+)/m.exec(text)?.[1]
}

async function signRelease(values) {
  const dir = resolve(values.dir ?? join(cargoTargetDir(), 'dist'))
  if (!existsSync(dir)) fail(`目录不存在：${dir}`)
  const manifests = readdirSync(dir)
    .filter((f) => /^latest.*\.yml$/.test(f))
    .sort()
  // 每份 Windows 清单对应的暂存目录中的程序（latest.yml → stage，latest-arm64.yml → stage-arm64）：是那个架构的
  // 程序，签名前再核对它内置了这把公钥。没有 Windows 的清单时照旧要 stage（只有安装包时加 --no-app-check）
  const apps = []
  if (!values['no-app-check']) {
    const archs = Object.keys(WINDOWS_ARCHES).filter((a) =>
      manifests.includes(manifestName(`win-${a}`)),
    )
    for (const arch of archs.length > 0 ? archs : ['x64']) {
      const app = join(dir, WINDOWS_ARCHES[arch].stage, 'Termlet.exe')
      if (!existsSync(app))
        fail(`${app} 不存在，无法确认程序内置了公钥（只有安装包时加 --no-app-check）`)
      checkWindowsArch(app, arch)
      apps.push(app)
    }
  }
  if (manifests.length === 0) fail(`${dir} 中没有 latest*.yml（先运行 build）`)
  checkArm64Manifests(manifests, values)
  const channels = readdirSync(dir).filter((f) => /^(alpha|beta)\b.*\.yml$/.test(f))
  if (channels.length > 0)
    fail(`${channels.join('、')} 不会被应用读取（应用只认 latest*.yml），删除后重试`)
  const versions = new Set()
  const texts = new Map()
  for (const name of manifests) {
    const text = readFileSync(join(dir, name), 'utf8')
    const version = manifestVersion(text)
    if (!version) fail(`${name} 中没有版本号`)
    versions.add(version)
    // 清单中列出的文件要在目录中、哈希一致，免得给过时的清单签名
    const files = manifestFiles(text)
    if (files.length === 0) fail(`${name} 中没有文件`)
    for (const f of files) {
      const path = join(dir, f.url)
      if (!existsSync(path)) fail(`${name} 中的 ${f.url} 不在目录中`)
      if (sha(path, 'sha512', 'base64') !== f.sha512)
        fail(`${f.url} 与 ${name} 中的 SHA-512 不一致`)
    }
    texts.set(name, text)
  }
  if (versions.size !== 1) fail(`各平台清单的版本不一致：${[...versions].join('、')}`)
  const [version] = versions
  // 本版本的每个包都要列在它那个平台的清单中，否则那个平台的应用看不到新版本、官网取不到它
  for (const f of readdirSync(dir)) {
    const m = PACKAGE_NAME.exec(f)
    if (!m || m[1] !== version) continue
    const name = manifestName(m[2])
    const listed = texts.has(name) && manifestFiles(texts.get(name)).some((e) => e.url === f)
    if (!listed) {
      fail(`${f} 没有列在 ${name} 中：先运行 node scripts/release.mjs manifest，给出这个平台的全部包`)
    }
  }

  const { data, keys, secret } = await loadSigningKey(values)
  for (const app of apps) {
    if (!readFileSync(app).includes(Buffer.from(data.publicKey))) {
      fail(`${relative(dir, app)} 没有内置这把公钥：确认 keys.rs 后重新构建`)
    }
  }
  const written = []
  for (const [name, text] of texts) {
    const comment = manifestComment(name, version)
    const sig = createSignature(text, secret, comment)
    if (verifySignature(text, sig, keys) !== comment) fail(`${name} 的签名自检失败`)
    writeFileSync(join(dir, `${name}.minisig`), sig)
    written.push(`${name}.minisig`)
  }

  // SHA256SUMS：本版本的全部包（安装包、便携版、Linux 与 macOS 的包）与清单，供手动核对（应用不读取）
  const assets = readdirSync(dir)
    .filter((f) => PACKAGE_NAME.exec(f)?.[1] === version || texts.has(f))
    .sort()
  const sums = assets.map((f) => `${sha(join(dir, f), 'sha256', 'hex')}  ${f}\n`).join('')
  const sumsSig = createSignature(sums, secret, checksumsComment(version))
  if (verifySignature(sums, sumsSig, keys) !== checksumsComment(version))
    fail('SHA256SUMS 的签名自检失败')
  writeFileSync(join(dir, 'SHA256SUMS'), sums)
  writeFileSync(join(dir, 'SHA256SUMS.minisig'), sumsSig)
  written.push('SHA256SUMS', 'SHA256SUMS.minisig')

  console.log(`已用 ${data.name}（${data.keyId}）签名版本 ${version}：${written.join('、')}`)
  console.log('要发布的文件：')
  for (const f of [...assets, ...written].filter((f, i, a) => a.indexOf(f) === i))
    console.log(`  ${f}`)
  if (version.includes('-')) {
    // 备用的更新源是 releases/latest/download，GitHub 的“最新版本”不包括预发布
    console.log(
      `注意：${version} 是测试版。publish 照常把它标为 GitHub 上的最新版本，以后在网页上编辑这个 Release 时不要勾选 ` +
        '"Set as a pre-release"，否则经 GitHub 检查更新的取不到它',
    )
  }
  console.log(`下一步：node scripts/release.mjs publish --dir ${dir} --dry-run`)
}

async function signConfig(values, path) {
  if (!path)
    fail('用法：node scripts/release.mjs sign-config --key <密钥文件> [--days 90] <update.json>')
  const days = Number(values.days ?? CONFIG_DAYS.default)
  if (!Number.isInteger(days) || days < 1 || days > CONFIG_DAYS.max) {
    fail(`--days 应为 1 到 ${CONFIG_DAYS.max} 的整数`)
  }
  const file = resolve(path)
  if (!existsSync(file)) fail(`找不到 ${file}`)
  const text = readFileSync(file, 'utf8')
  // 签名覆盖原样的字节：CRLF 提交到 Git 后可能被改成 LF，签名就对不上了
  if (text.includes('\r')) fail('请用 LF 换行（提交到 Git 时 CRLF 可能被转换，签名会失效）')
  let parsed
  try {
    parsed = JSON.parse(text)
  } catch {
    fail(`${file} 不是有效的 JSON`)
  }
  if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed))
    fail('远程配置应为 JSON 对象')
  const { keys, secret } = await loadSigningKey(values)
  // 签名中的时间精确到秒
  const issued = new Date(Math.floor(Date.now() / 1000) * 1000)
  const expires = new Date(issued.getTime() + days * 24 * 60 * 60 * 1000)
  const comment = remoteConfigComment(issued, expires)
  const sig = createSignature(text, secret, comment)
  if (verifySignature(text, sig, keys) !== comment) fail('签名自检失败')
  writeFileSync(`${file}.minisig`, sig)
  console.log(`已签名：${file}.minisig（与 ${file} 一起上传，内容不能再改动）`)
  console.log(
    `有效期到 ${isoSeconds(expires)}：到期前重新签名，过期后应用不再采用这份配置（改用内置的更新源）`,
  )
  console.log(`下一步：node scripts/release.mjs publish-config ${path}`)
}

// ---------------------------------------------------------------- 版本号与更新日志

/** 比较两个版本号（语义化版本：没有后缀的比带 -beta.1 这类后缀的新），a 较新时为正、相同时为 0 */
function compareVersions(a, b) {
  const parse = (v) => {
    const [core, pre] = v.split(/-(.*)/s)
    return { nums: core.split('.').map(Number), pre: pre ? pre.split('.') : [] }
  }
  const x = parse(a)
  const y = parse(b)
  for (let i = 0; i < 3; i++) if (x.nums[i] !== y.nums[i]) return x.nums[i] - y.nums[i]
  if (x.pre.length === 0 || y.pre.length === 0) return y.pre.length - x.pre.length
  for (let i = 0; i < Math.max(x.pre.length, y.pre.length); i++) {
    if (i >= x.pre.length) return -1
    if (i >= y.pre.length) return 1
    const [p, q] = [x.pre[i], y.pre[i]]
    if (p === q) continue
    const [pn, qn] = [/^\d+$/.test(p), /^\d+$/.test(q)]
    if (pn && qn) return Number(p) - Number(q)
    if (pn !== qn) return pn ? -1 : 1
    return p < q ? -1 : 1
  }
  return 0
}

/** YYYY-MM-DD，并且是真实存在的一天 */
function isDate(s) {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(s ?? '')) return false
  const time = Date.parse(`${s}T00:00:00Z`)
  return !Number.isNaN(time) && new Date(time).toISOString().slice(0, 10) === s
}

/** 本机时区的今天（YYYY-MM-DD） */
function localToday() {
  const d = new Date()
  const pad = (n) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`
}

/**
 * 读一份更新日志：各个版本标题（“## ” 开头的行，代码块中的不算）所在的行、版本、括号中的日期或“未发布”与正文
 * （标题的下一行到下一个版本标题之前，去掉首尾空行）。标题写法不对的 version 为 undefined。
 */
function readChangelog(spec) {
  const path = join(root, spec.name)
  if (!existsSync(path)) fail(`找不到更新日志 ${spec.name}`)
  const raw = readFileSync(path, 'utf8')
  const bom = raw.startsWith('\uFEFF')
  const eol = raw.includes('\r\n') ? '\r\n' : '\n'
  const lines = raw.replace(/^\uFEFF/, '').split(/\r?\n/)
  const sections = []
  let fence = null
  for (const [index, line] of lines.entries()) {
    if (fence) {
      const close = /^ {0,3}(`{3,}|~{3,})[ \t]*$/.exec(line)?.[1]
      if (close && close[0] === fence[0] && close.length >= fence.length) fence = null
      continue
    }
    const open = /^ {0,3}(`{3,}|~{3,})/.exec(line)?.[1]
    if (open) fence = open
    else if (/^##[ \t]/.test(line)) {
      const m = CHANGELOG_HEADING.exec(line)
      sections.push({ index, title: line.replace(/^##[ \t]+/, '').trim(), version: m?.[1], note: m?.[2] })
    }
  }
  for (const [i, s] of sections.entries()) {
    const end = sections[i + 1]?.index ?? lines.length
    s.body = lines
      .slice(s.index + 1, end)
      .join('\n')
      .trim()
  }
  return { path, bom, eol, lines, sections }
}

/**
 * 这个版本在两份更新日志中的一节：标题要是这个版本与发布日期（还是“未发布”、缺了、没有正文时报错），两份的日期相同。
 * 返回 [中文, 英文]，各为 { name, date, title, body }。
 */
function changelogEntries(version) {
  const entries = CHANGELOGS.map((spec) => {
    const s = readChangelog(spec).sections.find((x) => x.version === version)
    if (!s) {
      fail(`${spec.name} 中没有 ${version} 这一节（标题应为“${spec.title(version, 'YYYY-MM-DD')}”）`)
    }
    if (UNRELEASED.test(s.note)) {
      fail(
        `${spec.name} 中 ${version} 这一节还是“${s.title}”：发版前先运行 node scripts/release.mjs prepare ${version}` +
          '（把它改成版本号与日期），提交后再打标签',
      )
    }
    if (!isDate(s.note)) fail(`${spec.name} 中“${s.title}”的日期应为 YYYY-MM-DD`)
    if (!s.body) fail(`${spec.name} 中 ${version} 这一节没有内容，不发空的 Release 说明`)
    return { name: spec.name, date: s.note, title: s.title, body: s.body }
  })
  const [zh, en] = entries
  if (zh.date !== en.date) {
    fail(`两份更新日志中 ${version} 的日期不同：${zh.name} 是 ${zh.date}，${en.name} 是 ${en.date}`)
  }
  return entries
}

/**
 * GitHub Release 的说明：更新日志中这一版的中文一节、核对下载的方法，分隔线，英文一节与核对方法。相对链接（在仓库中
 * 写的 LICENSE、README.md#… 这类）改成这个标签下的仓库地址。provenance 为真时（CI 中签名时生成了构建来源证明）
 * 另写一句 gh attestation verify 的用法。
 */
function releaseNotes(version, { provenance }) {
  const [zh, en] = changelogEntries(version)
  const blob = `https://github.com/${REPO}/blob/v${version}`
  const absolute = (body) =>
    body.replace(
      /\]\((?![a-z][a-z0-9+.-]*:|#|\/)([^)\s]+)\)/gi,
      (_, target) => `](${blob}/${target.replace(/^\.\//, '')})`,
    )
  const sample = `termlet-${version}-win-x64.exe`
  return [
    '## 更新内容',
    '',
    absolute(zh.body),
    '',
    '## 核对下载的文件',
    '',
    `- \`SHA256SUMS\` 是这个版本全部文件的 SHA-256，\`SHA256SUMS.minisig\` 是它的签名（minisign 格式）；核对方法与公钥见 README 的[核对下载的文件](${blob}/README.md#核对下载的文件)。`,
    ...(provenance
      ? [
          `- 构建来源证明：这些文件由本仓库的 GitHub Actions 从标签 \`v${version}\` 构建，可以用 GitHub CLI 核对，例如 \`gh attestation verify ${sample} -R ${REPO}\`。`,
        ]
      : []),
    '',
    '---',
    '',
    "## What's changed",
    '',
    absolute(en.body),
    '',
    '## Checking your download',
    '',
    `- \`SHA256SUMS\` lists the SHA-256 of every file in this release, and \`SHA256SUMS.minisig\` is its signature (minisign format); see [Checking your download](${blob}/README.en.md#checking-your-download) in the README for how to check it and the public keys.`,
    ...(provenance
      ? [
          `- Build provenance: these files were built from the tag \`v${version}\` by this repository's GitHub Actions; check with the GitHub CLI, e.g. \`gh attestation verify ${sample} -R ${REPO}\`.`,
        ]
      : []),
    '',
  ].join('\n')
}

/** 版本号：--tag v<版本>（另核对与 Cargo.toml 的版本一致）或直接给出的 <版本> */
function versionArgument(values, positional, usage) {
  if (values.tag && positional) fail(`--tag 与版本号只能给一个：${usage}`)
  const version = values.tag ? values.tag.replace(/^v/, '') : positional?.replace(/^v/, '')
  if (!version || !VERSION.test(version) || (values.tag && !values.tag.startsWith('v'))) {
    fail(`用法：${usage}`)
  }
  if (values.tag) {
    const cargo = workspaceVersion()
    if (cargo !== version) fail(`标签 ${values.tag} 与 Cargo.toml 的版本 ${cargo} 不一致`)
  }
  return version
}

/** notes：核对更新日志中这一版的两节，输出 GitHub Release 的说明（与 CI 中 publish 用的相同） */
function notesCommand(values, positional) {
  const version = versionArgument(values, positional, 'notes (--tag v<版本> | <版本>) [--out <文件>]')
  const text = releaseNotes(version, { provenance: true })
  if (!values.out) {
    process.stdout.write(text)
    return
  }
  const out = resolve(values.out)
  writeFileSync(out, text)
  const [zh, en] = changelogEntries(version)
  console.log(`已写出 ${out}：${zh.name}“${zh.title}”与 ${en.name}“${en.title}”两节`)
}

/** Cargo.toml 中本工作区的各个包（名字与版本） */
function workspacePackages() {
  const meta = JSON.parse(
    execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps'], {
      cwd: root,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    }),
  )
  const members = new Set(meta.workspace_members)
  return meta.packages.filter((p) => members.has(p.id)).map((p) => ({ name: p.name, version: p.version }))
}

/**
 * prepare：发版前的准备。改 Cargo.toml 的版本号与 Cargo.lock 中本工作区的包，把两份更新日志最上面“未发布”的一节
 * 改成这个版本与日期。全部核对通过才写文件；不提交。
 */
function prepare(values, positional) {
  const usage = 'prepare <版本> [--date YYYY-MM-DD] [--dry-run]（例如 prepare 0.4.0）'
  const version = positional?.replace(/^v/, '')
  if (!version || !VERSION.test(version)) fail(`用法：${usage}`)
  const date = values.date ?? localToday()
  if (!isDate(date)) fail(`--date 应为 YYYY-MM-DD：${date}`)
  const current = workspaceVersion()
  if (compareVersions(version, current) < 0) fail(`${version} 比 Cargo.toml 现在的版本 ${current} 旧`)
  const writes = []

  for (const spec of CHANGELOGS) {
    const log = readChangelog(spec)
    const [top] = log.sections
    const wanted = spec.title(version, date)
    const label = wanted.replace(/^## /, '')
    if (!top) fail(`${spec.name} 中没有任何版本`)
    if (top.version === version && top.note === date) {
      console.log(`${spec.name}：最上面一节已是“${label}”，不改`)
      continue
    }
    if (!top.version || !UNRELEASED.test(top.note ?? '')) {
      fail(
        `${spec.name} 最上面一节是“${top.title}”，不是“未发布”：这一版的改动要先写进最上面的` +
          `“${spec.title(version, spec.unreleased).replace(/^## /, '')}”一节`,
      )
    }
    const other = log.sections.slice(1).find((s) => s.version === version)
    if (other) fail(`${spec.name} 中已有 ${version} 一节（“${other.title}”）`)
    if (!top.body) fail(`${spec.name} 中“${top.title}”一节没有内容`)
    if (top.version !== version) {
      console.log(`注意：${spec.name} 中“未发布”一节写的版本是 ${top.version}，改成 ${version}`)
    }
    const lines = [...log.lines]
    lines[top.index] = wanted
    writes.push({
      path: log.path,
      text: `${log.bom ? '\uFEFF' : ''}${lines.join(log.eol)}`,
      what: `${spec.name}：“${top.title}” → “${label}”`,
    })
  }

  if (version !== current) {
    const tomlPath = join(root, 'Cargo.toml')
    const toml = readFileSync(tomlPath, 'utf8')
    // 只改 [workspace.package] 这一节中的 version（与 workspaceVersion 读的是同一处）
    const section = /\[workspace\.package\]([\s\S]*?)(?:\n\[|$)/.exec(toml)
    const start = section ? section.index + section[0].indexOf(section[1]) : -1
    const body = section?.[1] ?? ''
    const bumpedBody = body.replace(/^(version\s*=\s*")([^"]+)(")/m, (_, before, _old, after) => `${before}${version}${after}`)
    if (!section || bumpedBody === body) fail('没有在 Cargo.toml 的 [workspace.package] 中找到 version')
    const bumped = toml.slice(0, start) + bumpedBody + toml.slice(start + body.length)
    writes.push({ path: tomlPath, text: bumped, what: `Cargo.toml：${current} → ${version}` })

    const lockPath = join(root, 'Cargo.lock')
    let lock = readFileSync(lockPath, 'utf8')
    const packages = workspacePackages()
    for (const p of packages) {
      if (p.version !== current) {
        console.log(`注意：${p.name} 的版本 ${p.version} 不随工作区，不改`)
        continue
      }
      const escaped = p.name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
      const entry = new RegExp(`(\\[\\[package\\]\\]\\r?\\nname = "${escaped}"\\r?\\nversion = ")([^"]+)(")`, 'g')
      let count = 0
      lock = lock.replace(entry, (_, before, _old, after) => {
        count += 1
        return `${before}${version}${after}`
      })
      if (count !== 1) fail(`Cargo.lock 中 ${p.name} 出现了 ${count} 次（应为 1 次）`)
    }
    writes.push({ path: lockPath, text: lock, what: `Cargo.lock：本工作区的 ${packages.length} 个包 → ${version}` })
  }

  if (writes.length === 0) {
    console.log(`已经准备好 ${version}（${date}），没有要改的`)
    return
  }
  console.log(values['dry-run'] ? '要改的（--dry-run：没有写入）：' : '已修改：')
  for (const w of writes) console.log(`  ${w.what}`)
  if (values['dry-run']) return
  for (const w of writes) writeFileSync(w.path, w.text)
  if (version !== current) {
    const check = spawnSync('cargo', ['metadata', '--format-version', '1', '--locked'], {
      cwd: root,
      encoding: 'utf8',
      maxBuffer: 256 * 1024 * 1024,
      stdio: ['ignore', 'ignore', 'pipe'],
    })
    if (check.status !== 0) {
      fail(`改过以后 Cargo.lock 与 Cargo.toml 对不上（cargo metadata --locked 失败）：${check.stderr.trim().split('\n').at(-1)}`)
    }
  }
  console.log(
    `下一步：git diff 核对后提交（例如“发布 ${version}”）并推送，CI 通过后打标签推送：` +
      `git tag v${version} && git push origin v${version}`,
  )
}

// ---------------------------------------------------------------- 发布

/** --prefix（测试用）：对象名的前缀；没有时为空 */
function testPrefix(values) {
  const prefix = values.prefix ?? ''
  if (prefix && !TEST_PREFIX.test(prefix))
    fail(`--prefix 只用来测试，要以 termlet-test- 开头（小写字母、数字与 -）：${prefix}`)
  if (values['test-pubkey'] && !prefix) fail('--test-pubkey 只能与 --prefix 一起用')
  return prefix
}

/** 发布前核对签名用的公钥：内置的（keys.rs），测试时另加 --test-pubkey */
function publishKeys(values) {
  const keys = builtinKeys()
  if (keys.length === 0) fail(`${relative(root, KEYS_FILE)} 中没有公钥`)
  if (values['test-pubkey']) {
    parsePublicKey(values['test-pubkey'])
    keys.push(values['test-pubkey'])
  }
  return keys
}

/** 对象的 Content-Type（cf 设不了 Cache-Control：安装包按 CDN 默认的缓存，版本描述文件在 CDN 上另有 60 秒的规则） */
function contentType(name) {
  if (name.endsWith('.yml')) return 'text/yaml; charset=utf-8'
  if (name.endsWith('.json')) return 'application/json; charset=utf-8'
  if (name.endsWith('.minisig') || name === 'SHA256SUMS') return 'text/plain; charset=utf-8'
  if (name.endsWith('.tar.gz')) return 'application/gzip'
  if (name.endsWith('.zip')) return 'application/zip'
  if (name.endsWith('.deb')) return 'application/vnd.debian.binary-package'
  if (name.endsWith('.rpm')) return 'application/x-rpm'
  // .exe 与 .AppImage
  return 'application/octet-stream'
}

/**
 * 目录中签过名、可以发布的版本：每份 latest*.yml 与 SHA256SUMS 都有签名，签名的公钥受信任、可信注释与文件名和
 * 版本一致；SHA256SUMS 中的文件都在、SHA-256 一致；清单列出的安装包都在 SHA256SUMS 中、SHA-512 一致。
 * 返回版本号与按上传顺序排好的文件：安装包与包、SHA256SUMS 与签名、清单的签名、清单。
 */
function checkSignedRelease(dir, keys) {
  const names = readdirSync(dir)
  const manifests = names.filter((f) => /^latest.*\.yml$/.test(f)).sort()
  if (manifests.length === 0) fail(`${dir} 中没有 latest*.yml（先 build 或 fetch，再 sign）`)
  const texts = new Map(manifests.map((m) => [m, readFileSync(join(dir, m), 'utf8')]))
  const versions = new Set([...texts.values()].map(manifestVersion))
  if (versions.size !== 1 || versions.has(undefined))
    fail(`各清单的版本不一致或没有版本号：${[...versions].join('、')}`)
  const [version] = versions
  const checkSigned = (name, comment) => {
    const sigPath = join(dir, `${name}.minisig`)
    if (!existsSync(join(dir, name))) fail(`${dir} 中没有 ${name}（先运行 sign）`)
    if (!existsSync(sigPath)) fail(`${name} 没有签名（先运行 sign）`)
    const what = `${name}.minisig 核对失败`
    const trusted = verifySignature(
      readFileSync(join(dir, name)),
      readFileSync(sigPath, 'utf8'),
      keys,
      what,
    )
    if (trusted !== comment)
      fail(`${what}：可信注释是“${trusted}”，应为“${comment}”（重新运行 sign）`)
  }
  for (const m of manifests) checkSigned(m, manifestComment(m, version))
  checkSigned('SHA256SUMS', checksumsComment(version))

  const sums = new Map()
  for (const line of readFileSync(join(dir, 'SHA256SUMS'), 'utf8').split('\n').filter(Boolean)) {
    const m = /^([0-9a-f]{64}) {2}([A-Za-z0-9._-]+)$/.exec(line)
    if (!m) fail(`SHA256SUMS 中这一行的格式不对：${line}`)
    sums.set(m[2], m[1])
  }
  for (const [name, hash] of sums) {
    const path = join(dir, name)
    if (!existsSync(path)) fail(`SHA256SUMS 中的 ${name} 不在目录中`)
    if (sha(path, 'sha256', 'hex') !== hash)
      fail(`${name} 与 SHA256SUMS 中的 SHA-256 不一致（重新运行 sign）`)
  }
  for (const [m, text] of texts) {
    if (!sums.has(m)) fail(`SHA256SUMS 中没有 ${m}（重新运行 sign）`)
    for (const f of manifestFiles(text)) {
      if (!sums.has(f.url)) fail(`${m} 中的 ${f.url} 不在 SHA256SUMS 中`)
      if (sha(join(dir, f.url), 'sha512', 'base64') !== f.sha512)
        fail(`${f.url} 与 ${m} 中的 SHA-512 不一致`)
    }
  }
  const packages = [...sums.keys()].filter((f) => !texts.has(f))
  // Windows 的安装包最先传
  packages.sort(
    (a, b) => Number(!a.endsWith('.exe')) - Number(!b.endsWith('.exe')) || a.localeCompare(b),
  )
  const order = [
    ...packages,
    'SHA256SUMS',
    'SHA256SUMS.minisig',
    ...manifests.map((m) => `${m}.minisig`),
    ...manifests,
  ]
  return { version, files: order.map((name) => ({ name, path: join(dir, name) })) }
}

/** cf 运行的方式：npm 装的 cf 在 Windows 上是 cf.cmd，Node 不经 shell 运行不了 .cmd，改用 node 运行它的入口脚本 */
let cfInvocation = null
function cfCommand() {
  if (cfInvocation) return cfInvocation
  if (process.platform !== 'win32') {
    cfInvocation = { file: 'cf', args: [] }
    return cfInvocation
  }
  let shim = ''
  try {
    shim = execFileSync('where', ['cf.cmd'], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).split(/\r?\n/)[0]
  } catch {
    // PATH 中没有
  }
  const script = shim && join(dirname(shim.trim()), 'node_modules/cf/bin/cf')
  if (!script || !existsSync(script))
    fail('找不到 Cloudflare 的 cf 命令行：npm install -g cf 后运行 cf auth login')
  cfInvocation = { file: process.execPath, args: [script] }
  return cfInvocation
}

/** cf 的工作目录：它在当前目录写 .cloudflare/（记下选的账号），放在临时目录中，退出时删掉 */
let cfCwd = null

/** 运行 cf，返回解析后的 JSON 输出；失败时报错退出（说明中去掉账号 ID 等长串） */
function cf(args, what) {
  const { file, args: pre } = cfCommand()
  if (!cfCwd) {
    cfCwd = mkdtempSync(join(tmpdir(), 'termlet-cf-'))
    process.on('exit', () => rmSync(cfCwd, { recursive: true, force: true }))
  }
  const r = spawnSync(file, [...pre, ...args], {
    cwd: cfCwd,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  if (r.error) fail(`${what}失败：无法运行 cf（${r.error.message}）`)
  if (r.status !== 0) {
    const detail = `${r.stderr}\n${r.stdout}`
      .split(/\r?\n/)
      .map((l) => l.replace(/^[\s│┌└]+/, '').trim())
      .filter((l) => l && !l.startsWith('Assertion failed'))
      .slice(0, 4)
      .join('；')
      .replace(/[0-9a-f]{32}/g, '<id>')
    fail(`${what}失败：${detail || `cf 退出码 ${r.status}`}（要先 cf auth login）`)
  }
  try {
    return JSON.parse(r.stdout)
  } catch {
    fail(`${what}：cf 的输出不是 JSON`)
  }
}

// ---------------------------------------------------------------- 下载站（R2 与 CDN）

/**
 * R2 的 S3 接口的凭据：R2_ENDPOINT（https://<账号 ID>.r2.cloudflarestorage.com）、R2_ACCESS_KEY_ID、R2_SECRET_ACCESS_KEY
 * 都设了时返回，都没设时为 null（用登录的 cf），只设了一部分时报错。CI 中来自 release 环境的 Secrets：R2 令牌只能读写
 * 这个桶的对象。不打印；读过就从环境中删掉（之后运行的 gh 等子进程拿不到）
 */
function r2Credentials() {
  const endpoint = process.env.R2_ENDPOINT
  const id = takeEnvSecret('R2_ACCESS_KEY_ID')
  const secret = takeEnvSecret('R2_SECRET_ACCESS_KEY')
  const given = [endpoint, id, secret].filter(Boolean).length
  if (given === 0) return null
  if (given < 3) fail('R2_ENDPOINT、R2_ACCESS_KEY_ID、R2_SECRET_ACCESS_KEY 要一起设')
  let url
  try {
    url = new URL(endpoint)
  } catch {
    fail('R2_ENDPOINT 不是有效的地址')
  }
  if (url.protocol !== 'https:' || url.pathname !== '/' || url.search || url.username) {
    fail('R2_ENDPOINT 应为 https://<账号 ID>.r2.cloudflarestorage.com（不带路径与桶名）')
  }
  return { origin: url.origin, host: url.host, id, secret }
}

/**
 * 清 CDN 缓存用 Cloudflare 的接口：CLOUDFLARE_PURGE_TOKEN（只能清 termlet.dev 缓存的 API 令牌）与 CLOUDFLARE_ZONE_ID
 * 都设了时返回，都没设时为 null（用登录的 cf），只设了一个时报错；令牌读过就从环境中删掉
 */
function purgeCredentials() {
  const token = takeEnvSecret('CLOUDFLARE_PURGE_TOKEN')
  const zone = process.env.CLOUDFLARE_ZONE_ID
  if (!token && !zone) return null
  if (!token || !zone) fail('CLOUDFLARE_PURGE_TOKEN 与 CLOUDFLARE_ZONE_ID 要一起设')
  if (!/^[0-9a-f]{32}$/.test(zone)) fail('CLOUDFLARE_ZONE_ID 应为 32 位十六进制的 zone ID')
  return { token, zone }
}

/**
 * 上传与清缓存的方式：环境变量给出了令牌时走 Cloudflare 的接口，否则用登录的 cf。GitHub Actions 中不用 cf，令牌缺了
 * 就报错（在上传任何文件之前调用）
 */
function cloudflareBackend() {
  const s3 = r2Credentials()
  const purge = purgeCredentials()
  if (IN_CI && (!s3 || !purge)) {
    fail(
      'GitHub Actions 中要设 R2_ENDPOINT、R2_ACCESS_KEY_ID、R2_SECRET_ACCESS_KEY、CLOUDFLARE_PURGE_TOKEN 与 ' +
        'CLOUDFLARE_ZONE_ID（release 环境的 Secrets 与变量）',
    )
  }
  const how = `上传用${s3 ? ' R2 的 S3 接口' : '登录的 cf'}，清缓存用${purge ? ' Cloudflare 的接口' : '登录的 cf'}`
  return { s3, purge, how }
}

const sha256Hex = (data) => createHash('sha256').update(data).digest('hex')
const hmacSha256 = (key, data) => createHmac('sha256', key).update(data).digest()
const sleep = (ms) => new Promise((done) => setTimeout(done, ms))

/** S3 的百分号编码：字母、数字与 -._~ 以外的都编码；keepSlash 时路径中的 / 不编码 */
function uriEncode(s, keepSlash = false) {
  const encoded = encodeURIComponent(s).replace(
    /[!'()*]/g,
    (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`,
  )
  return keepSlash ? encoded.replace(/%2F/g, '/') : encoded
}

/** 规范的查询串：键与值编码后按键（相同时按值）排序 */
function canonicalQuery(query) {
  const order = (a, b) => (a < b ? -1 : a > b ? 1 : 0)
  return Object.entries(query)
    .map(([k, v]) => [uriEncode(k), uriEncode(String(v))])
    .sort(([a, x], [b, y]) => order(a, b) || order(x, y))
    .map(([k, v]) => `${k}=${v}`)
    .join('&')
}

/**
 * 请求的签名（AWS Signature Version 4，R2 的 S3 接口用它），返回 Authorization 头。headers 是另外要签进去的头
 * （host 与 x-amz-date 由这里加上），path 是编码过的路径，payloadHash 是正文的 SHA-256（十六进制）。
 * selftest 用 AWS 的签名测试样例核对。
 */
function sigV4({ method, host, path, query = {}, headers = {}, payloadHash, amzDate, region, service, keyId, secret }) {
  const all = { host, 'x-amz-date': amzDate }
  for (const [k, v] of Object.entries(headers)) all[k.toLowerCase()] = v
  const names = Object.keys(all).sort()
  const canonicalHeaders = names
    .map((n) => `${n}:${String(all[n]).trim().replace(/\s+/g, ' ')}\n`)
    .join('')
  const signedHeaders = names.join(';')
  const request = [method, path, canonicalQuery(query), canonicalHeaders, signedHeaders, payloadHash].join('\n')
  const day = amzDate.slice(0, 8)
  const scope = `${day}/${region}/${service}/aws4_request`
  const toSign = ['AWS4-HMAC-SHA256', amzDate, scope, sha256Hex(request)].join('\n')
  let key = hmacSha256(`AWS4${secret}`, day)
  for (const part of [region, service, 'aws4_request']) key = hmacSha256(key, part)
  const signature = createHmac('sha256', key).update(toSign).digest('hex')
  return `AWS4-HMAC-SHA256 Credential=${keyId}/${scope}, SignedHeaders=${signedHeaders}, Signature=${signature}`
}

/**
 * R2 的 S3 接口的一次请求（路径式：/<桶>/<对象名>）。网络错误、5xx 与 429 等几秒重试，最多 3 次；其他错误报错退出，
 * 说明中只有状态码与 R2 返回的错误代码。返回 { res, text }。
 */
async function s3Request(creds, method, key, { query = {}, body, headers = {}, what }) {
  const path = `/${uriEncode(R2_BUCKET)}${key === undefined ? '' : `/${uriEncode(key, true)}`}`
  const payloadHash = sha256Hex(body ?? Buffer.alloc(0))
  const qs = canonicalQuery(query)
  let problem = ''
  for (let attempt = 0; attempt < 3; attempt++) {
    if (attempt > 0) await sleep(3000 * attempt)
    const amzDate = new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d{3}/, '')
    const signed = { ...headers, 'x-amz-content-sha256': payloadHash }
    const authorization = sigV4({
      method,
      host: creds.host,
      path,
      query,
      headers: signed,
      payloadHash,
      amzDate,
      region: 'auto',
      service: 's3',
      keyId: creds.id,
      secret: creds.secret,
    })
    try {
      const res = await fetch(`${creds.origin}${path}${qs ? `?${qs}` : ''}`, {
        method,
        headers: { ...signed, 'x-amz-date': amzDate, authorization },
        body,
        signal: AbortSignal.timeout(10 * 60_000),
      })
      const text = await res.text()
      if (res.ok) return { res, text }
      const code = /<Code>([^<]*)<\/Code>/.exec(text)?.[1]
      problem = `HTTP ${res.status}${code ? ` ${code}` : ''}`
      if (res.status < 500 && res.status !== 429) break
    } catch (e) {
      problem = e.message
    }
  }
  fail(`${what}失败：${problem}`)
}

/** 传到 R2，核对 R2 记下的 ETag（单次上传时是内容的 MD5）；S3 接口另带 Content-MD5，内容不对时 R2 直接拒收 */
async function r2Upload(backend, file, key) {
  const md5 = sha(file.path, 'md5', 'hex')
  let etag
  if (backend.s3) {
    const { res } = await s3Request(backend.s3, 'PUT', key, {
      body: readFileSync(file.path),
      headers: {
        'content-type': contentType(file.name),
        'content-md5': Buffer.from(md5, 'hex').toString('base64'),
      },
      what: `上传 ${key} `,
    })
    etag = res.headers.get('etag') ?? ''
  } else {
    const out = cf(
      [
        'r2',
        'buckets',
        'objects',
        'upload',
        key,
        '--bucket-name',
        R2_BUCKET,
        '--file',
        file.path,
        '--content-type',
        contentType(file.name),
      ],
      `上传 ${key} `,
    )
    if (out?.key !== key) fail(`上传 ${key} 后核对失败：cf 返回的对象名是 ${out?.key}`)
    etag = String(out?.etag ?? '')
  }
  etag = etag.replace(/"/g, '')
  if (etag !== md5) fail(`上传 ${key} 后核对失败：R2 记下的 ETag 是 ${etag}，本机文件的 MD5 是 ${md5}`)
  console.log(`  已上传 ${key}（${formatSize(statSync(file.path).size)}）`)
}

/** 删除 R2 中的对象（只有测试时删） */
async function r2Delete(backend, key) {
  if (backend.s3) await s3Request(backend.s3, 'DELETE', key, { what: `删除 ${key} ` })
  else cf(['r2', 'buckets', 'objects', 'delete', key, '--bucket-name', R2_BUCKET, '--force'], `删除 ${key} `)
}

/** R2 中以 prefix 开头的对象名 */
async function r2List(backend, prefix) {
  if (backend.s3) {
    const { text } = await s3Request(backend.s3, 'GET', undefined, {
      query: { 'list-type': '2', prefix },
      what: '列出测试文件',
    })
    return [...text.matchAll(/<Key>([^<]*)<\/Key>/g)].map((m) => m[1])
  }
  const left = cf(
    ['r2', 'buckets', 'objects', 'list', '--bucket-name', R2_BUCKET, '--prefix', prefix],
    '列出测试文件',
  )
  if (!Array.isArray(left)) fail(`列出测试文件：cf 的输出不是列表：${JSON.stringify(left).slice(0, 300)}`)
  return left.map((o) => o?.key ?? JSON.stringify(o))
}

/** 清 CDN 缓存（按地址） */
async function purgeCache(backend, urls) {
  for (let i = 0; i < urls.length; i += PURGE_BATCH) {
    const files = urls.slice(i, i + PURGE_BATCH)
    if (backend.purge) await purgeViaApi(backend.purge, files)
    else {
      cf(
        ['cache', 'purge', '-z', DL_ZONE, '--force', '--body', JSON.stringify({ files })],
        '清 CDN 缓存',
      )
    }
  }
  console.log(`  已清 CDN 缓存：${urls.length} 个地址`)
}

/** 经 Cloudflare 的接口清缓存（令牌只有这个 zone 的 Cache Purge 权限）；网络错误、5xx 与 429 重试 */
async function purgeViaApi({ token, zone }, files) {
  let problem = ''
  for (let attempt = 0; attempt < 3; attempt++) {
    if (attempt > 0) await sleep(3000 * attempt)
    try {
      const res = await fetch(`https://api.cloudflare.com/client/v4/zones/${zone}/purge_cache`, {
        method: 'POST',
        headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
        body: JSON.stringify({ files }),
        signal: AbortSignal.timeout(30_000),
      })
      const data = await res.json().catch(() => null)
      if (res.ok && data?.success) return
      const errors = (data?.errors ?? []).map((e) => `${e.code} ${e.message}`).join('；')
      problem = `HTTP ${res.status}${errors ? ` ${errors}` : ''}`
      if (res.status < 500 && res.status !== 429) break
    } catch (e) {
      problem = e.message
    }
  }
  fail(`清 CDN 缓存失败：${problem}`)
}

/**
 * 经 https 取回核对：小文件比较内容，大文件比较大小与 ETag（R2 的 MD5）。清缓存要一点时间传到各地，
 * 不一致时等几秒再试。
 */
async function verifyOnline(files, prefix) {
  for (const file of files) {
    const url = `${DL_ORIGIN}/${prefix}${file.name}`
    const size = statSync(file.path).size
    const small = size <= FETCH_COMPARE_MAX
    const expected = small ? sha(file.path, 'sha256', 'hex') : sha(file.path, 'md5', 'hex')
    let problem = ''
    for (let attempt = 0; attempt < 5; attempt++) {
      if (attempt > 0) await sleep(3000)
      try {
        const res = await fetch(url, {
          method: small ? 'GET' : 'HEAD',
          signal: AbortSignal.timeout(30_000),
        })
        if (!res.ok) {
          problem = `HTTP ${res.status}`
          continue
        }
        if (small) {
          const got = createHash('sha256')
            .update(Buffer.from(await res.arrayBuffer()))
            .digest('hex')
          problem = got === expected ? '' : `内容不同（SHA-256 ${got}）`
        } else {
          const length = Number(res.headers.get('content-length'))
          const etag = (res.headers.get('etag') ?? '').replace(/^W\//, '').replace(/"/g, '')
          problem =
            length !== size
              ? `大小为 ${length}，本机为 ${size}`
              : etag && etag !== expected
                ? `ETag 为 ${etag}，本机文件的 MD5 为 ${expected}`
                : ''
        }
        if (!problem) break
      } catch (e) {
        problem = e.message
      }
    }
    if (problem) fail(`经 https 核对 ${url} 失败：${problem}`)
  }
  console.log(`  已经 ${DL_ORIGIN} 取回核对：${files.length} 个文件`)
}

/** 传到下载站：依次上传，清缓存，经 https 核对；测试时（有前缀）最后删掉这些对象 */
async function publishToR2(backend, files, prefix) {
  console.log(`上传到 R2（桶 ${R2_BUCKET}，${DL_ORIGIN}；${backend.how}）：`)
  for (const file of files) await r2Upload(backend, file, `${prefix}${file.name}`)
  const urls = files.map((f) => `${DL_ORIGIN}/${prefix}${f.name}`)
  await purgeCache(backend, urls)
  await verifyOnline(files, prefix)
  if (!prefix) return
  for (const file of files) await r2Delete(backend, `${prefix}${file.name}`)
  // cf 不加 --force 的删除会放弃却照样成功：列一遍核对确实删掉了
  const left = await r2List(backend, prefix)
  if (left.length > 0) fail(`测试文件没有删干净：${left.join('、').slice(0, 300)}`)
  await purgeCache(backend, urls)
  console.log(`  已删除测试文件（前缀 ${prefix}）`)
}

// ---------------------------------------------------------------- GitHub Releases 与官网

/** gh 的结果（不打印），找不到 gh 时报错 */
function ghQuiet(args) {
  const r = spawnSync('gh', args, {
    cwd: root,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  if (r.error) fail(`无法运行 gh（${r.error.message}）：要先安装 GitHub CLI 并 gh auth login`)
  return r
}

/** gh 失败时的第一行说明 */
function ghReason(r) {
  return (r.stderr.split(/\r?\n/).find((l) => l.trim()) ?? `gh 退出码 ${r.status}`).trim()
}

/**
 * 仓库、标签与这个版本的 Release：仓库读不到时为 { error }，否则 { release: null 或 { isDraft }, tagExists }
 */
function githubRelease(repo, tag) {
  const view = ghQuiet(['repo', 'view', repo, '--json', 'name'])
  if (view.status !== 0) return { error: ghReason(view) }
  const release = ghQuiet(['release', 'view', tag, '-R', repo, '--json', 'tagName,isDraft'])
  return {
    release: release.status === 0 ? JSON.parse(release.stdout) : null,
    // 单数的 git/ref 只认完全相同的引用（复数的 git/refs 按前缀匹配，v0.1.0 会匹配到 v0.1.0-beta.1）
    tagExists: ghQuiet(['api', `repos/${repo}/git/ref/tags/${tag}`]).status === 0,
  }
}

/**
 * 在 GitHub 上建 Release v<版本>（--verify-tag：标签要已推送），附件与下载站上的相同，标为最新、不标预发布（备用的
 * 更新源是 releases/latest/download，GitHub 的“最新版本”不包括预发布）。已有时补传、覆盖同名文件，上次没传完留下的
 * 草稿改为正式并标为最新。notes 为 { file } 或 { text }（经标准输入交给 gh，不写临时文件）。
 */
function publishToGithub(version, files, state, notes) {
  const tag = `v${version}`
  const paths = files.map((f) => f.path)
  if (state.release) {
    console.log(`GitHub：${REPO} 已有 ${tag}，补传并覆盖同名文件`)
    execFileSync('gh', ['release', 'upload', tag, '-R', REPO, '--clobber', ...paths], {
      stdio: 'inherit',
    })
    if (state.release.isDraft) {
      execFileSync('gh', ['release', 'edit', tag, '-R', REPO, '--draft=false', '--latest'], {
        stdio: 'inherit',
      })
    }
  } else {
    console.log(`GitHub：在 ${REPO} 上新建 ${tag}`)
    execFileSync(
      'gh',
      [
        'release',
        'create',
        tag,
        '-R',
        REPO,
        '--verify-tag',
        '--title',
        `Termlet ${version}`,
        '--latest',
        '--notes-file',
        notes.file ?? '-',
        ...paths,
      ],
      notes.file
        ? { stdio: 'inherit' }
        : { input: notes.text, stdio: ['pipe', 'inherit', 'inherit'] },
    )
  }
  console.log(`  https://github.com/${REPO}/releases/tag/${tag}`)
}

/**
 * 官网的部署钩子 TERMLET_PAGES_DEPLOY_HOOK（Cloudflare Pages 的地址，有了它就能触发构建，是秘密，不打印）：
 * 读过即从环境中删掉。没设时为 null；required（GitHub Actions 中、没加 --skip-website）时报错
 */
function websiteHook(required) {
  const hook = takeEnvSecret('TERMLET_PAGES_DEPLOY_HOOK')
  if (!hook) {
    if (required) {
      fail('GitHub Actions 中要设 TERMLET_PAGES_DEPLOY_HOOK（release 环境的 Secrets），不让官网重建时加 --skip-website')
    }
    return null
  }
  if (!hook.startsWith(PAGES_HOOK_PREFIX) || !/^[\w-]+$/.test(hook.slice(PAGES_HOOK_PREFIX.length))) {
    fail(`TERMLET_PAGES_DEPLOY_HOOK 应为 Cloudflare Pages 的部署钩子地址（${PAGES_HOOK_PREFIX}<ID>）`)
  }
  return hook
}

/**
 * 调用部署钩子：官网重新构建，读下载站上的新清单与公开仓库的更新日志（只列已发布的版本）。304 是这个分支已有排队
 * 的构建，同样算成功
 */
async function triggerWebsite(hook) {
  let problem = ''
  for (let attempt = 0; attempt < 3; attempt++) {
    if (attempt > 0) await sleep(3000 * attempt)
    try {
      const res = await fetch(hook, { method: 'POST', signal: AbortSignal.timeout(30_000) })
      if (res.ok || res.status === 304) {
        console.log('官网：已调用 Cloudflare Pages 的部署钩子，官网重新构建后（几分钟）下载页与更新日志换成新版本')
        return
      }
      problem = `HTTP ${res.status}`
      if (res.status < 500 && res.status !== 429) break
    } catch (e) {
      problem = e.message
    }
  }
  fail(
    `调用官网的部署钩子失败：${problem}。下载站与 GitHub Release 已经发布；在 Cloudflare Pages 项目的 Deployments 中` +
      '对最近一次正式部署点 Retry deployment，官网就会重建',
  )
}

/** 没有部署钩子时提示怎样让官网重建 */
function websiteHint(dryRun) {
  console.log(
    `官网：没有设 TERMLET_PAGES_DEPLOY_HOOK，${dryRun ? '发布以后' : ''}官网不会自动重建：在 Cloudflare Pages 项目的 ` +
      'Deployments 中对最近一次正式部署点 Retry deployment（或推送官网仓库的 main），下载页与更新日志才换成新版本',
  )
}

/** Release 说明的前几行（--dry-run 时预览） */
function previewNotes(text) {
  const lines = text.split('\n').filter((l) => l.trim())
  for (const line of lines.slice(0, 3)) console.log(`    | ${line}`)
  if (lines.length > 3) console.log(`    | ……（共 ${lines.length} 行）`)
}

async function publishRelease(values) {
  const dir = resolve(values.dir ?? join(cargoTargetDir(), 'dist'))
  if (!existsSync(dir)) fail(`目录不存在：${dir}`)
  const prefix = testPrefix(values)
  const dryRun = Boolean(values['dry-run'])
  const toR2 = !values['skip-r2']
  // 测试（--prefix）时不碰 GitHub，也不调用部署钩子
  const toGithub = !values['skip-github'] && !prefix
  const toWebsite = !values['skip-website'] && !prefix
  if (!toR2 && !toGithub) fail('没有要做的事（--skip-r2 与 --skip-github 都加了，或测试时加了 --skip-r2）')
  const notesFile = values.notes ? resolve(values.notes) : null
  if (notesFile && !existsSync(notesFile)) fail(`找不到 ${notesFile}`)
  const { version, files } = checkSignedRelease(dir, publishKeys(values))
  checkArm64Manifests(
    files.map((f) => f.name).filter((n) => /^latest.*\.yml$/.test(n)),
    values,
  )
  const tag = `v${version}`
  // 令牌与钩子缺了的话，在上传任何文件之前报错
  const backend = toR2 ? cloudflareBackend() : null
  const hook = toWebsite ? websiteHook(IN_CI) : null

  console.log(`版本 ${version}（${dir}），签名与哈希核对通过。`)
  if (toR2) {
    console.log(
      `R2：依次上传到桶 ${R2_BUCKET}${prefix ? `（测试，对象名前加 ${prefix}，核对后删掉）` : ''}，再清缓存、核对` +
        `（${backend.how}）：`,
    )
    for (const f of files) {
      console.log(
        `  ${prefix}${f.name}  ${formatSize(statSync(f.path).size)}  ${contentType(f.name)}`,
      )
    }
  }
  // GitHub：仓库、标签、说明有一样不齐，正式发布时在上传任何文件之前报错
  let github = null
  if (toGithub) {
    // 说明有问题（更新日志中这一版还是“未发布”、缺了）时直接报错，--dry-run 也一样
    const notes = notesFile
      ? { file: notesFile, what: `--notes 给出的 ${notesFile}` }
      : { text: releaseNotes(version, { provenance: IN_CI }), what: `更新日志中 ${version} 的中英文两节` }
    const state = githubRelease(REPO, tag)
    const problem = state.error
      ? `${REPO} 读不到（${state.error}）`
      : !state.tagExists
        ? `${REPO} 中没有标签 ${tag}（先推送标签）`
        : null
    if (problem && !dryRun) fail(`GitHub：${problem}。没有上传任何文件（这次不建 Release 时加 --skip-github）`)
    if (problem) console.log(`GitHub：${problem}，正式发布时会在上传之前报错`)
    if (state.release) {
      const draft = state.release.isDraft ? '（草稿，会改为正式并标为最新）' : ''
      console.log(
        `GitHub：${REPO} 已有 ${tag}${draft}，会补传并覆盖同名文件（${files.length} 个，与 R2 上的相同），说明不改`,
      )
    } else if (!state.error) {
      console.log(`GitHub：会在 ${REPO} 上新建 ${tag}（--verify-tag），标为最新，附件 ${files.length} 个（与 R2 上的相同）`)
      console.log(`  说明：${notes.what}`)
      if (notes.text) previewNotes(notes.text)
    }
    github = { state, notes }
  }
  if (toWebsite) {
    if (hook) console.log(`官网：${dryRun ? '发布以后会' : '最后'}调用 Cloudflare Pages 的部署钩子，让官网重建`)
    else websiteHint(dryRun)
  }
  if (dryRun) {
    console.log('（--dry-run：没有上传）')
    return
  }
  if (toR2) await publishToR2(backend, files, prefix)
  if (prefix) {
    console.log('测试通过：上传、清缓存、经 https 核对都正常，测试文件已删除')
    return
  }
  if (github) publishToGithub(version, files, github.state, github.notes)
  console.log(`已发布 ${version}`)
  if (hook) await triggerWebsite(hook)
}

async function publishConfig(values, path) {
  if (!path) fail('用法：node scripts/release.mjs publish-config [--dry-run] <update.json>')
  const prefix = testPrefix(values)
  const file = resolve(path)
  for (const f of [file, `${file}.minisig`])
    if (!existsSync(f)) fail(`找不到 ${f}（先运行 sign-config）`)
  const text = readFileSync(file)
  const trusted = verifySignature(
    text,
    readFileSync(`${file}.minisig`, 'utf8'),
    publishKeys(values),
    `${file}.minisig 核对失败`,
  )
  const m = /^termlet-remote-config (\S+) (\S+)$/.exec(trusted)
  if (!m) fail(`${file}.minisig 不是远程配置的签名（可信注释：${trusted}）`)
  if (Date.parse(m[2]) <= Date.now()) fail(`远程配置的签名已在 ${m[2]} 过期：重新运行 sign-config`)
  try {
    JSON.parse(text.toString('utf8'))
  } catch {
    fail(`${file} 不是有效的 JSON`)
  }
  const backend = cloudflareBackend()
  // 对象名固定（防火墙只放行 /update.json 与 /update.json.minisig），先传签名
  const files = [
    { name: 'update.json.minisig', path: `${file}.minisig` },
    { name: 'update.json', path: file },
  ]
  console.log(
    `远程配置签发于 ${m[1]}，有效期到 ${m[2]}；依次上传到桶 ${R2_BUCKET}，再清缓存、核对（${backend.how}）：`,
  )
  for (const f of files) console.log(`  ${prefix}${f.name}  ${contentType(f.name)}`)
  if (values['dry-run']) {
    console.log('（--dry-run：没有上传）')
    return
  }
  await publishToR2(backend, files, prefix)
  console.log(
    prefix
      ? '测试通过：上传、清缓存、经 https 核对都正常，测试文件已删除'
      : `已发布远程配置：${DL_ORIGIN}/update.json（CDN 缓存已清，应用下次检查更新时读到）`,
  )
}

// ---------------------------------------------------------------- 密钥

async function newkey(values) {
  const dir = keyDir(values.out)
  const name = values.name ?? 'next'
  if (!/^[\w-]+$/.test(name)) fail('--name 只能包含字母、数字、- 与 _')
  const path = join(dir, `termlet-update-${name}.key.json`)
  if (existsSync(path)) fail(`${path} 已存在`)
  const passphrase = await askNewPassphrase(`新密钥（${name}）`)
  const { privateKey } = generateKeyPairSync('ed25519')
  const data = encryptKey(name, privateKey, randomBytes(8), passphrase)
  // 写入前先确认能用同一个口令解开
  decryptKey(data, passphrase)
  mkdirSync(dir, { recursive: true })
  writeFileSync(path, `${JSON.stringify(data, null, 2)}\n`, { flag: 'wx', mode: 0o600 })
  console.log(`已生成 ${path}\n密钥 ID：${data.keyId}\n公钥：${data.publicKey}`)
  console.log(
    `按 ${relative(root, KEYS_FILE)} 开头的轮换步骤把公钥加入 UPDATE_PUBLIC_KEYS，并离线备份密钥文件`,
  )
}

function show(path) {
  if (!path) fail('用法：node scripts/release.mjs show <密钥文件>')
  const data = readKeyFile(path)
  const builtin = builtinKeys().includes(data.publicKey) ? '是' : '否'
  console.log(
    `名称：${data.name}\n密钥 ID：${data.keyId}\n公钥：${data.publicKey}\n生成于：${data.created}\n应用内置：${builtin}`,
  )
}

/** 固定样例（与 termlet-core 的 update::signature 测试相同）：同样的私钥与内容，签名逐字节不变 */
function selftest() {
  const seed = Buffer.alloc(32, 7)
  const der = Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), seed])
  const key = createPrivateKey({ key: der, format: 'der', type: 'pkcs8' })
  const id = Buffer.from('0102030405060708', 'hex')
  const publicKey = encodePublicKey(id, createPublicKey(key))
  const expected = [
    'untrusted comment: test',
    'RUQBAgMEBQYHCPqS5E59BgU+P2kcY4vRS0+DAo9LVq7/E6QRA+hjTrCvQoS1ug6zKt5EH4J2UU1WKUhjMDAPXLhYS9xgoQlSvAE=',
    'trusted comment: termlet-test',
    '13sM3nu5p4qt/5u4osdwc0ME2X21WGqN4b5c4btGYP6vmUOMornq598yYOgx+fht0FQaUbNDgtOKOxcQfh3IDA==',
    '',
  ].join('\n')
  if (publicKey !== 'RWQBAgMEBQYHCOpKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs')
    fail('公钥的编码与样例不同')
  if (createSignature('termlet', { id, key }, 'termlet-test', 'test') !== expected)
    fail('签名与样例不同')
  if (verifySignature('termlet', expected, [publicKey]) !== 'termlet-test') fail('样例校验失败')
  const keys = builtinKeys()
  if (keys.length === 0) fail(`${relative(root, KEYS_FILE)} 中没有公钥`)
  for (const k of keys) parsePublicKey(k)
  // 清单名与应用的 metadata_file_for 相同（termlet-core 的 platform_names_match_the_release_script）：x64 的两份
  // 不能改名，已发布的 x64 客户端读的就是它们
  const names = {
    'win-x64': 'latest.yml',
    'win-arm64': 'latest-arm64.yml',
    'linux-x64': 'latest-linux.yml',
    'linux-arm64': 'latest-linux-arm64.yml',
    'mac-arm64': 'latest-mac.yml',
  }
  for (const [platform, name] of Object.entries(names)) {
    if (manifestName(platform) !== name)
      fail(`${platform} 的清单应为 ${name}，现在是 ${manifestName(platform)}`)
  }
  for (const suffix of PACKAGES) {
    const m = PACKAGE_NAME.exec(`termlet-1.2.3-${suffix}`)
    if (!m || !names[m[2]]) fail(`PACKAGES 中的 ${suffix} 不是发布的包名`)
    if (!CI_ARTIFACTS.some((a) => a.platform === m[2])) fail(`${suffix} 不属于 CI_ARTIFACTS 中的任何产物`)
  }
  // 只有 Windows、Linux 的 arm64 是实验版（缺了不挡发布）；x64 与 macOS 必需
  for (const a of CI_ARTIFACTS) {
    const experimental = ['win-arm64', 'linux-arm64'].includes(a.platform)
    if (Boolean(a.experimental) !== experimental) fail(`${a.name} 的 experimental 应为 ${experimental}`)
  }
  // S3 请求的签名：AWS 的签名测试样例（botocore 的 aws4_testsuite：get-vanilla、get-vanilla-query-order-key-case、
  // post-x-www-form-urlencoded），密钥是样例中公开的示例值
  const sample = {
    host: 'example.amazonaws.com',
    path: '/',
    amzDate: '20150830T123600Z',
    region: 'us-east-1',
    service: 'service',
    keyId: 'AKIDEXAMPLE',
    secret: 'wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY',
  }
  const sigV4Cases = [
    [{ method: 'GET', payloadHash: sha256Hex('') }, '5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31'],
    [
      { method: 'GET', query: { Param2: 'value2', Param1: 'value1' }, payloadHash: sha256Hex('') },
      'b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500',
    ],
    [
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        payloadHash: sha256Hex('Param1=value1'),
      },
      'ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a',
    ],
  ]
  for (const [request, signature] of sigV4Cases) {
    const got = /Signature=([0-9a-f]+)$/.exec(sigV4({ ...sample, ...request }))?.[1]
    if (got !== signature) fail(`S3 请求的签名与 AWS 的样例不同（${request.method}）：${got}`)
  }
  if (uriEncode("a b+c/d!'()*~") !== 'a%20b%2Bc%2Fd%21%27%28%29%2A~') fail('S3 的百分号编码与规则不同')
  // 版本号的先后（prepare 不许改成更旧的版本）
  const order = ['0.9.0', '1.0.0-alpha', '1.0.0-alpha.1', '1.0.0-beta.2', '1.0.0-beta.10', '1.0.0-rc.1', '1.0.0', '1.0.1', '1.10.0']
  for (let i = 1; i < order.length; i++) {
    if (!(compareVersions(order[i], order[i - 1]) > 0 && compareVersions(order[i - 1], order[i]) < 0))
      fail(`版本比较不对：${order[i]} 应比 ${order[i - 1]} 新`)
  }
  if (!CHANGELOG_HEADING.test('## 0.3.0（2026-11-20）') || !CHANGELOG_HEADING.test('## 0.3.0 (unreleased)'))
    fail('更新日志的版本标题认不出')
  console.log(
    `签名实现与样例一致；内置公钥 ${keys.length} 把：${keys.map((k) => keyIdHex(parsePublicKey(k).id)).join('、')}；` +
      `${PACKAGES.length} 种包分属 ${new Set(Object.values(names)).size} 份清单；S3 请求的签名与 AWS 的样例一致`,
  )
}

const [command, ...rest] = process.argv.slice(2)
const { values, positionals } = parseArgs({
  args: rest,
  options: {
    out: { type: 'string' },
    file: { type: 'string', multiple: true },
    key: { type: 'string' },
    dir: { type: 'string' },
    name: { type: 'string' },
    days: { type: 'string' },
    target: { type: 'string' },
    arch: { type: 'string' },
    stage: { type: 'string' },
    tag: { type: 'string' },
    run: { type: 'string' },
    repo: { type: 'string' },
    notes: { type: 'string' },
    from: { type: 'string' },
    digest: { type: 'string', multiple: true },
    date: { type: 'string' },
    prefix: { type: 'string' },
    'test-pubkey': { type: 'string' },
    'no-app-check': { type: 'boolean' },
    'skip-cargo': { type: 'boolean' },
    'dry-run': { type: 'boolean' },
    'skip-r2': { type: 'boolean' },
    'skip-github': { type: 'boolean' },
    'skip-website': { type: 'boolean' },
    'require-arm64': { type: 'boolean' },
  },
  allowPositionals: true,
})
if (command === 'prepare') prepare(values, positionals[0])
else if (command === 'notes') notesCommand(values, positionals[0])
else if (command === 'build') build(values)
else if (command === 'manifest') {
  if (!values.file?.length) fail('用法：manifest --file <包> [--file <同一平台的其他包>]... [--out <目录>]')
  const files = values.file.map((f) => resolve(f))
  const out = resolve(values.out ?? dirname(files[0]))
  console.log(`已生成 ${join(out, writeManifest(files, out))}`)
} else if (command === 'fetch') await fetchRelease(values)
else if (command === 'collect') collect(values)
else if (command === 'sign') await signRelease(values)
else if (command === 'sign-config') await signConfig(values, positionals[0])
else if (command === 'publish') await publishRelease(values)
else if (command === 'publish-config') await publishConfig(values, positionals[0])
else if (command === 'newkey') await newkey(values)
else if (command === 'licenses') {
  const out = resolve(values.out ?? join(cargoTargetDir(), 'dist', RUST_LICENSES))
  mkdirSync(dirname(out), { recursive: true })
  writeRustLicenses(out, values.target ?? WINDOWS_TARGET)
} else if (command === 'notices') {
  if (!values.target || !values.out) fail('用法：notices --target <Rust 目标> --out <目录>')
  writeNotices(resolve(values.out), values.target, false)
} else if (command === 'show') show(positionals[0])
else if (command === 'selftest') selftest()
else
  fail(
    '用法见脚本开头的注释：prepare、notes、build、manifest、collect、fetch、sign、sign-config、publish、publish-config、' +
      'newkey、licenses、notices、show、selftest',
  )
