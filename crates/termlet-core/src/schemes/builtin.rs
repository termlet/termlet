//! 内置终端配色的数据：每套从该主题项目自己的终端移植文件取色，`source` 记出处（随包的许可文本按此生成，
//! 见 `licenses.rs`）。
//!
//! 只收宽松许可（MIT、Apache-2.0、X11 等）的配色；不从 iTerm2-Color-Schemes 合集、Tabby 社区包、
//! GPL 项目整理的数据中取色，许可不明的（Monokai 等）不收。ID 发布后不能改（设置、主机中按 ID 引用）。
//! 颜色在编译时检查：写错（不是小写的 `#rrggbb`、16 色不全）时编译失败。

use super::{License, SchemeSource};

/// 一套内置配色的原始数据（编译期常量）。
pub(super) struct RawScheme {
    pub id: &'static str,
    pub name: &'static str,
    pub foreground: &'static str,
    pub background: &'static str,
    pub cursor: &'static str,
    pub cursor_accent: Option<&'static str>,
    pub selection_background: &'static str,
    pub selection_foreground: Option<&'static str>,
    pub ansi: [&'static str; 16],
    pub source: Option<SchemeSource>,
}

/// 没有写出的字段：颜色留空（编译时检查会报错），可选项为无。
const BLANK: RawScheme = RawScheme {
    id: "",
    name: "",
    foreground: "",
    background: "",
    cursor: "",
    cursor_accent: None,
    selection_background: "",
    selection_foreground: None,
    ansi: [""; 16],
    source: None,
};

/// 16 色：普通一组与 bright 一组各 8 个，空格分隔，顺序为 black red green yellow blue magenta cyan white。
const fn ansi(normal: &'static str, bright: &'static str) -> [&'static str; 16] {
    assert!(
        normal.len() == 63 && bright.len() == 63,
        "每组 8 个 #rrggbb，以一个空格分隔"
    );
    let mut out = [""; 16];
    let mut i = 0;
    while i < 16 {
        let group = if i < 8 { normal } else { bright };
        let (_, rest) = group.split_at((i % 8) * 8);
        let (color, _) = rest.split_at(7);
        out[i] = color;
        i += 1;
    }
    out
}

/// GitHub 上的文件地址；分支默认为 main。
macro_rules! gh {
    ($repo:literal, $path:literal) => {
        gh!($repo, $path, "main")
    };
    ($repo:literal, $path:literal, $branch:literal) => {
        concat!("https://github.com/", $repo, "/blob/", $branch, "/", $path)
    };
}

const ONE_HALF: SchemeSource = SchemeSource {
    project: "One Half",
    url: "",
    license: License::Mit,
    copyright: "Copyright (c) 2019 Son A. Pham <sp@sonpham.me>",
};
const SOLARIZED: SchemeSource = SchemeSource {
    project: "Solarized",
    url: gh!("altercation/solarized", "xresources/solarized", "master"),
    license: License::Mit,
    copyright: "Copyright (c) 2011 Ethan Schoonover",
};
const GITHUB: SchemeSource = SchemeSource {
    project: "GitHub Theme (projekt0n/github-theme-contrib)",
    url: "",
    license: License::Mit,
    copyright: "Copyright (c) 2022 projekt0n",
};
const CATPPUCCIN: SchemeSource = SchemeSource {
    project: "Catppuccin for Windows Terminal",
    url: "",
    license: License::Mit,
    copyright: "Copyright (c) 2021 Catppuccin",
};
const TOKYO_NIGHT: SchemeSource = SchemeSource {
    project: "Tokyo Night (folke/tokyonight.nvim)",
    url: "",
    license: License::Apache2,
    // 仓库的 LICENSE 是未填写版权行的 Apache-2.0 原文，这里写项目作者
    copyright: "Copyright (c) Folke Lemaitre",
};
const GRUVBOX: SchemeSource = SchemeSource {
    project: "gruvbox",
    url: gh!("morhetz/gruvbox", "colors/gruvbox.vim", "master"),
    // 仓库没有 LICENSE 文件，README 与 package.json 声明为 MIT/X11
    license: License::Mit,
    copyright: "Copyright (c) Pavel Pertsev <morhetz@gmail.com>",
};
const ROSE_PINE: SchemeSource = SchemeSource {
    project: "Rosé Pine for Windows Terminal",
    url: "",
    license: License::Mit,
    copyright: "Copyright (c) Rosé Pine",
};
const EVERFOREST: SchemeSource = SchemeSource {
    project: "Everforest",
    url: gh!("sainnhe/everforest", "colors/everforest.vim", "master"),
    license: License::Mit,
    copyright: "Copyright (c) 2019 sainnhe",
};
const FLEXOKI: SchemeSource = SchemeSource {
    project: "Flexoki",
    url: "",
    license: License::Mit,
    copyright: "Copyright (c) 2023 Steph Ango",
};
const WINDOWS_TERMINAL: SchemeSource = SchemeSource {
    project: "Windows Terminal",
    url: gh!(
        "microsoft/terminal",
        "src/cascadia/TerminalSettingsModel/defaults.json"
    ),
    license: License::Mit,
    copyright: "Copyright (c) Microsoft Corporation. All rights reserved.",
};

/// 全部内置配色，按设置中列出的顺序。
pub(super) const SCHEMES: &[RawScheme] = &[
    RawScheme {
        // One Half Light 的调整版：加深在白底上看不清的颜色；white / brightWhite 用灰色，
        // 避免程序输出的白色文字在白底上消失
        id: "termlet-light",
        name: "Termlet Light",
        foreground: "#383a42",
        background: "#ffffff",
        cursor: "#526fff",
        cursor_accent: Some("#ffffff"),
        selection_background: "#bfceff",
        ansi: ansi(
            "#383a42 #e45649 #50a14f #c18401 #0184bc #a626a4 #0997b3 #a0a1a7",
            "#696c77 #ca1243 #3f8f3e #986801 #4078f2 #9a1fa0 #0a7f96 #4f525d",
        ),
        source: Some(SchemeSource {
            url: gh!("sonph/onehalf", "iterm/OneHalfLight.itermcolors", "master"),
            ..ONE_HALF
        }),
        ..BLANK
    },
    RawScheme {
        // One Half Dark 的调整版：背景与界面深色内容区一致（#232323）；black 比背景略亮，
        // 用作背景色时（tmux 状态栏等）能看出来；brightBlack 提亮到在背景上清楚可读（命令补全提示等）；
        // bright 一组比普通色略亮，粗体与普通能区分开
        id: "termlet-dark",
        name: "Termlet Dark",
        foreground: "#dcdfe4",
        background: "#232323",
        cursor: "#a3b3cc",
        cursor_accent: Some("#232323"),
        selection_background: "#474e5d",
        ansi: ansi(
            "#3c3f46 #e06c75 #98c379 #e5c07b #61afef #c678dd #56b6c2 #d0d3d9",
            "#7f848e #ef8189 #a9d38c #f0cf8f #7cbdf5 #d48fe8 #6cc5d1 #f5f6f8",
        ),
        source: Some(SchemeSource {
            url: gh!("sonph/onehalf", "iterm/OneHalfDark.itermcolors", "master"),
            ..ONE_HALF
        }),
        ..BLANK
    },
    RawScheme {
        // iTerm2 移植中 brightBlack 与背景相同（看不见），改用同一仓库 kitty 移植的 color8
        id: "one-half-dark",
        name: "One Half Dark",
        foreground: "#dcdfe4",
        background: "#282c34",
        cursor: "#a3b3cc",
        selection_background: "#474e5d",
        ansi: ansi(
            "#282c34 #e06c75 #98c379 #e5c07b #61afef #c678dd #56b6c2 #dcdfe4",
            "#5d677a #e06c75 #98c379 #e5c07b #61afef #c678dd #56b6c2 #dcdfe4",
        ),
        source: Some(SchemeSource {
            url: gh!("sonph/onehalf", "iterm/OneHalfDark.itermcolors", "master"),
            ..ONE_HALF
        }),
        ..BLANK
    },
    RawScheme {
        // Xresources 移植中浅色的写法（base 色对调）；选区用 Solarized 定义的"背景高亮"色 base2
        id: "solarized-light",
        name: "Solarized Light",
        foreground: "#657b83",
        background: "#fdf6e3",
        cursor: "#586e75",
        selection_background: "#eee8d5",
        ansi: ansi(
            "#eee8d5 #dc322f #859900 #b58900 #268bd2 #d33682 #2aa198 #073642",
            "#fdf6e3 #cb4b16 #93a1a1 #839496 #657b83 #6c71c4 #586e75 #002b36",
        ),
        source: Some(SOLARIZED),
        ..BLANK
    },
    RawScheme {
        // 选区用 Solarized 定义的"背景高亮"色 base02
        id: "solarized-dark",
        name: "Solarized Dark",
        foreground: "#839496",
        background: "#002b36",
        cursor: "#93a1a1",
        selection_background: "#073642",
        ansi: ansi(
            "#073642 #dc322f #859900 #b58900 #268bd2 #d33682 #2aa198 #eee8d5",
            "#002b36 #cb4b16 #586e75 #657b83 #839496 #6c71c4 #93a1a1 #fdf6e3",
        ),
        source: Some(SOLARIZED),
        ..BLANK
    },
    RawScheme {
        id: "github-light",
        name: "GitHub Light",
        foreground: "#1f2328",
        background: "#ffffff",
        cursor: "#1f2328",
        selection_background: "#bbdfff",
        ansi: ansi(
            "#24292f #cf222e #116329 #4d2d00 #0969da #8250df #1b7c83 #6e7781",
            "#57606a #a40e26 #1a7f37 #633c01 #218bff #a475f9 #3192aa #8c959f",
        ),
        source: Some(SchemeSource {
            url: gh!(
                "projekt0n/github-theme-contrib",
                "themes/windows_terminal/github_light_default.json"
            ),
            ..GITHUB
        }),
        ..BLANK
    },
    RawScheme {
        id: "github-dark",
        name: "GitHub Dark",
        foreground: "#e6edf3",
        background: "#0d1117",
        cursor: "#e6edf3",
        selection_background: "#1e4273",
        ansi: ansi(
            "#484f58 #ff7b72 #3fb950 #d29922 #58a6ff #bc8cff #39c5cf #b1bac4",
            "#6e7681 #ffa198 #56d364 #e3b341 #79c0ff #d2a8ff #56d4dd #ffffff",
        ),
        source: Some(SchemeSource {
            url: gh!(
                "projekt0n/github-theme-contrib",
                "themes/windows_terminal/github_dark_default.json"
            ),
            ..GITHUB
        }),
        ..BLANK
    },
    RawScheme {
        id: "catppuccin-latte",
        name: "Catppuccin Latte",
        foreground: "#4c4f69",
        background: "#eff1f5",
        cursor: "#dc8a78",
        selection_background: "#acb0be",
        ansi: ansi(
            "#5c5f77 #d20f39 #40a02b #df8e1d #1e66f5 #ea76cb #179299 #acb0be",
            "#acb0be #d20f39 #40a02b #df8e1d #1e66f5 #ea76cb #179299 #bcc0cc",
        ),
        source: Some(SchemeSource {
            url: gh!("catppuccin/windows-terminal", "latte.json"),
            ..CATPPUCCIN
        }),
        ..BLANK
    },
    RawScheme {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        foreground: "#cdd6f4",
        background: "#1e1e2e",
        cursor: "#f5e0dc",
        selection_background: "#585b70",
        ansi: ansi(
            "#45475a #f38ba8 #a6e3a1 #f9e2af #89b4fa #f5c2e7 #94e2d5 #bac2de",
            "#585b70 #f38ba8 #a6e3a1 #f9e2af #89b4fa #f5c2e7 #94e2d5 #a6adc8",
        ),
        source: Some(SchemeSource {
            url: gh!("catppuccin/windows-terminal", "mocha.json"),
            ..CATPPUCCIN
        }),
        ..BLANK
    },
    RawScheme {
        id: "tokyo-night-day",
        name: "Tokyo Night Day",
        foreground: "#3760bf",
        background: "#e1e2e7",
        cursor: "#3760bf",
        selection_background: "#b7c1e3",
        ansi: ansi(
            "#b4b5b9 #f52a65 #587539 #8c6c3e #2e7de9 #9854f1 #007197 #6172b0",
            "#a1a6c5 #f52a65 #587539 #8c6c3e #2e7de9 #7847bd #007197 #3760bf",
        ),
        source: Some(SchemeSource {
            url: gh!(
                "folke/tokyonight.nvim",
                "extras/windows_terminal/tokyonight_day.json"
            ),
            ..TOKYO_NIGHT
        }),
        ..BLANK
    },
    RawScheme {
        id: "tokyo-night",
        name: "Tokyo Night",
        foreground: "#c0caf5",
        background: "#1a1b26",
        cursor: "#c0caf5",
        selection_background: "#283457",
        ansi: ansi(
            "#15161e #f7768e #9ece6a #e0af68 #7aa2f7 #bb9af7 #7dcfff #a9b1d6",
            "#414868 #f7768e #9ece6a #e0af68 #7aa2f7 #9d7cd8 #7dcfff #c0caf5",
        ),
        source: Some(SchemeSource {
            url: gh!(
                "folke/tokyonight.nvim",
                "extras/windows_terminal/tokyonight_night.json"
            ),
            ..TOKYO_NIGHT
        }),
        ..BLANK
    },
    RawScheme {
        // 取 gruvbox.vim 中 g:terminal_color_0..15 的映射（浅色、默认对比度）；选区为 Visual 用的 bg3
        id: "gruvbox-light",
        name: "Gruvbox Light",
        foreground: "#3c3836",
        background: "#fbf1c7",
        cursor: "#3c3836",
        selection_background: "#bdae93",
        ansi: ansi(
            "#fbf1c7 #cc241d #98971a #d79921 #458588 #b16286 #689d6a #7c6f64",
            "#928374 #9d0006 #79740e #b57614 #076678 #8f3f71 #427b58 #3c3836",
        ),
        source: Some(GRUVBOX),
        ..BLANK
    },
    RawScheme {
        // 同上（深色、默认对比度）
        id: "gruvbox-dark",
        name: "Gruvbox Dark",
        foreground: "#ebdbb2",
        background: "#282828",
        cursor: "#ebdbb2",
        selection_background: "#665c54",
        ansi: ansi(
            "#282828 #cc241d #98971a #d79921 #458588 #b16286 #689d6a #a89984",
            "#928374 #fb4934 #b8bb26 #fabd2f #83a598 #d3869b #8ec07c #ebdbb2",
        ),
        source: Some(GRUVBOX),
        ..BLANK
    },
    RawScheme {
        id: "rose-pine-dawn",
        name: "Rosé Pine Dawn",
        foreground: "#575279",
        background: "#faf4ed",
        cursor: "#9893a5",
        selection_background: "#dfdad9",
        ansi: ansi(
            "#f2e9e1 #b4637a #286983 #ea9d34 #56949f #907aa9 #d7827e #575279",
            "#797593 #b4637a #286983 #ea9d34 #56949f #907aa9 #d7827e #575279",
        ),
        source: Some(SchemeSource {
            url: gh!("rose-pine/windows-terminal", "rose-pine-dawn.scheme.json"),
            ..ROSE_PINE
        }),
        ..BLANK
    },
    RawScheme {
        id: "rose-pine",
        name: "Rosé Pine",
        foreground: "#e0def4",
        background: "#191724",
        cursor: "#6e6a86",
        selection_background: "#403d52",
        ansi: ansi(
            "#26233a #eb6f92 #31748f #f6c177 #9ccfd8 #c4a7e7 #ebbcba #e0def4",
            "#908caa #eb6f92 #31748f #f6c177 #9ccfd8 #c4a7e7 #ebbcba #e0def4",
        ),
        source: Some(SchemeSource {
            url: gh!("rose-pine/windows-terminal", "rose-pine.scheme.json"),
            ..ROSE_PINE
        }),
        ..BLANK
    },
    RawScheme {
        // 取 everforest.vim 的终端色映射，调色板见同仓库 autoload/everforest.vim（浅色、medium 背景）；
        // 选区为 Visual 用的 bg_visual
        id: "everforest-light",
        name: "Everforest Light",
        foreground: "#5c6a72",
        background: "#fdf6e3",
        cursor: "#5c6a72",
        selection_background: "#eaedc8",
        ansi: ansi(
            "#5c6a72 #f85552 #8da101 #dfa000 #3a94c5 #df69ba #35a77c #e6e2cc",
            "#5c6a72 #f85552 #8da101 #dfa000 #3a94c5 #df69ba #35a77c #e6e2cc",
        ),
        source: Some(EVERFOREST),
        ..BLANK
    },
    RawScheme {
        // 同上（深色、medium 背景）
        id: "everforest-dark",
        name: "Everforest Dark",
        foreground: "#d3c6aa",
        background: "#2d353b",
        cursor: "#d3c6aa",
        selection_background: "#543a48",
        ansi: ansi(
            "#475258 #e67e80 #a7c080 #dbbc7f #7fbbb3 #d699b6 #83c092 #d3c6aa",
            "#475258 #e67e80 #a7c080 #dbbc7f #7fbbb3 #d699b6 #83c092 #d3c6aa",
        ),
        source: Some(EVERFOREST),
        ..BLANK
    },
    RawScheme {
        id: "flexoki-light",
        name: "Flexoki Light",
        foreground: "#100f0f",
        background: "#fffcf0",
        cursor: "#100f0f",
        cursor_accent: Some("#fffcf0"),
        selection_background: "#cecdc3",
        selection_foreground: Some("#100f0f"),
        ansi: ansi(
            "#100f0f #d14d41 #879a39 #d0a215 #4385be #ce5d97 #3aa99f #fffcf0",
            "#6f6e69 #af3029 #66800b #ad8301 #205ea6 #a02f6f #24837b #f2f0e5",
        ),
        source: Some(SchemeSource {
            url: gh!("kepano/flexoki", "kitty/flexoki_light.conf"),
            ..FLEXOKI
        }),
    },
    RawScheme {
        id: "flexoki-dark",
        name: "Flexoki Dark",
        foreground: "#cecdc3",
        background: "#100f0f",
        cursor: "#cecdc3",
        cursor_accent: Some("#100f0f"),
        selection_background: "#403e3c",
        selection_foreground: Some("#cecdc3"),
        ansi: ansi(
            "#100f0f #af3029 #66800b #ad8301 #205ea6 #a02f6f #24837b #878580",
            "#6f6e69 #d14d41 #879a39 #d0a215 #4385be #ce5d97 #3aa99f #cecdc3",
        ),
        source: Some(SchemeSource {
            url: gh!("kepano/flexoki", "kitty/flexoki_dark.conf"),
            ..FLEXOKI
        }),
    },
    RawScheme {
        // Windows Terminal 的默认配色。原数据没有选区色（Windows Terminal 用半透明白色叠加），
        // 这里与导入时的补法相同，由前景与背景混合
        id: "campbell",
        name: "Campbell",
        foreground: "#cccccc",
        background: "#0c0c0c",
        cursor: "#ffffff",
        selection_background: "#464646",
        ansi: ansi(
            "#0c0c0c #c50f1f #13a10e #c19c00 #0037da #881798 #3a96dd #cccccc",
            "#767676 #e74856 #16c60c #f9f1a5 #3b78ff #b4009e #61d6d6 #f2f2f2",
        ),
        source: Some(WINDOWS_TERMINAL),
        ..BLANK
    },
    RawScheme {
        // Dracula 规范中 ANSI 的 blue 是紫色、magenta 是粉色，保持原样
        id: "dracula",
        name: "Dracula",
        foreground: "#f8f8f2",
        background: "#282a36",
        cursor: "#f8f8f2",
        selection_background: "#44475a",
        ansi: ansi(
            "#21222c #ff5555 #50fa7b #f1fa8c #bd93f9 #ff79c6 #8be9fd #f8f8f2",
            "#6272a4 #ff6e6e #69ff94 #ffffa5 #d6acff #ff92df #a4ffff #ffffff",
        ),
        source: Some(SchemeSource {
            project: "Dracula for Windows Terminal",
            url: gh!("dracula/windows-terminal", "dracula.json", "master"),
            license: License::Mit,
            copyright: "Copyright (c) 2019 thismat",
        }),
        ..BLANK
    },
    RawScheme {
        id: "nord",
        name: "Nord",
        foreground: "#d8dee9",
        background: "#2e3440",
        cursor: "#d8dee9",
        cursor_accent: Some("#2e3440"),
        selection_background: "#4c566a",
        ansi: ansi(
            "#3b4252 #bf616a #a3be8c #ebcb8b #81a1c1 #b48ead #88c0d0 #e5e9f0",
            "#4c566a #bf616a #a3be8c #ebcb8b #81a1c1 #b48ead #8fbcbb #eceff4",
        ),
        source: Some(SchemeSource {
            project: "Nord Alacritty",
            url: gh!("nordtheme/alacritty", "src/nord.yaml"),
            license: License::Mit,
            copyright: "Copyright (c) 2016-present Sven Greb <development@svengreb.de>",
        }),
        ..BLANK
    },
    RawScheme {
        id: "kanagawa-wave",
        name: "Kanagawa Wave",
        foreground: "#dcd7ba",
        background: "#1f1f28",
        cursor: "#c8c093",
        selection_background: "#2d4f67",
        ansi: ansi(
            "#090618 #c34043 #76946a #c0a36e #7e9cd8 #957fb8 #6a9589 #c8c093",
            "#727169 #e82424 #98bb6c #e6c384 #7fb4ca #938aa9 #7aa89f #dcd7ba",
        ),
        source: Some(SchemeSource {
            project: "Kanagawa (rebelot/kanagawa.nvim)",
            url: gh!(
                "rebelot/kanagawa.nvim",
                "extras/windows-terminal/kanagawa.json",
                "master"
            ),
            license: License::Mit,
            copyright: "Copyright (c) 2021 Tommaso Laurenzi",
        }),
        ..BLANK
    },
    RawScheme {
        // Termlet 自调：给视力不好、投影演示的场景。除 black 外，16 色与背景的对比度都不低于 7:1（WCAG AAA）；
        // 选区用深蓝底白字
        id: "high-contrast-light",
        name: "High Contrast Light",
        foreground: "#000000",
        background: "#ffffff",
        cursor: "#0033cc",
        cursor_accent: Some("#ffffff"),
        selection_background: "#1c5cc8",
        selection_foreground: Some("#ffffff"),
        ansi: ansi(
            "#000000 #b00000 #006400 #6b5300 #0040c0 #9c009c #005f6b #545454",
            "#404040 #8b0000 #004d00 #574300 #00008b #7a007a #004a54 #262626",
        ),
        ..BLANK
    },
    RawScheme {
        // Termlet 自调：同上（深色）。black 比背景略亮，用作背景色时能看出来
        id: "high-contrast-dark",
        name: "High Contrast Dark",
        foreground: "#ffffff",
        background: "#000000",
        cursor: "#ffd700",
        cursor_accent: Some("#000000"),
        selection_background: "#1c5cc8",
        selection_foreground: Some("#ffffff"),
        ansi: ansi(
            "#3c3c3c #ff6b6b #4ee44e #ffeb3b #6cb6ff #ff79ff #4fe8f0 #e0e0e0",
            "#a0a0a0 #ff9999 #8cff8c #ffff8c #9ccfff #ffa8ff #8cffff #ffffff",
        ),
        ..BLANK
    },
    RawScheme {
        // xterm 原生的 16 色（XTerm-col.ad 中的 X11 颜色名换算为十六进制），照顾习惯黑底的用户；
        // 前景取 color7（gray90），选区与导入时的补法相同，由前景与背景混合
        id: "xterm-classic",
        name: "Classic Black (xterm)",
        foreground: "#e5e5e5",
        background: "#000000",
        cursor: "#e5e5e5",
        selection_background: "#454545",
        ansi: ansi(
            "#000000 #cd0000 #00cd00 #cdcd00 #0000ee #cd00cd #00cdcd #e5e5e5",
            "#7f7f7f #ff0000 #00ff00 #ffff00 #5c5cff #ff00ff #00ffff #ffffff",
        ),
        source: Some(SchemeSource {
            project: "xterm",
            url: gh!("ThomasDickey/xterm-snapshots", "XTerm-col.ad", "master"),
            license: License::X11,
            copyright: "Copyright 2002-2006,2009 by Thomas E. Dickey",
        }),
        ..BLANK
    },
];

/// 小写的 `#rrggbb`。
const fn is_color(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 7 || b[0] != b'#' {
        return false;
    }
    let mut i = 1;
    while i < 7 {
        if !matches!(b[i], b'0'..=b'9' | b'a'..=b'f') {
            return false;
        }
        i += 1;
    }
    true
}

const fn optional_color(s: Option<&str>) -> bool {
    match s {
        Some(s) => is_color(s),
        None => true,
    }
}

// 编译时检查全部颜色：写错时编译失败，运行时不会遇到不合法的内置配色
const _: () = {
    let mut i = 0;
    while i < SCHEMES.len() {
        let s = &SCHEMES[i];
        assert!(
            !s.id.is_empty() && !s.name.is_empty(),
            "内置配色缺少 ID 或名称"
        );
        assert!(
            is_color(s.foreground)
                && is_color(s.background)
                && is_color(s.cursor)
                && is_color(s.selection_background)
                && optional_color(s.cursor_accent)
                && optional_color(s.selection_foreground),
            "内置配色的颜色必须是小写的 #rrggbb"
        );
        let mut k = 0;
        while k < 16 {
            assert!(is_color(s.ansi[k]), "内置配色的 16 色必须是小写的 #rrggbb");
            k += 1;
        }
        if let Some(source) = &s.source {
            assert!(!source.url.is_empty(), "内置配色的出处缺少地址");
        }
        i += 1;
    }
};
