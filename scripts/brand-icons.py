#!/usr/bin/env python3
"""生成系统与品牌标志（Simple Icons）：主机列表与 Tab 上远程系统、本机 shell 的图标，主机工具 Docker 页、
命令行审批框中发起程序的标志。

用法（在仓库任意位置执行均可，只用 Python 3 标准库）：

    python scripts/brand-icons.py              下载固定版本的 npm 包，核对 SHA-256 后生成
    python scripts/brand-icons.py --src <目录>  用已解压的 npm 包目录（含 icons/、data/simple-icons.json）离线生成

生成的文件（都提交进仓库，不要手改）：

    crates/termlet-desktop/assets/brand-icons/<slug>.svg   用到的标志，每个只留一条路径（24×24，单色）
    crates/termlet-desktop/assets/brand-icons/LICENSE      Simple Icons 的 CC0 许可原文，以及每个标志的来源、
                                                           单独的许可与商标说明（随安装包分发）
    crates/termlet-desktop/src/brand_icons/table.rs        Brand 枚举：标识、品牌名、品牌色与 SVG

只收 ICONS 中列出的标志。Simple Icons 整体是 CC0，但个别标志另有许可（数据中的 license）：没有写的按 CC0；
署名类（CC-BY、CC-BY-SA）与 MIT 要在 ATTRIBUTIONS 中写明作者（LICENSE 中列出作者、来源与协议，MIT 附全文）；
custom 的逐个确认后放进 CUSTOM_LICENSES；其他（GPL 等）报错不收。商标不因许可而放弃，归各自所有者。

去掉某个标志：从 ICONS 中删去重新生成即可（界面按标识查找，找不到时 Linux 发行版落到通用的 linux 标志，其余用原来的
图标，见 brand_icons/mod.rs 的对应表）。

升级图标集时改 VERSION 与 TARBALL_SHA256（下载 npm 包后用 sha256sum 算出），重新生成并检查报告。
"""

import argparse
import hashlib
import io
import json
import re
import sys
import tarfile
import tempfile
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

VERSION = '16.36.0'
TARBALL_URL = f'https://registry.npmjs.org/simple-icons/-/simple-icons-{VERSION}.tgz'
TARBALL_SHA256 = '2069d8e682dda3aa05fa71de36c0ab17984e62d2bd34d7082c4a85e0691b4235'

ROOT = Path(__file__).resolve().parent.parent
ASSETS_DIR = ROOT / 'crates/termlet-desktop/assets/brand-icons'
TABLE_RS = ROOT / 'crates/termlet-desktop/src/brand_icons/table.rs'

# 收录的标志：Simple Icons 的 slug → (Rust 变体名, 用途)。Simple Icons 中没有的（微软的 Windows、PowerShell，
# OpenAI，MSYS2、Cygwin）不在这里，界面用原来的图标；fish shell 的标志是 GPL-2.0，不收；Fedora 的是 custom 许可，
# 用户 2026-11-02 定不收（Fedora 主机用通用的 linux 标志）
ICONS = {
    # 远程系统（RemoteOs）与 WSL 发行版
    'almalinux': ('AlmaLinux', '系统'),
    'alpinelinux': ('AlpineLinux', '系统'),
    'apple': ('Apple', '系统：macOS'),
    'archlinux': ('ArchLinux', '系统'),
    'centos': ('CentOs', '系统'),
    'debian': ('Debian', '系统'),
    'freebsd': ('FreeBsd', '系统'),
    'gentoo': ('Gentoo', '系统'),
    'kalilinux': ('KaliLinux', '系统'),
    'linux': ('Linux', '系统：认不出发行版的 Linux'),
    'manjaro': ('Manjaro', '系统'),
    'netbsd': ('NetBsd', '系统'),
    'openbsd': ('OpenBsd', '系统'),
    'raspberrypi': ('RaspberryPi', '系统：Raspberry Pi OS'),
    'redhat': ('RedHat', '系统：RHEL'),
    'rockylinux': ('RockyLinux', '系统'),
    'suse': ('Suse', '系统：openSUSE 与 SLES'),
    'ubuntu': ('Ubuntu', '系统'),
    # 本机的 shell
    'gitforwindows': ('GitForWindows', 'shell：Git Bash'),
    'gnubash': ('GnuBash', 'shell：bash'),
    'nushell': ('Nushell', 'shell：nu'),
    'zsh': ('Zsh', 'shell：zsh'),
    # 主机工具与命令行审批框
    'claude': ('Claude', '审批框：Claude Code'),
    'docker': ('Docker', '主机工具：Docker 页'),
    'googlegemini': ('GoogleGemini', '审批框：Gemini CLI'),
}

