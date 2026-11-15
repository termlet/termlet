// 由 docs/brand/logo.svg 与 docs/brand/wordmark.svg（文字标）渲染 Termlet 的各种图标与品牌图：
// - 程序图标 crates/termlet-desktop/resources/windows/termlet.ico：16–256 共 9 种尺寸，每种尺寸单独从矢量图渲染
//   （小尺寸比缩小大图清楚）；256 用 PNG 压缩，其余为 32 位 BMP。termlet-desktop 的 build.rs 把它嵌入 Termlet.exe
//   （资源 ID 1：资源管理器、任务栏显示它，GPUI 也用它作窗口图标），安装程序与卸载程序的图标也用它。
// - 安装向导完成页（及卸载程序）左侧的图 packaging/windows/installer-sidebar.bmp：164×314 的 24 位 BMP（NSIS 只认这种格式）。
// - macOS 的图标集 packaging/macos/Termlet.iconset：16、32、128、256、512 及各自的 @2x，打包时（.github/workflows/release.yml）
//   用 iconutil 生成 Termlet.app 中的 Termlet.icns。
// - Linux 的图标 packaging/linux/icons/hicolor/<尺寸>/apps/dev.termlet.Termlet.png：16–512 共 8 种尺寸，按 hicolor 图标主题的
//   目录结构，打包时放进 tar.gz 的 share/icons（scalable 的那份直接用 logo.svg）。
//   这两个平台与 Windows 用同一张图（画布四周的留白与 macOS 的图标网格一致），每种像素尺寸也都单独渲染。
// - docs/brand/ 下的品牌图：PNG 版 logo、头像、横排组合标（浅色、深色背景各一）、社交分享图，
//   以及 docs/brand/web/ 下的网站图标（favicon、apple-touch-icon、网站清单的图标）。清单见 docs/brand/README.md。
// 用法：node packaging/render-icons.mjs   （需要本机安装 Chrome 或 Edge；修改 logo 后运行，再重新构建）
// 作者：ruan
import { execFileSync } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { deflateSync, inflateSync } from 'node:zlib'

const root = join(fileURLToPath(import.meta.url), '../..')
const brand = join(root, 'docs/brand')
const logo = pathToFileURL(join(brand, 'logo.svg')).href
const icoOut = join(root, 'crates/termlet-desktop/resources/windows/termlet.ico')
const sidebarOut = join(root, 'packaging/windows/installer-sidebar.bmp')
/** Windows 用到的图标尺寸（资源管理器的各种视图、任务栏、高 DPI） */
const SIZES = [256, 128, 64, 48, 40, 32, 24, 20, 16]
/** macOS 的图标集（iconutil 的输入，文件名是固定的）：[文件名, 像素]，@2x 的像素是两倍 */
const MAC_ICONSET = join(root, 'packaging/macos/Termlet.iconset')
const MAC_ICONS = [16, 32, 128, 256, 512].flatMap((size) => [
  [`icon_${size}x${size}.png`, size],
  [`icon_${size}x${size}@2x.png`, size * 2],
])
/** Linux 的图标：hicolor 图标主题的目录与尺寸，文件名是应用的 ID（与 .desktop 的 Icon= 相同） */
const LINUX_ICONS = join(root, 'packaging/linux/icons/hicolor')
const LINUX_SIZES = [16, 24, 32, 48, 64, 128, 256, 512]
const APP_ID = 'dev.termlet.Termlet'
const SIDEBAR = { width: 164, height: 314 }
/** 网站图标 favicon.ico 中的尺寸 */
const FAVICON_SIZES = [48, 32, 16]
/** 横排组合标：文字标的高度、图标与文字的间距，都按底板边长的比例 */
const LOCKUP = { textHeight: 0.5, gap: 0.25 }
/** 横排组合标的 PNG 的高度 */
const LOCKUP_PNG_HEIGHT = 256
/** 深色背景上文字标的颜色 */
const DARK_TEXT = '#f8fafc'
/** 社交分享图：GitHub 仓库的 Social preview（建议 1280×640）与网站的 Open Graph 图（1200×630） */
const SOCIAL = [
  { file: 'social-github.png', width: 1280, height: 640 },
  { file: 'social-og.png', width: 1200, height: 630 },
]

function fail(message) {
  console.error(`错误：${message}`)
  process.exit(1)
}

