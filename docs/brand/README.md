# termlet 品牌素材

> 更新：2026-10-30（加 macOS 的图标集与 Linux 的 hicolor 图标）；2026-10-29（加文字标、横排组合标、头像、网站图标与社交分享图，都由脚本从 `logo.svg` 与 `wordmark.svg` 生成）

## Slogan

| 语言 | 文案 |
|---|---|
| 中文（主） | 轻巧的 SSH 终端，AI 也能安全上手 |
| English | The lightweight SSH client your AI can safely use. |

两个卖点：**轻量**（安装包 16.2MB、冷启动约 0.3 秒（实测 268–324ms）、默认浅色界面），**AI 安全接入**（termlet CLI：凭据留在本机、主机白名单、危险命令拦截、审计日志）。README（中英文）、设置的“关于”页（文案键 `settings.slogan`）与社交分享图使用这句；分享图上的文字由脚本从 `settings.slogan` 读取。

## Logo

主 logo：`logo.svg`（A 叶芽光标）。

- **含义**：英文后缀 -let 表示"小的"（leaflet 小叶、droplet 小水滴），termlet 即"小终端"。提示符 `>` 表明这是终端；原本的下划线光标 `_` 换成一片叶子，取自 leaflet，表达轻巧、清新，也让图标与常见的 `>_` 终端图标区分开。
- **小尺寸**：16px 时叶子只剩一个绿点，主要靠 `>` 识别。

当时没有选中的备选方案（不收录在仓库中）：C 提示符窗口（蓝紫渐变底、白色 `>_`，最直观，但与常见终端图标相似）；D 双箭头（白底、渐变 `››`，表示连接与转发）。

名称与标志的使用规则见仓库根部的 [TRADEMARK.md](../../TRADEMARK.md)：它们不随 AGPL 授权。

### 文字标