# 不写许可时按 Simple Icons 整体的 CC0；下面这些另有许可、可以收（在 LICENSE 中写明来源与许可）
PERMITTED_LICENSES = {
    'MIT', 'CC-BY-3.0', 'CC-BY-4.0', 'CC-BY-SA-2.5', 'CC-BY-SA-3.0', 'CC-BY-SA-4.0',
}
# custom 许可：逐个看过许可链接后放进来（slug → 说明，写进 LICENSE）；现在没有
CUSTOM_LICENSES = {}

# 另有许可的标志的作者（版权人）与说明，写进 LICENSE（取自各自的官方页面，见 Source）
ATTRIBUTIONS = {
    'debian': {
        'author': 'Copyright (c) 1999 Software in the Public Interest, Inc.; created by Raul Silva '
                  '(Debian Open Use Logo, https://www.debian.org/logos/)',
        'note': 'Debian 官方以 LGPL-3.0 或更新版本、或 CC BY-SA 3.0 Unported 发布；这里按 CC BY-SA 3.0 使用。'
                ' / Released by Debian under the LGPL-3.0-or-later or CC BY-SA 3.0 Unported;'
                ' used here under CC BY-SA 3.0.',
    },
    'gentoo': {
        'author': 'Gentoo Foundation, Inc. (Gentoo "g" logo, https://wiki.gentoo.org/wiki/Project:Artwork)',
        'note': 'Simple Icons 的数据标为 CC BY-SA 2.5；Gentoo 现在的标志使用准则写的是 CC BY-SA 4.0。'
                ' / Simple Icons lists CC BY-SA 2.5; the current Gentoo name and logo guidelines state CC BY-SA 4.0.',
    },
    'rockylinux': {
        'author': 'Rocky Enterprise Software Foundation (https://github.com/rocky-linux/branding)',
    },
    'gnubash': {
        'copyright': "Copyright (c) 2016 ol' dirty bashnerds",
        'author': "Copyright (c) 2016 ol' dirty bashnerds (https://github.com/odb/official-bash-logo)",
    },
}

# 协议的说明链接（CC 协议按链接引用全文）
LICENSE_URLS = {
    'CC-BY-3.0': 'https://creativecommons.org/licenses/by/3.0/',
    'CC-BY-4.0': 'https://creativecommons.org/licenses/by/4.0/',
    'CC-BY-SA-2.5': 'https://creativecommons.org/licenses/by-sa/2.5/',
    'CC-BY-SA-3.0': 'https://creativecommons.org/licenses/by-sa/3.0/',
    'CC-BY-SA-4.0': 'https://creativecommons.org/licenses/by-sa/4.0/',
}

MIT_TEXT = """Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE."""

SVG_NS = 'http://www.w3.org/2000/svg'


def fail(message):
    print(f'错误：{message}', file=sys.stderr)
    sys.exit(1)


def load_data(src):
    data = json.loads((src / 'data/simple-icons.json').read_text(encoding='utf-8'))
    icons = data if isinstance(data, list) else data.get('icons')
    if not isinstance(icons, list):
        fail('data/simple-icons.json 的格式不认识')
    by_slug = {}
    for icon in icons:
        slug = icon.get('slug')
        if slug:
            by_slug[slug] = icon
    return by_slug


def clean_svg(text, slug):
    """只留一条路径：<svg xmlns viewBox="0 0 24 24"><path d="…"/></svg>（去掉 title、role 等）。"""
    try:
        root = ET.fromstring(text)
    except ET.ParseError as e:
        fail(f'{slug}.svg 不是合法的 XML：{e}')
    if root.tag != f'{{{SVG_NS}}}svg':
        fail(f'{slug}.svg 的根元素不是 svg')
    if root.get('viewBox') != '0 0 24 24':
        fail(f'{slug}.svg 的 viewBox 不是 0 0 24 24')
    paths = [el for el in root if el.tag == f'{{{SVG_NS}}}path']
    others = [el.tag for el in root if el.tag not in (f'{{{SVG_NS}}}path', f'{{{SVG_NS}}}title')]
    if len(paths) != 1 or others:
        fail(f'{slug}.svg 应只有一条 path（另有 {others}）')
    d = paths[0].get('d', '')
    if not re.fullmatch(r'[MmLlHhVvCcSsQqTtAaZz0-9eE.,\- ]+', d):
        fail(f'{slug}.svg 的路径中有不认识的字符')
    return f'<svg xmlns="{SVG_NS}" viewBox="0 0 24 24"><path d="{d}"/></svg>\n'