const candidates = {
  win32: [
    'C:/Program Files/Google/Chrome/Application/chrome.exe',
    'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  ],
  darwin: [
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
  ],
  linux: ['/usr/bin/google-chrome', '/usr/bin/chromium', '/usr/bin/chromium-browser'],
}[process.platform]
const browser = (candidates ?? []).find((p) => existsSync(p))
if (!browser) fail('没有找到 Chrome 或 Edge，无法渲染图标')

/** 用无头浏览器把一段 HTML 截成 PNG（透明背景、缩放比例 1） */
function screenshot(dir, name, html, width, height) {
  const file = join(dir, `${name}.html`)
  const out = join(dir, `${name}.png`)
  writeFileSync(file, `<!doctype html><html><head><meta charset="utf-8">${html}</html>`)
  execFileSync(browser, [
    '--headless=new',
    '--disable-gpu',
    '--hide-scrollbars',
    '--force-device-scale-factor=1',
    '--default-background-color=00000000',
    `--window-size=${width},${height}`,
    `--screenshot=${out}`,
    pathToFileURL(file).href,
  ])
  return decodePng(readFileSync(out))
}

/** 解码 8 位、不隔行扫描的 RGB / RGBA PNG（浏览器截图的格式），返回每像素 4 字节的 RGBA */
function decodePng(png) {
  const width = png.readUInt32BE(16)
  const height = png.readUInt32BE(20)
  const [bitDepth, colorType, , , interlace] = png.subarray(24, 29)
  if (bitDepth !== 8 || ![2, 6].includes(colorType) || interlace !== 0) {
    fail(`不支持的 PNG 格式：位深 ${bitDepth}，颜色类型 ${colorType}，隔行 ${interlace}`)
  }
  const idat = []
  for (let pos = 8; pos < png.length; ) {
    const length = png.readUInt32BE(pos)
    const type = png.toString('latin1', pos + 4, pos + 8)
    if (type === 'IDAT') idat.push(png.subarray(pos + 8, pos + 8 + length))
    pos += 12 + length
  }
  const raw = inflateSync(Buffer.concat(idat))
  const bpp = colorType === 6 ? 4 : 3
  const stride = width * bpp
  const pixels = Buffer.alloc(height * stride)
  for (let y = 0; y < height; y++) {
    const filter = raw[y * (stride + 1)]
    const line = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1))
    for (let x = 0; x < stride; x++) {
      const a = x >= bpp ? pixels[y * stride + x - bpp] : 0
      const b = y > 0 ? pixels[(y - 1) * stride + x] : 0
      const c = x >= bpp && y > 0 ? pixels[(y - 1) * stride + x - bpp] : 0
      const p = a + b - c
      const [pa, pb, pc] = [Math.abs(p - a), Math.abs(p - b), Math.abs(p - c)]
      const predictor = [0, a, b, (a + b) >> 1, pa <= pb && pa <= pc ? a : pb <= pc ? b : c][filter]
      if (predictor === undefined) fail(`PNG 中有未知的过滤类型 ${filter}`)
      pixels[y * stride + x] = (line[x] + predictor) & 0xff
    }
  }
  const rgba = Buffer.alloc(width * height * 4)
  for (let i = 0; i < width * height; i++) {
    pixels.copy(rgba, i * 4, i * bpp, i * bpp + 3)
    rgba[i * 4 + 3] = bpp === 4 ? pixels[i * bpp + 3] : 255
  }
  return { width, height, rgba }
}

/** 从截图中裁出一块 */
function crop(image, left, top, width, height) {
  const rgba = Buffer.alloc(width * height * 4)
  for (let y = 0; y < height; y++) {
    const from = ((top + y) * image.width + left) * 4
    image.rgba.copy(rgba, y * width * 4, from, from + width * 4)
  }
  return { width, height, rgba }
}

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
  return c >>> 0
})

function crc32(buf) {
  let c = 0xffffffff
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8)
  return (c ^ 0xffffffff) >>> 0
}

