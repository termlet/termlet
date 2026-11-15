#!/usr/bin/env python3
"""生成 SFTP 文件列表的彩色文件图标数据（Catppuccin vscode-icons，MIT 许可）。

用法（在仓库任意位置执行均可，只用 Python 3 标准库）：

    python scripts/file-icons.py              下载固定版本的源码归档，核对 SHA-256 后生成
    python scripts/file-icons.py --src <目录>  用已解压的源码目录（含 src/defaults、icons/）离线生成

生成的文件（都提交进仓库）：

    crates/termlet-desktop/assets/file-icons/mocha.txt   每行一个图标：名称<TAB>压成一行的 Mocha SVG，按名称排序
    crates/termlet-desktop/assets/file-icons/LICENSE     Catppuccin vscode-icons 的许可原文
    crates/termlet-desktop/src/sftp/file_icons_table.rs  文件名、扩展名、文件夹名 → 图标名的表，以及 Mocha → Latte 调色板

只打包 Mocha（深色）风味；Latte（浅色）在应用中按调色板换色得到。生成时逐个核对：
每个收录的图标按调色板换色后都要与 Latte 原文件（规范化空白后）完全一致，否则报错。

映射来自 Catppuccin 的 src/defaults/fileIcons.ts 与 folderIcons.ts，键一律转成小写，
同一个键出现多次时以后出现的为准（与它在 TypeScript 中合并对象的结果相同）。
下面的 EXTRA_* 表是 Termlet 自己补充的映射，只补 Catppuccin 没有覆盖的；已覆盖的会报错。

升级图标集时改 VERSION 与 ARCHIVE_SHA256（下载归档后用 sha256sum 算出），重新生成并检查报告。
"""

import argparse
import hashlib
import io
import re
import sys
import tarfile
import tempfile
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

VERSION = 'v1.26.0'
ARCHIVE_URL = f'https://github.com/catppuccin/vscode-icons/archive/refs/tags/{VERSION}.tar.gz'
ARCHIVE_SHA256 = '740f0de7e530972cc07a17c4a8cc92825fa3cdddc7eb8dcc913a52403df12a47'

ROOT = Path(__file__).resolve().parent.parent
ASSETS_DIR = ROOT / 'crates/termlet-desktop/assets/file-icons'
TABLE_RS = ROOT / 'crates/termlet-desktop/src/sftp/file_icons_table.rs'

DEFAULT_FILE = '_file'
DEFAULT_FOLDER = '_folder'

# ---------------------------------------------------------------- Termlet 的补充映射

# 完整文件名（小写）→ 图标名：服务器上常见、Catppuccin 没有覆盖的文件
EXTRA_FILE_NAMES = {
    # SSH 私钥、主机密钥与信任列表（对应的 .pub 公钥 Catppuccin 已按扩展名 pub 用 key 图标）
    'authorized_keys': 'key',
    'authorized_keys2': 'key',
    'known_hosts': 'key',
    'id_rsa': 'key',
    'id_dsa': 'key',
    'id_ecdsa': 'key',
    'id_ecdsa_sk': 'key',
    'id_ed25519': 'key',
    'id_ed25519_sk': 'key',
    'ssh_host_rsa_key': 'key',
    'ssh_host_ecdsa_key': 'key',
    'ssh_host_ed25519_key': 'key',
    # Shell 的启动脚本
    '.bashrc': 'bash',
    '.bash_profile': 'bash',
    '.bash_login': 'bash',
    '.bash_logout': 'bash',
    '.bash_aliases': 'bash',
    'bash.bashrc': 'bash',
    '.profile': 'bash',
    '.zshrc': 'bash',
    '.zprofile': 'bash',
    '.zshenv': 'bash',
    '.zlogin': 'bash',
    '.zlogout': 'bash',
    # SSH 服务与客户端配置、提权与账户数据库
    'sshd_config': 'security',
    'ssh_config': 'security',
    'sudoers': 'security',
    'passwd': 'security',
    'shadow': 'security',
    'group': 'security',
    'gshadow': 'security',
    # 系统配置（resolv.conf、sysctl.conf 等已按扩展名 conf 用 properties 图标）
    'fstab': 'properties',
    'crontab': 'properties',
    'hosts': 'properties',
    'hosts.allow': 'properties',
    'hosts.deny': 'properties',
    'environment': 'properties',
}