`wordmark.svg`：“Termlet”，字体 [Manrope](https://github.com/google/fonts/tree/main/ofl/manrope)（SIL OFL 1.1），字重 700，字距 -0.01em，用 HarfBuzz 排版（保留字体的字偶距）后转成轮廓，显示时不依赖字体；颜色 `#1e293b` 与提示符 `>` 相同。2026-10-29 从 Inter、Geist、Manrope、Nunito、JetBrains Mono、Space Grotesk、Outfit、Plus Jakarta Sans 中选定：笔画开阔、略带圆润，与圆头的 `>` 和叶子协调。

要重做（换字体、改字距）时：用 fontTools 的 `varLib.instancer` 把可变字体实例化到所需字重，用 uharfbuzz 排版（开 `kern`），按排版结果把各字形经 `SVGPathPen` 沿基线翻转输出，viewBox 取轮廓的边界，路径写 `fill="#1e293b"`（脚本按这个颜色换出深色背景版）。

### 版本一览

`logo.svg` 与 `wordmark.svg` 是手工维护的源文件，其余都由 `packaging/render-icons.mjs` 生成，不要手改（程序与安装包用的图标见下面的“在代码中的使用”）。

| 文件 | 内容 | 用途 |
|---|---|---|
| `logo.svg` | 源文件：圆角底板，画布四周留白 | 程序内、README |
| `wordmark.svg` | 源文件：文字标 | 组合标、安装向导侧边图 |
| `logo.png` | 1024×1024，透明背景 | 不认 SVG 的地方 |
| `logo-horizontal.svg`、`.png` | 横排组合标，深色文字（PNG 高 256） | 浅色背景：网站导航栏、文档页眉 |
| `logo-horizontal-dark.svg`、`.png` | 横排组合标，浅色文字 `#f8fafc` | 深色背景：网站深色模式 |
| `avatar.png` | 1024×1024，底色铺满、没有圆角与描边 | GitHub 组织头像（平台自己切圆角或圆形） |
| `social-github.png` | 1280×640，组合标与中英文 slogan | GitHub 仓库 Settings → Social preview |
| `social-og.png` | 1200×630，同上 | 网站的 `og:image`、`twitter:image` |
| `web/favicon.ico` | 16、32、48，只取底板（四周不留白） | 网站根目录的 `/favicon.ico` |
| `web/favicon.svg` | 只取底板 | 支持 SVG 图标的浏览器 |
| `web/apple-touch-icon.png` | 180×180，底色铺满（iOS 要求不透明，圆角由系统切） | iOS 添加到主屏幕 |
| `web/icon-192.png`、`web/icon-512.png` | 透明背景 | 网站清单（manifest）的普通图标 |
| `web/icon-maskable-512.png` | 512×512，底色铺满，图形在中心 80% 的安全区内 | 网站清单的可遮罩图标 |

网站中的引用方式（`web/` 下的文件放到网站根目录）：

```html
<link rel="icon" href="/favicon.ico" sizes="32x32">
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<link rel="manifest" href="/site.webmanifest">
<meta property="og:image" content="https://termlet.dev/social-og.png">
```

`favicon.ico` 写 `sizes="32x32"`，Chrome 才会优先用 SVG。清单中的图标：

```json
"icons": [
  { "src": "/icon-192.png", "type": "image/png", "sizes": "192x192" },
  { "src": "/icon-512.png", "type": "image/png", "sizes": "512x512" },
  { "src": "/icon-maskable-512.png", "type": "image/png", "sizes": "512x512", "purpose": "maskable" }
]
```

还没有做的：微软商店的磁贴图、单色版（目前没有托盘图标、水印等用处）。

### 配色

| 用途 | 颜色 |
|---|---|
| 提示符 `>`、文字标 | `#1e293b`（slate-800） |
| 深色背景上的文字标 | `#f8fafc`（slate-50） |
| 叶子渐变 | `#047857` → `#10b981`（emerald-700 → emerald-500） |
| 叶脉 | `#ecfdf5` |
| 底色渐变 | `#f0fdf4` → `#d1fae5`，描边 `#a7f3d0` |
| 分享图上英文 slogan | `#475569`（slate-600） |

### 尺寸与安全区

画布 1024×1024，圆角方块 832×832（四周各留 96），圆角半径 200，与 macOS 图标网格一致；Windows、Linux 使用同一张图。

横排组合标：图标只取底板（连同描边共 842×842），文字标的高度为底板边长的 1/2，与图标的间距为 1/4，两者的中线对齐（脚本中的 `LOCKUP`）。四周留白的大小由使用的地方决定，建议不少于底板边长的 1/4。

## 在代码中的使用

| 位置 | 来源 |
|---|---|
| 程序图标（资源管理器、任务栏、窗口）与安装程序、卸载程序的图标 | `crates/termlet-desktop/resources/windows/termlet.ico`（16–256 共 9 种尺寸，每种尺寸单独从 `logo.svg` 渲染）；`termlet-desktop` 的 `build.rs` 把它嵌入 `Termlet.exe`（资源 ID 1，GPUI 也用它作窗口图标），`packaging/windows/installer.nsi` 用它作安装程序与卸载程序的图标 |
| 安装向导完成页（及卸载程序）左侧的图 | `packaging/windows/installer-sidebar.bmp`（164×314 的 24 位 BMP，NSIS 只认这种格式；由 `logo.svg` 与 `wordmark.svg` 渲染） |
| 标题栏、开始页、锁屏、设置的“关于” | `crates/termlet-desktop/src/brand.rs` 编译时嵌入 `docs/brand/logo.svg`（唯一来源），按 128 像素栅格化显示（彩色 SVG，GPUI 的 `svg()` 只能画单色） |
| macOS 的应用图标（Finder、程序坞） | `packaging/macos/Termlet.iconset`（iconutil 的输入：`icon_16x16.png` 到 `icon_512x512@2x.png` 共 10 个，16–1024 像素，每种像素尺寸单独渲染）；`.github/workflows/release.yml` 打包时 `iconutil -c icns` 生成 `Termlet.app/Contents/Resources/Termlet.icns`，`packaging/macos/Info.plist` 的 `CFBundleIconFile` 指向它 |
| Linux 的菜单与任务栏图标 | `packaging/linux/icons/hicolor/<尺寸>/apps/dev.termlet.Termlet.png`（16、24、32、48、64、128、256、512）；打包时连同 `logo.svg`（作为 `scalable/apps/dev.termlet.Termlet.svg`）放进 tar.gz 的 `share/icons/hicolor`，菜单项 `dev.termlet.Termlet.desktop` 的 `Icon=dev.termlet.Termlet` 按名称找到它们，`install.sh` 装到 `~/.local/share/icons/hicolor` |
| README（中英文） | `docs/brand/logo.svg` |

命令行工具 `termlet.exe` 不带图标。macOS 与 Linux 的图标与 Windows 的程序图标是同一张图（画布四周的留白与 macOS 的图标网格一致，见上面的“尺寸与安全区”），macOS 不另加阴影。

### 修改 logo、文字标或 slogan 后重新生成

```bash
node packaging/render-icons.mjs   # 用本机 Chrome / Edge 无头渲染
```

脚本会校验各图的尺寸，以及四角：程序图标、透明背景的图与组合标应当透明，底色铺满的图与分享图应当不透明。分享图上的 slogan 用系统字体（Microsoft YaHei UI、Segoe UI）排版，在 Windows 上运行。之后重新构建（`build.rs` 在图标变化时重新嵌入），`Termlet.exe`、安装程序的图标与安装向导的图随之更新，macOS、Linux 的图标在下次打包时更新；标题栏等处的 logo 直接取 `logo.svg`，不用重新生成。