/** 编码为 8 位 RGBA 的 PNG（不做行过滤，交给 deflate） */
function encodePng({ width, height, rgba }) {
  const chunk = (type, data) => {
    const len = Buffer.alloc(4)
    len.writeUInt32BE(data.length)
    const body = Buffer.concat([Buffer.from(type, 'latin1'), data])
    const crc = Buffer.alloc(4)
    crc.writeUInt32BE(crc32(body))
    return Buffer.concat([len, body, crc])
  }
  const ihdr = Buffer.alloc(13)
  ihdr.writeUInt32BE(width, 0)
  ihdr.writeUInt32BE(height, 4)
  ihdr.set([8, 6, 0, 0, 0], 8)
  const raw = Buffer.alloc(height * (width * 4 + 1))
  for (let y = 0; y < height; y++) {
    rgba.copy(raw, y * (width * 4 + 1) + 1, y * width * 4, (y + 1) * width * 4)
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ])
}

/** 图标中的 32 位 BMP：BITMAPINFOHEADER（高度写两倍）、自下而上的 BGRA，再加 1 位的透明掩码 */
function encodeIconBmp({ width, height, rgba }) {
  const maskStride = Math.ceil(width / 32) * 4
  const header = Buffer.alloc(40)
  header.writeUInt32LE(40, 0)
  header.writeInt32LE(width, 4)
  header.writeInt32LE(height * 2, 8)
  header.writeUInt16LE(1, 12)
  header.writeUInt16LE(32, 14)
  header.writeUInt32LE(width * height * 4 + maskStride * height, 20)
  const pixels = Buffer.alloc(width * height * 4)
  const mask = Buffer.alloc(maskStride * height)
  for (let y = 0; y < height; y++) {
    const row = height - 1 - y
    for (let x = 0; x < width; x++) {
      const i = (y * width + x) * 4
      const o = (row * width + x) * 4
      pixels[o] = rgba[i + 2]
      pixels[o + 1] = rgba[i + 1]
      pixels[o + 2] = rgba[i]
      pixels[o + 3] = rgba[i + 3]
      if (rgba[i + 3] === 0) mask[row * maskStride + (x >> 3)] |= 0x80 >> (x & 7)
    }
  }
  return Buffer.concat([header, pixels, mask])
}

/** ICO 文件：目录项 + 各尺寸的图像 */
function encodeIco(images) {
  const header = Buffer.alloc(6)
  header.writeUInt16LE(1, 2)
  header.writeUInt16LE(images.length, 4)
  const entries = []
  const datas = []
  let offset = 6 + 16 * images.length
  for (const image of images) {
    const data = image.width >= 256 ? encodePng(image) : encodeIconBmp(image)
    const entry = Buffer.alloc(16)
    entry[0] = image.width >= 256 ? 0 : image.width
    entry[1] = image.height >= 256 ? 0 : image.height
    entry.writeUInt16LE(1, 4)
    entry.writeUInt16LE(32, 6)
    entry.writeUInt32LE(data.length, 8)
    entry.writeUInt32LE(offset, 12)
    offset += data.length
    entries.push(entry)
    datas.push(data)
  }
  return Buffer.concat([header, ...entries, ...datas])
}

/** 编码为 24 位 BMP（NSIS 只认这种格式）：自下而上、BGR，每行补齐到 4 字节；透明部分叠在白色上 */
function encodeBmp({ width, height, rgba }) {
  const rowSize = Math.ceil((width * 3) / 4) * 4
  const size = 54 + rowSize * height
  const bmp = Buffer.alloc(size)
  bmp.write('BM', 0, 'latin1')
  bmp.writeUInt32LE(size, 2)
  bmp.writeUInt32LE(54, 10)
  bmp.writeUInt32LE(40, 14)
  bmp.writeInt32LE(width, 18)
  bmp.writeInt32LE(height, 22)
  bmp.writeUInt16LE(1, 26)
  bmp.writeUInt16LE(24, 28)
  bmp.writeUInt32LE(rowSize * height, 34)
  bmp.writeInt32LE(2835, 38) // 72 DPI
  bmp.writeInt32LE(2835, 42)
  for (let y = 0; y < height; y++) {
    const row = 54 + (height - 1 - y) * rowSize
    for (let x = 0; x < width; x++) {
      const i = (y * width + x) * 4
      const alpha = rgba[i + 3] / 255
      const over = (v) => Math.round(v * alpha + 255 * (1 - alpha))
      bmp[row + x * 3] = over(rgba[i + 2])
      bmp[row + x * 3 + 1] = over(rgba[i + 1])
      bmp[row + x * 3 + 2] = over(rgba[i])
    }
  }
  return bmp
}