# 扩展名（小写，不带点）→ 图标名
EXTRA_FILE_EXTENSIONS = {
    # systemd 单元（INI 格式，与 conf、ini 同用 properties 图标）
    'service': 'properties',
    'socket': 'properties',
    'timer': 'properties',
    'target': 'properties',
    'mount': 'properties',
    'automount': 'properties',
    'path': 'properties',
    'slice': 'properties',
    # 软件包与压缩格式（gz、xz、zst 等 Catppuccin 已用 zip 图标）
    'deb': 'zip',
    'rpm': 'zip',
    'lz4': 'zip',
    'lzma': 'zip',
    'txz': 'zip',
    # 光盘与虚拟机磁盘映像（img 在 Catppuccin 中是图片，不改）
    'iso': 'binary',
    'qcow2': 'binary',
    'vmdk': 'binary',
    'vdi': 'binary',
    'vhdx': 'binary',
    # 共享库（lib 图标；libssl.so.3 这样的版本号后缀在查找时去掉）
    'so': 'lib',
    'dll': 'lib',
    'dylib': 'lib',
    # 证书与密钥库（pem、crt、key、pfx 等 Catppuccin 已覆盖）
    'p12': 'certificate',
    'csr': 'certificate',
    'der': 'certificate',
    'crl': 'certificate',
    'jks': 'key',
    'ppk': 'key',
    # 锁文件（package-lock、Cargo.lock 等 Catppuccin 按文件名另有图标）
    'lock': 'lock',
}

# 文件夹名（小写）→ 图标名（带 folder_ 前缀）
EXTRA_FOLDER_NAMES = {
    # 密钥与证书目录
    '.ssh': 'folder_security',
    '.gnupg': 'folder_security',
    'ssl': 'folder_security',
    'certs': 'folder_security',
    'pki': 'folder_security',
    # 系统配置目录
    'etc': 'folder_config',
    'conf.d': 'folder_config',
    'sites-available': 'folder_config',
    'sites-enabled': 'folder_config',
    'systemd': 'folder_config',
    'init.d': 'folder_scripts',
    # 工具在家目录下的数据
    '.kube': 'folder_kubernetes',
    '.npm': 'folder_node',
    '.vscode-server': 'folder_vscode',
}

# Catppuccin 中只按 VS Code 语言 ID 关联、没有扩展名的图标：语言 ID → 补上的扩展名
# （取 VS Code 内置语言的扩展名；其中已被别的图标占用的不补，例如 patch 已是 git 图标）
LANGUAGE_EXTENSIONS = {
    'diff': ['diff', 'rej'],
}


def fail(message):
    print(f'错误：{message}', file=sys.stderr)
    sys.exit(1)


# ---------------------------------------------------------------- 解析 TypeScript 对象字面量

TOKEN = re.compile(
    r"""(?P<space>\s+)|(?P<comment>//[^\n]*|/\*.*?\*/)|'(?P<sq>(?:[^'\\\n]|\\.)*)'"""
    r"""|"(?P<dq>(?:[^"\\\n]|\\.)*)"|(?P<ident>[A-Za-z_$][\w$]*)|(?P<punct>[{}\[\]:,])""",
    re.S,
)


def tokenize(text, start):
    """从 start 起切出记号：('str', 值)、('ident', 名称)、('punct', 符号)，跳过空白与注释。"""
    pos = start
    while pos < len(text):
        m = TOKEN.match(text, pos)
        if not m:
            fail(f'无法解析的内容：{text[pos:pos + 40]!r}')
        pos = m.end()
        kind = m.lastgroup
        if kind in ('space', 'comment'):
            continue
        if kind in ('sq', 'dq'):
            yield 'str', re.sub(r'\\(.)', r'\1', m.group(kind))
        else:
            yield kind, m.group(kind)