def license_of(slug, entry):
    """(许可的说明, 是否 CC0)；不能收时报错。"""
    lic = entry.get('license')
    if not lic:
        return 'CC0-1.0 (Simple Icons)', True
    kind = lic.get('type')
    if kind == 'custom':
        if slug not in CUSTOM_LICENSES:
            fail(f'{slug} 的许可是 custom（{lic.get("url")}），确认后加进 CUSTOM_LICENSES')
        return f'custom: {lic.get("url")} — {CUSTOM_LICENSES[slug]}', False
    if kind not in PERMITTED_LICENSES:
        fail(f'{slug} 的许可是 {kind}，不在可以收的范围内')
    if 'author' not in ATTRIBUTIONS.get(slug, {}):
        fail(f'{slug} 的许可是 {kind}，要在 ATTRIBUTIONS 中写明作者')
    if kind == 'MIT' and 'copyright' not in ATTRIBUTIONS[slug]:
        fail(f'{slug} 的许可是 MIT，要在 ATTRIBUTIONS 中写明版权行（copyright）')
    return kind, False


def rust_str(s):
    return '"' + s.replace('\\', '\\\\').replace('"', '\\"') + '"'


def write_text(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, 'w', encoding='utf-8', newline='') as f:
        f.write(text)


def generate(src):
    data = load_data(src)
    rows = []
    for slug in sorted(ICONS):
        variant, purpose = ICONS[slug]
        entry = data.get(slug)
        if entry is None:
            fail(f'Simple Icons {VERSION} 中没有 {slug}')
        hex_color = entry.get('hex', '')
        if not re.fullmatch(r'[0-9A-Fa-f]{6}', hex_color):
            fail(f'{slug} 的品牌色 {hex_color!r} 不是 6 位十六进制')
        svg = clean_svg((src / 'icons' / f'{slug}.svg').read_text(encoding='utf-8'), slug)
        lic, cc0 = license_of(slug, entry)
        rows.append({
            'slug': slug, 'variant': variant, 'purpose': purpose, 'title': entry['title'],
            'hex': hex_color.upper(), 'svg': svg, 'license': lic, 'cc0': cc0,
            'source': entry.get('source', ''), 'guidelines': entry.get('guidelines', ''),
        })
    variants = [r['variant'] for r in rows]
    if len(set(variants)) != len(variants):
        fail('Rust 变体名有重复')

    # 只留 ICONS 中的文件
    ASSETS_DIR.mkdir(parents=True, exist_ok=True)
    for old in ASSETS_DIR.glob('*.svg'):
        if old.stem not in ICONS:
            old.unlink()
    for r in rows:
        write_text(ASSETS_DIR / f'{r["slug"]}.svg', r['svg'])

    cc0_text = (src / 'LICENSE.md').read_text(encoding='utf-8').replace('\r\n', '\n').rstrip()
    lines = [
        f'Termlet 的系统与品牌标志取自 Simple Icons {VERSION}（https://simpleicons.org，'
        'https://github.com/simple-icons/simple-icons）。',
        f'The system and brand logos in Termlet are from Simple Icons {VERSION}.',
        '',
        '这些标志是各自所有者的商标，Termlet 只用它们标明主机运行的系统、本机的 shell 与发起请求的程序，'
        '不表示与这些所有者有任何关联或得到其认可。',
        'These logos are trademarks of their respective owners. Termlet uses them only to identify the system a host '
        'runs, the local shell and the program making a request; this does not imply any affiliation with or '
        'endorsement by those owners.',
        '',
        'Simple Icons 整体以 CC0 1.0 发布（原文附在最后）；个别标志另有许可，见下表中的作者、来源与协议。',
        'Simple Icons is released under CC0 1.0 (full text at the end); some logos have their own licenses: see '
        'the author, source and license listed for them below.',
        '',
        '标志原样使用（只去掉 SVG 中的标题等元数据），以界面的文字颜色或品牌色单色显示；“彩色”风格下品牌色在背景上'
        '看不清时会调亮或调暗。按 CC BY-SA 提供的标志，调整了颜色的版本同样按该协议提供。',
        'The logos are used as-is (only metadata such as the SVG title is removed) and drawn in a single color: the '
        'interface text color or the brand color, which in the "color" style is lightened or darkened when it is '
        'hard to see on the background. For logos under CC BY-SA, such color-adjusted versions are likewise '
        'provided under the same license.',
        '',
    ]
    mit = []
    for r in rows:
        attribution = ATTRIBUTIONS.get(r['slug'], {})
        lines.append(f'- {r["title"]} ({r["slug"]})')
        if 'author' in attribution:
            lines.append(f'  Author: {attribution["author"]}')
        lines.append(f'  License: {r["license"]}')
        if r['license'] in LICENSE_URLS:
            lines.append(f'  License text: {LICENSE_URLS[r["license"]]}')
        if r['license'] == 'MIT':
            lines.append('  License text: the MIT License below')
            mit.append(attribution['copyright'])
        if 'note' in attribution:
            lines.append(f'  Note: {attribution["note"]}')
        if r['source']:
            lines.append(f'  Source: {r["source"]}')
        if r['guidelines']:
            lines.append(f'  Guidelines: {r["guidelines"]}')
    for copyright_line in mit:
        lines += ['', '-' * 72, '', 'MIT License', '', copyright_line, '', MIT_TEXT]
    lines += ['', '-' * 72, '', cc0_text, '']
    write_text(ASSETS_DIR / 'LICENSE', '\n'.join(lines))

    out = [
        f'//! 由 scripts/brand-icons.py 从 Simple Icons {VERSION} 生成，请勿手工修改。',
        '//!',
        '//! 每个标志的资源路径是 `brand-icons/<slug>.svg`（`assets/brand-icons/` 中的文件，单色的一条路径），',
        '//! 品牌色取自 Simple Icons 的数据。许可与商标说明见 `assets/brand-icons/LICENSE`。',
        '',
        '/// 打包的系统与品牌标志。',
        '#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]',
        'pub enum Brand {',
    ]
    for r in rows:
        out.append(f'    /// {r["title"]}（{r["purpose"]}）')
        out.append(f'    {r["variant"]},')
    out += ['}', '', '/// 一个标志的数据。', 'pub(super) struct BrandData {',
            '    /// Simple Icons 中的标识（资源文件名）', '    pub slug: &\'static str,',
            '    /// 资源路径', '    pub path: &\'static str,',
            '    /// 品牌色 `0xRRGGBB`', '    pub hex: u32,',
            '    pub svg: &\'static str,', '}', '']
    out.append('impl Brand {')
    out.append(f'    pub const ALL: [Brand; {len(rows)}] = [')
    for r in rows:
        out.append(f'        Brand::{r["variant"]},')
    out += ['    ];', '}', '']
    out.append('/// 各标志的数据，与 [`Brand`] 的变体一一对应（按变体的顺序）。')
    out.append(f'pub(super) static BRANDS: [BrandData; {len(rows)}] = [')
    for r in rows:
        out += [
            '    BrandData {',
            f'        slug: {rust_str(r["slug"])},',
            f'        path: {rust_str("brand-icons/" + r["slug"] + ".svg")},',
            f'        hex: 0x{r["hex"]},',
            f'        svg: include_str!({rust_str("../../assets/brand-icons/" + r["slug"] + ".svg")}),',
            '    },',
        ]
    out.append('];')
    for line in out:
        if len(line) > 100:
            fail(f'生成的 Rust 代码有一行超过 100 列，rustfmt 会改动它：{line}')
    write_text(TABLE_RS, '\n'.join(out) + '\n')

    print(f'Simple Icons {VERSION}：收录 {len(rows)} 个标志')
    for r in rows:
        note = '' if r['cc0'] else f'（{r["license"]}）'
        print(f'  {r["slug"]:<14} #{r["hex"]}  {len(r["svg"]):>5} 字节  {r["purpose"]}{note}')


def download():
    print(f'下载 {TARBALL_URL}')
    with urllib.request.urlopen(TARBALL_URL, timeout=120) as response:
        data = response.read()
    digest = hashlib.sha256(data).hexdigest()
    if digest != TARBALL_SHA256:
        fail(f'npm 包的 SHA-256 不符：应为 {TARBALL_SHA256}，实际是 {digest}')
    return data


def main():
    parser = argparse.ArgumentParser(description='生成系统与品牌标志（Simple Icons）')
    parser.add_argument('--src', type=Path, help='已解压的 simple-icons npm 包目录（离线生成）')
    args = parser.parse_args()
    if args.src:
        generate(args.src.resolve())
        return
    data = download()
    with tempfile.TemporaryDirectory() as tmp:
        with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as tar:
            if hasattr(tarfile, 'data_filter'):
                tar.extractall(tmp, filter='data')
            else:
                tar.extractall(tmp)
        package = Path(tmp) / 'package'
        if not package.is_dir():
            fail('npm 包中没有 package 目录')
        generate(package)


if __name__ == '__main__':
    main()