/** 拆出 SVG 根元素的 viewBox 与其中的内容 */
function parseSvg(file) {
  const svg = readFileSync(join(brand, file), 'utf8')
  const m = svg.match(/^<svg\b[^>]*\sviewBox="([^"]+)"[^>]*>([\s\S]*)<\/svg>\s*$/)
  if (!m) fail(`${file} 的格式不对：根元素应当是带 viewBox 的 <svg>`)
  const [x, y, w, h] = m[1].trim().split(/\s+/).map(Number)
  return { svg, x, y, w, h, body: dedent(m[2]) }
}

/** 去掉首尾的空行与各行共同的缩进 */
function dedent(text) {
  const lines = text.replace(/^\s*\n|\s+$/g, '').split('\n')
  const common = Math.min(...lines.filter((line) => line.trim()).map((line) => line.match(/^ */)[0].length))
  return lines.map((line) => line.slice(common)).join('\n')
}

/** 取元素的属性值 */
const attr = (tag, name) => tag.match(new RegExp(`\\s${name}="([^"]*)"`))?.[1]
/** SVG 中的数值：最多两位小数 */
const num = (v) => String(Math.round(v * 100) / 100)
/** 每行缩进两格 */
const indent = (text) => text.replace(/^/gm, '  ')

const logoSvg = parseSvg('logo.svg')
const wordmark = parseSvg('wordmark.svg')
const wordmarkFill = attr(wordmark.body, 'fill') ?? fail('wordmark.svg 的路径没有写 fill')
// 底板是 logo 中的圆角方块（<rect>），连同描边算出它占的范围
const plate = logoSvg.body.match(/<rect\b[^>]*\/>/)?.[0] ?? fail('logo.svg 中没有底板（<rect>）')
const plateStroke = Number(attr(plate, 'stroke-width') ?? 0)
const PLATE = {
  x: Number(attr(plate, 'x')) - plateStroke / 2,
  y: Number(attr(plate, 'y')) - plateStroke / 2,
  size: Number(attr(plate, 'width')) + plateStroke,
}
if (attr(plate, 'width') !== attr(plate, 'height') || Number.isNaN(PLATE.x + PLATE.y + PLATE.size)) {
  fail('logo.svg 的底板应当是写明 x、y、width、height 的正方形')
}
const plateViewBox = [PLATE.x, PLATE.y, PLATE.size, PLATE.size].map(num).join(' ')
/** 只取底板（连同描边），四周不留白：网站图标与组合标用 */
const tightSvg = logoSvg.svg.replace(/viewBox="[^"]*"/, `viewBox="${plateViewBox}"`)
/** 底色铺满整个画布、没有圆角与描边：头像、apple-touch-icon、可遮罩图标（由平台自己切成圆角或圆形） */
const fullBleedSvg = logoSvg.svg.replace(
  plate,
  `<rect x="${logoSvg.x}" y="${logoSvg.y}" width="${logoSvg.w}" height="${logoSvg.h}" fill="${attr(plate, 'fill')}"/>`,
)

/** 横排组合标：左边是图标（只取底板），右边是文字标，两者的中线对齐；返回 SVG 与宽高比 */
function lockupSvg(textColor) {
  const s = PLATE.size
  const textHeight = s * LOCKUP.textHeight
  const textWidth = (wordmark.w * textHeight) / wordmark.h
  const textX = s + s * LOCKUP.gap
  const width = textX + textWidth
  const text = wordmark.body.replaceAll(`fill="${wordmarkFill}"`, `fill="${textColor}"`)
  const svg = [
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${num(width)} ${num(s)}">`,
    `  <svg width="${num(s)}" height="${num(s)}" viewBox="${plateViewBox}">`,
    indent(indent(logoSvg.body)),
    '  </svg>',
    `  <svg x="${num(textX)}" y="${num((s - textHeight) / 2)}" width="${num(textWidth)}" height="${num(textHeight)}" viewBox="${[wordmark.x, wordmark.y, wordmark.w, wordmark.h].map(num).join(' ')}">`,
    indent(indent(text)),
    '  </svg>',
    '</svg>',
    '',
  ].join('\n')
  return { svg, ratio: width / s }
}