def parse_object(tokens):
    """解析一个对象：键 → 值，值是对象、字符串数组或字符串。开头的 `{` 已读掉。"""
    obj = {}
    while True:
        kind, value = next(tokens)
        if (kind, value) == ('punct', '}'):
            return obj
        if kind not in ('str', 'ident'):
            fail(f'对象中应为键，实际是 {value!r}')
        key = value
        if key in obj:
            fail(f'重复的键 {key!r}')
        if next(tokens) != ('punct', ':'):
            fail(f'键 {key!r} 后缺少冒号')
        kind, value = next(tokens)
        if (kind, value) == ('punct', '{'):
            obj[key] = parse_object(tokens)
        elif (kind, value) == ('punct', '['):
            obj[key] = parse_string_array(tokens)
        elif kind == 'str':
            obj[key] = value
        else:
            fail(f'键 {key!r} 的值无法解析：{value!r}')
        kind, value = next(tokens)
        if (kind, value) == ('punct', '}'):
            return obj
        if (kind, value) != ('punct', ','):
            fail(f'键 {key!r} 的值后应为逗号，实际是 {value!r}')


def parse_string_array(tokens):
    """解析字符串数组，开头的 `[` 已读掉。"""
    items = []
    while True:
        kind, value = next(tokens)
        if (kind, value) == ('punct', ']'):
            return items
        if kind != 'str':
            fail(f'数组中只应有字符串，实际是 {value!r}')
        items.append(value)
        kind, value = next(tokens)
        if (kind, value) == ('punct', ']'):
            return items
        if (kind, value) != ('punct', ','):
            fail(f'数组元素后应为逗号，实际是 {value!r}')


def parse_defaults(path, name, fields):
    """解析 `const <name>: ... = { ... }`：图标名 → {字段: [字符串]}，只允许 fields 中的字段。"""
    text = path.read_text(encoding='utf-8')
    m = re.search(rf'^const {name}: \w+ = \{{', text, re.M)
    if not m:
        fail(f'{path.name} 中找不到 const {name}')
    tokens = tokenize(text, m.end())
    icons = parse_object(tokens)
    for icon, entry in icons.items():
        if not isinstance(entry, dict):
            fail(f'{path.name} 中 {icon!r} 的值不是对象')
        unknown = set(entry) - set(fields)
        if unknown:
            fail(f'{path.name} 中 {icon!r} 有不认识的字段：{sorted(unknown)}')
    return icons


# ---------------------------------------------------------------- 映射表


def add_mapping(table, key, icon, what, overrides):
    """加入一条映射（键转小写），同一个键已有别的图标时以后来的为准并记下来。"""
    key = key.lower()
    if key in table and table[key] != icon:
        overrides.append(f'{what} {key}：{table[key]} → {icon}')
    table[key] = icon


def resolve_file(name, names, extensions):
    """按完整文件名、再按扩展名从长到短查找（与应用中的规则一致，不含去掉备份后缀）。"""
    if name in names:
        return names[name]
    for i, ch in enumerate(name):
        if ch == '.' and name[i + 1:] in extensions:
            return extensions[name[i + 1:]]
    return None