/** 设置“关于”页的 slogan（文案键 settings.slogan）的简体中文与英文 */
function slogan() {
  const yml = readFileSync(join(root, 'crates/termlet-desktop/locales/settings.yml'), 'utf8')
  const m = yml.match(/^settings\.slogan:\r?\n {2}zh-CN: (.+)\r?\n {2}en: (.+)$/m)
  if (!m) fail('locales/settings.yml 中没有找到 settings.slogan 的简体中文与英文')
  return [m[1], m[2]].map((s) =>
    s
      .trim()
      .replace(/^(["'])(.*)\1$/, '$2')
      .replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' })[c]),
  )
}

const GAP = 8

/** 把若干张 SVG 按各自的尺寸排成一行渲染一次（每张都从矢量图缩放），再按位置裁开 */
function render(dir, name, items) {
  let x = 0
  const placed = items.map((item) => {
    const at = { ...item, x }
    x += item.width + GAP
    return at
  })
  const height = Math.max(...items.map((item) => item.height))
  const imgs = placed
    .map(
      ({ src, x, width, height }) =>
        `<img src="${src}" style="position:absolute;left:${x}px;top:0;width:${width}px;height:${height}px">`,
    )
    .join('')
  const sheet = screenshot(
    dir,
    name,
    `<style>html,body{margin:0;background:transparent}</style><body>${imgs}</body>`,
    x,
    height,
  )
  if (sheet.width !== x || sheet.height !== height) fail(`${name} 的截图尺寸不对：${sheet.width}×${sheet.height}`)
  return placed.map(({ x, width, height }) => crop(sheet, x, 0, width, height))
}

/** 四角应当透明（圆角方块外）或不透明（底色铺满），否则说明截图的背景或裁切不对 */
function checkCorners(image, alpha, what) {
  const { width, height, rgba } = image
  const corners = [0, width - 1, (height - 1) * width, height * width - 1]
  if (corners.some((i) => rgba[i * 4 + 3] !== alpha)) {
    fail(`${what}（${width}×${height}）的四角${alpha === 0 ? '不透明' : '透明'}`)
  }
}

const written = []
function save(path, data, note) {
  mkdirSync(dirname(path), { recursive: true })
  writeFileSync(path, data)
  written.push(`${path}（${note}）`)
}
const square = (src, size) => ({ src, width: size, height: size })

const dir = mkdtempSync(join(tmpdir(), 'termlet-icons-'))
try {
  const icons = render(
    dir,
    'icons',
    SIZES.map((size) => square(logo, size)),
  )
  for (const image of icons) checkCorners(image, 0, '程序图标')
  save(icoOut, encodeIco(icons), SIZES.join('、'))

  // macOS 的图标集与 Linux 的图标：同一种像素尺寸只渲染一次
  const platformSizes = [...new Set([...MAC_ICONS.map(([, px]) => px), ...LINUX_SIZES])].sort((a, b) => a - b)
  const platformIcons = new Map(
    render(
      dir,
      'platform-icons',
      platformSizes.map((size) => square(logo, size)),
    ).map((image, i) => [platformSizes[i], image]),
  )
  for (const image of platformIcons.values()) checkCorners(image, 0, '程序图标')
  for (const [file, px] of MAC_ICONS) {
    save(join(MAC_ICONSET, file), encodePng(platformIcons.get(px)), `${px}×${px}`)
  }
  for (const size of LINUX_SIZES) {
    const file = join(LINUX_ICONS, `${size}x${size}/apps/${APP_ID}.png`)
    save(file, encodePng(platformIcons.get(size)), `${size}×${size}`)
  }

  // 侧边图：底色与 logo 的底色同色系，logo 与文字标放在上部，下方留白
  const sidebar = screenshot(
    dir,
    'sidebar',
    `<style>
      html,body{margin:0;width:${SIDEBAR.width}px;height:${SIDEBAR.height}px;overflow:hidden}
      body{display:flex;flex-direction:column;align-items:center;
        background:linear-gradient(170deg,#f0fdf4 0%,#d1fae5 100%)}
      .logo{width:104px;height:104px;margin-top:58px}
      .name{height:17px;margin-top:12px}
    </style><body><img class="logo" src="${logo}"><img class="name" src="${pathToFileURL(join(brand, 'wordmark.svg')).href}"></body>`,
    SIDEBAR.width,
    SIDEBAR.height,
  )
  if (sidebar.width !== SIDEBAR.width || sidebar.height !== SIDEBAR.height) {
    fail(`侧边图尺寸不对：${sidebar.width}×${sidebar.height}`)
  }
  save(sidebarOut, encodeBmp(sidebar), `${SIDEBAR.width}×${SIDEBAR.height}，24 位 BMP`)

  // 透明背景的 logo：通用的 PNG 版与网站清单（manifest）中的图标
  const [logoPng, icon512, icon192] = render(
    dir,
    'logo',
    [1024, 512, 192].map((size) => square(logo, size)),
  )
  for (const image of [logoPng, icon512, icon192]) checkCorners(image, 0, 'PNG 版 logo')
  save(join(brand, 'logo.png'), encodePng(logoPng), '1024×1024，透明背景')
  save(join(brand, 'web/icon-512.png'), encodePng(icon512), '512×512，透明背景')
  save(join(brand, 'web/icon-192.png'), encodePng(icon192), '192×192，透明背景')

  // 网站图标：只取底板，浏览器标签页上的 16px 图标不浪费四周的像素
  const faviconSvg = join(brand, 'web/favicon.svg')
  save(faviconSvg, tightSvg, '只取底板的 SVG')
  const favicons = render(
    dir,
    'favicon',
    FAVICON_SIZES.map((size) => square(pathToFileURL(faviconSvg).href, size)),
  )
  for (const image of favicons) checkCorners(image, 0, '网站图标')
  save(join(brand, 'web/favicon.ico'), encodeIco(favicons), FAVICON_SIZES.join('、'))

  // 底色铺满的方形图：头像、iOS 主屏幕图标（必须不透明）、可遮罩图标
  const fullBleed = join(dir, 'full-bleed.svg')
  writeFileSync(fullBleed, fullBleedSvg)
  const [avatar, maskable, apple] = render(
    dir,
    'full-bleed',
    [1024, 512, 180].map((size) => square(pathToFileURL(fullBleed).href, size)),
  )
  for (const image of [avatar, maskable, apple]) checkCorners(image, 255, '底色铺满的图')
  save(join(brand, 'avatar.png'), encodePng(avatar), '1024×1024，底色铺满')
  save(join(brand, 'web/icon-maskable-512.png'), encodePng(maskable), '512×512，底色铺满')
  save(join(brand, 'web/apple-touch-icon.png'), encodePng(apple), '180×180，底色铺满')

  // 横排组合标：浅色背景用深色文字，深色背景用浅色文字
  const lockups = [
    ['logo-horizontal', wordmarkFill, '浅色背景用'],
    ['logo-horizontal-dark', DARK_TEXT, '深色背景用'],
  ].map(([name, color, note]) => {
    const { svg, ratio } = lockupSvg(color)
    const file = join(brand, `${name}.svg`)
    save(file, svg, note)
    return { name, note, src: pathToFileURL(file).href, width: Math.round(LOCKUP_PNG_HEIGHT * ratio), height: LOCKUP_PNG_HEIGHT }
  })
  render(dir, 'lockups', lockups).forEach((image, i) => {
    checkCorners(image, 0, '横排组合标')
    save(join(brand, `${lockups[i].name}.png`), encodePng(image), `${image.width}×${image.height}，${lockups[i].note}`)
  })

  // 社交分享图：组合标与 slogan（简体中文、英文）居中，底色与 logo 的底色同色系
  const [zh, en] = slogan()
  for (const { file, width, height } of SOCIAL) {
    const image = screenshot(
      dir,
      file,
      `<style>
        html,body{margin:0;width:${width}px;height:${height}px;overflow:hidden}
        body{display:flex;flex-direction:column;align-items:center;justify-content:center;
          background:linear-gradient(160deg,#f0fdf4 0%,#d1fae5 100%)}
        img{height:160px}
        .zh{margin-top:60px;font:400 46px "Microsoft YaHei UI","Microsoft YaHei","PingFang SC",sans-serif;color:#1e293b}
        .en{margin-top:16px;font:400 31px "Segoe UI",system-ui,sans-serif;color:#475569}
      </style><body><img src="${lockups[0].src}"><div class="zh">${zh}</div><div class="en">${en}</div></body>`,
      width,
      height,
    )
    if (image.width !== width || image.height !== height) fail(`${file} 的尺寸不对：${image.width}×${image.height}`)
    checkCorners(image, 255, '社交分享图')
    save(join(brand, file), encodePng(image), `${width}×${height}`)
  }
} finally {
  rmSync(dir, { recursive: true, force: true })
}

for (const line of written) console.log(`已生成 ${line}`)