def build_tables(file_icons, folder_icons, available):
    names, extensions, folders = {}, {}, {}
    overrides, skipped, notes = [], [], []

    for icon, entry in file_icons.items():
        for ext in entry.get('fileExtensions', []):
            if '/' in ext:
                # 带目录的关联（如 workflows/yml）只按名称查找时用不上
                skipped.append(f'扩展名 {ext}（{icon}）')
                continue
            add_mapping(extensions, ext, icon, '扩展名', overrides)
        for file_name in entry.get('fileNames', []):
            add_mapping(names, file_name, icon, '文件名', overrides)
    for icon, entry in folder_icons.items():
        for folder in entry.get('folderNames', []):
            add_mapping(folders, folder, f'folder_{icon}', '文件夹', overrides)

    # 只有语言 ID 的图标：按 LANGUAGE_EXTENSIONS 补扩展名，其余语言 ID 忽略
    for icon, entry in file_icons.items():
        if entry.get('fileExtensions') or entry.get('fileNames'):
            continue
        for language in entry.get('languageIds', []):
            added = []
            for ext in LANGUAGE_EXTENSIONS.get(language, []):
                if ext in extensions:
                    notes.append(f'语言 {language} 的扩展名 {ext} 已是 {extensions[ext]} 图标，不补')
                else:
                    extensions[ext] = icon
                    added.append(ext)
            if added:
                notes.append(f'图标 {icon} 只按语言 ID {language} 关联，补上扩展名 {", ".join(added)}')
            else:
                notes.append(f'图标 {icon} 只按语言 ID {language} 关联，忽略')

    # Termlet 的补充映射
    for key, icon in EXTRA_FILE_NAMES.items():
        if key != key.lower():
            fail(f'补充的文件名 {key} 应为小写')
        if key in names:
            fail(f'补充的文件名 {key} Catppuccin 已有（{names[key]}），删去这一项')
        current = resolve_file(key, names, extensions)
        if current == icon:
            fail(f'补充的文件名 {key} 已按扩展名得到 {icon} 图标，删去这一项')
        if current:
            notes.append(f'补充的文件名 {key} 改用 {icon}（原按扩展名为 {current}）')
    for key, icon in EXTRA_FILE_EXTENSIONS.items():
        if key != key.lower() or key.startswith('.'):
            fail(f'补充的扩展名 {key} 应为小写、不带点')
        if key in extensions:
            fail(f'补充的扩展名 {key} Catppuccin 已有（{extensions[key]}），删去这一项')
        current = resolve_file('x.' + key, {}, extensions)
        if current == icon:
            fail(f'补充的扩展名 {key} 已按更短的扩展名得到 {icon} 图标，删去这一项')
    for key, icon in EXTRA_FOLDER_NAMES.items():
        if key != key.lower():
            fail(f'补充的文件夹名 {key} 应为小写')
        if key in folders:
            fail(f'补充的文件夹名 {key} Catppuccin 已有（{folders[key]}），删去这一项')
    names.update(EXTRA_FILE_NAMES)
    extensions.update(EXTRA_FILE_EXTENSIONS)
    folders.update(EXTRA_FOLDER_NAMES)

    for table, what in ((names, '文件名'), (extensions, '扩展名'), (folders, '文件夹名')):
        for key, icon in table.items():
            if icon not in available:
                fail(f'{what} {key} 引用的图标 {icon} 不存在')
            if icon.endswith('_open'):
                fail(f'{what} {key} 引用了展开状态的图标 {icon}')
            if not re.fullmatch(r'[\x21-\x7e]+', key) or '"' in key or '\\' in key:
                fail(f'{what} {key!r} 含有不支持的字符')
    return names, extensions, folders, overrides, skipped, notes


# ---------------------------------------------------------------- SVG 与调色板

HEX = re.compile(r'#[0-9a-f]{6}(?![0-9A-Za-z])')
CSS_VAR = re.compile(r'var\(--vscode-ctp-(\w+)\)')


def compact_svg(svg, path):
    """把 SVG 压成一行：去掉注释、标签之间的空白，其余连续空白换成一个空格。"""
    if re.search(r'<(text|tspan|textPath)\b', svg):
        fail(f'{path} 中有文本元素，压缩空白可能改变显示')
    svg = re.sub(r'<!--.*?-->', '', svg, flags=re.S)
    svg = re.sub(r'<\?xml[^>]*\?>', '', svg)
    svg = re.sub(r'>\s+<', '><', svg)
    svg = re.sub(r'\s+', ' ', svg).strip()
    svg = svg.replace(' />', '/>')
    if not svg.startswith('<svg') or not svg.endswith('</svg>'):
        fail(f'{path} 不是以 <svg 开头、</svg> 结尾')
    try:
        root = ET.fromstring(svg)
    except ET.ParseError as e:
        fail(f'{path} 压缩后不是合法的 XML：{e}')
    if root.tag != '{http://www.w3.org/2000/svg}svg':
        fail(f'{path} 的根元素不是 svg')
    return svg


def derive_palette(icons_dir, icon_names):
    """对比所有图标的 Mocha 与 Latte 文件，按出现位置配对颜色，得出 Mocha 色 → Latte 色。

    换色的颜色必须一一对应；没有换的颜色（遮罩、裁剪路径里的白色等）不能是会被换掉的 Mocha 色。
    css-variables 风味中同一位置是 var(--vscode-ctp-<名称>)，用来给调色板标上 Catppuccin 的颜色名。
    """
    palette, kept, color_names = {}, set(), {}
    for name in icon_names:
        mocha = (icons_dir / 'mocha' / f'{name}.svg').read_text(encoding='utf-8')
        latte = (icons_dir / 'latte' / f'{name}.svg').read_text(encoding='utf-8')
        mocha_colors = HEX.findall(mocha)
        latte_colors = HEX.findall(latte)
        if len(mocha_colors) != len(latte_colors):
            fail(f'{name}.svg 的 Mocha 与 Latte 颜色个数不同')
        for m, l in zip(mocha_colors, latte_colors):
            if m == l:
                kept.add(m)
                continue
            if palette.setdefault(m, l) != l:
                fail(f'Mocha 色 {m} 对应了两个 Latte 色：{palette[m]}、{l}（{name}.svg）')
        css_path = icons_dir / 'css-variables' / f'{name}.svg'
        if css_path.exists():
            css = css_path.read_text(encoding='utf-8')
            css_tokens = re.findall(rf'{HEX.pattern}|{CSS_VAR.pattern}', css)
            if len(css_tokens) == len(mocha_colors):
                for m, var in zip(mocha_colors, css_tokens):
                    if var:
                        if color_names.setdefault(m, var) != var:
                            fail(f'Mocha 色 {m} 对应了两个颜色名：{color_names[m]}、{var}')
    latte_values = list(palette.values())
    if len(set(latte_values)) != len(latte_values):
        fail('调色板不是一一对应：有两个 Mocha 色换成同一个 Latte 色')
    clash = kept & set(palette)
    if clash:
        fail(f'这些颜色有的位置换色、有的位置不换，不能按调色板统一替换：{sorted(clash)}')
    if set(palette) & set(latte_values):
        fail('调色板的 Mocha 色与 Latte 色有重叠，逐个替换会串色')
    return palette, color_names


def recolor(svg, palette):
    return HEX.sub(lambda m: palette.get(m.group(), m.group()), svg)


# ---------------------------------------------------------------- 输出


def rust_str(s):
    return '"' + s.replace('\\', '\\\\').replace('"', '\\"') + '"'


def rust_table(name, doc, table, comments=None):
    lines = [f'/// {doc}', f'pub(super) static {name}: &[(&str, &str)] = &[']
    for key in sorted(table):
        line = f'    ({rust_str(key)}, {rust_str(table[key])}),'
        if comments and key in comments:
            line += f' // {comments[key]}'
        if len(line) > 100:
            fail(f'{name} 中 {key} 这一行超过 100 列，rustfmt 会改动它')
        lines.append(line)
    lines.append('];')
    return '\n'.join(lines)


def write_text(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, 'w', encoding='utf-8', newline='') as f:
        f.write(text)


def generate(src):
    defaults = src / 'src/defaults'
    icons_dir = src / 'icons'
    file_icons = parse_defaults(defaults / 'fileIcons.ts', 'fileIcons',
                                ('languageIds', 'fileExtensions', 'fileNames'))
    folder_icons = parse_defaults(defaults / 'folderIcons.ts', 'folderIcons', ('folderNames',))
    available = {p.stem for p in (icons_dir / 'mocha').glob('*.svg')}
    if {p.stem for p in (icons_dir / 'latte').glob('*.svg')} != available:
        fail('Mocha 与 Latte 的图标文件不一致')

    names, extensions, folders, overrides, skipped, notes = build_tables(
        file_icons, folder_icons, available)
    included = sorted(set(names.values()) | set(extensions.values()) | set(folders.values())
                      | {DEFAULT_FILE, DEFAULT_FOLDER})

    # 调色板从全部图标推出（覆盖更全），核对只针对收录的图标
    palette, color_names = derive_palette(icons_dir, sorted(available))
    mocha_lines, mismatched = [], []
    for name in included:
        mocha = compact_svg((icons_dir / 'mocha' / f'{name}.svg').read_text(encoding='utf-8'),
                            f'mocha/{name}.svg')
        latte = compact_svg((icons_dir / 'latte' / f'{name}.svg').read_text(encoding='utf-8'),
                            f'latte/{name}.svg')
        if recolor(mocha, palette) != latte:
            mismatched.append(name)
        mocha_lines.append(f'{name}\t{mocha}\n')
    if mismatched:
        fail('这些图标按调色板换色后与 Latte 原文件不一致：' + '、'.join(mismatched))

    mocha_txt = ''.join(mocha_lines)
    write_text(ASSETS_DIR / 'mocha.txt', mocha_txt)

    license_text = (src / 'LICENSE').read_text(encoding='utf-8').replace('\r\n', '\n')
    write_text(ASSETS_DIR / 'LICENSE',
               f'Termlet 的 SFTP 文件图标取自 Catppuccin vscode-icons {VERSION}'
               f'（https://github.com/catppuccin/vscode-icons），以下是它的许可原文。\n'
               f'The SFTP file icons in Termlet are from Catppuccin vscode-icons {VERSION};'
               f' its license follows.\n\n{license_text.rstrip()}\n')

    rs = '\n\n'.join([
        f'//! 由 scripts/file-icons.py 从 Catppuccin vscode-icons {VERSION}（MIT）生成，请勿手工修改。\n'
        '//!\n'
        '//! 键都是小写，各表按键排序（供二分查找）；图标名对应 `assets/file-icons/mocha.txt` 中的一行。',
        '/// 没有对应图标时的文件图标。\n'
        f'pub(super) const DEFAULT_FILE: &str = {rust_str(DEFAULT_FILE)};\n'
        '/// 没有对应图标时的文件夹图标，返回上一级（“..”）也用它。\n'
        f'pub(super) const DEFAULT_FOLDER: &str = {rust_str(DEFAULT_FOLDER)};',
        rust_table('FILE_NAMES', '完整文件名 → 图标名。', names),
        rust_table('FILE_EXTENSIONS', '扩展名（不带点，可以有多段，如 `tar.gz`）→ 图标名。', extensions),
        rust_table('FOLDER_NAMES', '文件夹名 → 图标名。', folders),
        rust_table('PALETTE_LATTE', 'Mocha 色 → Latte 色：浅色界面的图标由 Mocha 的 SVG 换色得到。',
                   palette, color_names),
    ]) + '\n'
    write_text(TABLE_RS, rs)

    folder_count = sum(n.startswith('folder_') or n == DEFAULT_FOLDER for n in included)
    print(f'Catppuccin vscode-icons {VERSION}')
    print(f'收录图标 {len(included)} 个：文件 {len(included) - folder_count}（含默认的 {DEFAULT_FILE}），'
          f'文件夹 {folder_count}（含默认的 {DEFAULT_FOLDER}）')
    print(f'映射：文件名 {len(names)}，扩展名 {len(extensions)}，文件夹名 {len(folders)}；'
          f'Termlet 补充 {len(EXTRA_FILE_NAMES)} + {len(EXTRA_FILE_EXTENSIONS)} + {len(EXTRA_FOLDER_NAMES)}')
    print(f'调色板 {len(palette)} 色，收录的图标换色后都与 Latte 原文件一致')
    print(f'mocha.txt {len(mocha_txt.encode("utf-8"))} 字节')
    unused = sorted(n for n in available - set(included) if not n.endswith('_open'))
    for title, items in (('同名键以后出现的为准', overrides), ('跳过', skipped), ('说明', notes),
                         ('没有被引用、未收录的图标', unused)):
        if items:
            print(f'{title}：')
            for item in items:
                print(f'  {item}')


def download():
    print(f'下载 {ARCHIVE_URL}')
    with urllib.request.urlopen(ARCHIVE_URL, timeout=60) as response:
        data = response.read()
    digest = hashlib.sha256(data).hexdigest()
    if digest != ARCHIVE_SHA256:
        fail(f'归档的 SHA-256 不符：应为 {ARCHIVE_SHA256}，实际是 {digest}')
    return data


def main():
    parser = argparse.ArgumentParser(description='生成 SFTP 文件列表的彩色文件图标数据')
    parser.add_argument('--src', type=Path, help='已解压的 Catppuccin vscode-icons 源码目录（离线生成）')
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
        tops = [p for p in Path(tmp).iterdir() if p.is_dir()]
        if len(tops) != 1:
            fail('归档的顶层应只有一个目录')
        generate(tops[0])


if __name__ == '__main__':
    main()
