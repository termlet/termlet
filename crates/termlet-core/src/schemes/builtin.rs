//! 内置终端配色的数据：每套从该主题项目自己的终端移植文件取色，`source` 记出处（随包的许可文本按此生成，
//! 见 `licenses.rs`）。
//!
//! 只收宽松许可（MIT、Apache-2.0、X11 等）的配色；不从 iTerm2-Color-Schemes 合集、Tabby 社区包、
//! GPL 项目整理的数据中取色，许可不明的（原版 Monokai、Monokai Pro 等；VS Code 自带的 Monokai 主题是 MIT，收了）
//! 不收。ID 发布后不能改（设置、主机中按 ID 引用）。
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
const AYU: SchemeSource = SchemeSource {
    project: "ayu (ayu-theme/vscode-ayu)",
    url: "",
    license: License::Mit,
    copyright: "Copyright (c) 2016 Ike Kurghinyan",
};
const VSCODE: SchemeSource = SchemeSource {
    project: "Visual Studio Code",
    url: "",
    license: License::Mit,
    copyright: "Copyright (c) 2015 - present Microsoft Corporation",
};
const NIGHTFOX: SchemeSource = SchemeSource {
    project: "Nightfox (EdenEast/nightfox.nvim)",
    url: "",
    license: License::Mit,
    copyright: "Copyright (c) 2021 James Simpson",
};
const KANAGAWA: SchemeSource = SchemeSource {
    project: "Kanagawa (rebelot/kanagawa.nvim)",
    url: "",
    license: License::Mit,
    copyright: "Copyright (c) 2021 Tommaso Laurenzi",
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
        // VS Code 主题 JSON 中的 terminal.*。主题没写终端光标色，与 VS Code 终端相同用前景色；
        // 选区 #abb2bf30 按透明度叠在背景上
        id: "one-dark-pro",
        name: "One Dark Pro",
        foreground: "#abb2bf",
        background: "#282c34",
        cursor: "#abb2bf",
        selection_background: "#41454e",
        ansi: ansi(
            "#3f4451 #e05561 #8cc265 #d18f52 #4aa5f0 #c162de #42b3c2 #d7dae0",
            "#4f5666 #ff616e #a5e075 #f0a45d #4dc4ff #de73ff #4cd1e0 #e6e6e6",
        ),
        source: Some(SchemeSource {
            project: "One Dark Pro (Binaryify/OneDark-Pro)",
            url: gh!("Binaryify/OneDark-Pro", "themes/OneDark-Pro.json", "master"),
            license: License::Mit,
            copyright: "Copyright (c) 2013-2022 Binaryify",
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
        id: "catppuccin-frappe",
        name: "Catppuccin Frappé",
        foreground: "#c6d0f5",
        background: "#303446",
        cursor: "#f2d5cf",
        selection_background: "#626880",
        ansi: ansi(
            "#51576d #e78284 #a6d189 #e5c890 #8caaee #f4b8e4 #81c8be #b5bfe2",
            "#626880 #e78284 #a6d189 #e5c890 #8caaee #f4b8e4 #81c8be #a5adce",
        ),
        source: Some(SchemeSource {
            url: gh!("catppuccin/windows-terminal", "frappe.json"),
            ..CATPPUCCIN
        }),
        ..BLANK
    },
    RawScheme {
        id: "catppuccin-macchiato",
        name: "Catppuccin Macchiato",
        foreground: "#cad3f5",
        background: "#24273a",
        cursor: "#f4dbd6",
        selection_background: "#5b6078",
        ansi: ansi(
            "#494d64 #ed8796 #a6da95 #eed49f #8aadf4 #f5bde6 #8bd5ca #b8c0e0",
            "#5b6078 #ed8796 #a6da95 #eed49f #8aadf4 #f5bde6 #8bd5ca #a5adcb",
        ),
        source: Some(SchemeSource {
            url: gh!("catppuccin/windows-terminal", "macchiato.json"),
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
        id: "rose-pine-moon",
        name: "Rosé Pine Moon",
        foreground: "#e0def4",
        background: "#232136",
        cursor: "#6e6a86",
        selection_background: "#44415a",
        ansi: ansi(
            "#393552 #eb6f92 #3e8fb0 #f6c177 #9ccfd8 #c4a7e7 #ea9a97 #e0def4",
            "#908caa #eb6f92 #3e8fb0 #f6c177 #9ccfd8 #c4a7e7 #ea9a97 #e0def4",
        ),
        source: Some(SchemeSource {
            url: gh!("rose-pine/windows-terminal", "rose-pine-moon.scheme.json"),
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
        // iTerm2 移植中 brightBlack 是 #000000（在背景上看不见），改用同一仓库 termite 移植的 color8
        // （Tomorrow 的注释色）；其余 bright 色与普通色相同，原文如此
        id: "tomorrow-night",
        name: "Tomorrow Night",
        foreground: "#c5c8c6",
        background: "#1d1f21",
        cursor: "#c5c8c6",
        selection_background: "#373b41",
        ansi: ansi(
            "#000000 #cc6666 #b5bd68 #f0c674 #81a2be #b294bb #8abeb7 #ffffff",
            "#969896 #cc6666 #b5bd68 #f0c674 #81a2be #b294bb #8abeb7 #ffffff",
        ),
        source: Some(SchemeSource {
            project: "Tomorrow Theme",
            url: gh!(
                "chriskempson/tomorrow-theme",
                "iTerm2/Tomorrow%20Night.itermcolors",
                "master"
            ),
            license: License::Mit,
            copyright: "Copyright (C) 2011 Chris Kempson",
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
        // vscode-ayu 主题 JSON 中的 terminal.*（由 ayu-colors 生成）。前景 #020a12a3 带透明度，叠在背景上；
        // 主题没写终端光标色与选区色，与 VS Code 终端相同：光标用前景色，选区用 editor.selectionBackground
        // （#035bd626，叠在背景上）
        id: "ayu-light",
        name: "Ayu Light",
        foreground: "#5b6066",
        background: "#f8f9fa",
        cursor: "#5b6066",
        selection_background: "#d3e1f5",
        ansi: ansi(
            "#000000 #f06b6c #6cbf43 #e7a100 #21a1e2 #a176cb #4abc96 #c7c7c7",
            "#686868 #f07171 #86b300 #eba400 #22a4e6 #a37acc #4cbf99 #d1d1d1",
        ),
        source: Some(SchemeSource {
            url: gh!("ayu-theme/vscode-ayu", "ayu-light.json", "master"),
            ..AYU
        }),
        ..BLANK
    },
    RawScheme {
        // 同上（深色）：前景 #fcf8ecbd、选区 #3388ff40 叠在背景上
        id: "ayu-dark",
        name: "Ayu Dark",
        foreground: "#bebcb5",
        background: "#0d1017",
        cursor: "#bebcb5",
        selection_background: "#172e51",
        ansi: ansi(
            "#1b1f29 #f06b73 #70bf56 #fdb04c #4fbfff #d0a1ff #93e2c8 #c7c7c7",
            "#686868 #f07178 #aad94c #ffb454 #59c2ff #d2a6ff #95e6cb #ffffff",
        ),
        source: Some(SchemeSource {
            url: gh!("ayu-theme/vscode-ayu", "ayu-dark.json", "master"),
            ..AYU
        }),
        ..BLANK
    },
    RawScheme {
        // VS Code 的默认浅色主题 Light 2026：16 色是终端的内置默认（terminalColorRegistry.ts），前景取它继承的
        // light_modern.json 的 terminal.foreground，背景取终端所在面板的 panel.background；
        // 选区 #0069cc26 叠在背景上
        id: "vscode-light-2026",
        name: "VS Code Light 2026",
        foreground: "#3b3b3b",
        background: "#fafafd",
        cursor: "#202020",
        cursor_accent: Some("#ffffff"),
        selection_background: "#d5e4f6",
        ansi: ansi(
            "#000000 #cd3131 #107c10 #949800 #0451a5 #bc05bc #0598bc #555555",
            "#666666 #f14c4c #14ce14 #b5ba00 #3b8eea #d670d6 #29b8db #a5a5a5",
        ),
        source: Some(SchemeSource {
            url: gh!(
                "microsoft/vscode",
                "extensions/theme-defaults/themes/2026-light.json"
            ),
            ..VSCODE
        }),
        ..BLANK
    },
    RawScheme {
        // 同上（深色 Dark 2026）：前景取 dark_modern.json 的 terminal.foreground，选区 #3994bc33 叠在背景上
        id: "vscode-dark-2026",
        name: "VS Code Dark 2026",
        foreground: "#cccccc",
        background: "#191a1b",
        cursor: "#bfbfbf",
        selection_background: "#1f323b",
        ansi: ansi(
            "#000000 #cd3131 #0dbc79 #e5e510 #2472c8 #bc3fbc #11a8cd #e5e5e5",
            "#666666 #f14c4c #23d18b #f5f543 #3b8eea #d670d6 #29b8db #e5e5e5",
        ),
        source: Some(SchemeSource {
            url: gh!(
                "microsoft/vscode",
                "extensions/theme-defaults/themes/2026-dark.json"
            ),
            ..VSCODE
        }),
        ..BLANK
    },
    RawScheme {
        // VS Code 自带的 Monokai 主题（theme-monokai，不是 Monokai Pro）：16 色取主题 JSON 中的 terminal.*；
        // 主题没写终端的背景、前景、光标与选区，按 VS Code 终端的回退：背景是 panel.background（主题也没写，
        // 回退到 editor.background），前景是终端默认的 #cccccc，光标用前景色，选区是 editor.selectionBackground
        // （#878b9180，叠在背景上）
        id: "monokai-vscode",
        name: "Monokai (VS Code)",
        foreground: "#cccccc",
        background: "#272822",
        cursor: "#cccccc",
        selection_background: "#575a5a",
        ansi: ansi(
            "#333333 #c4265e #86b42b #b3b42b #6a7ec8 #8c6bc8 #56adbc #e3e3dd",
            "#666666 #f92672 #a6e22e #e2e22e #819aff #ae81ff #66d9ef #f8f8f2",
        ),
        source: Some(SchemeSource {
            url: gh!(
                "microsoft/vscode",
                "extensions/theme-monokai/themes/monokai-color-theme.json"
            ),
            ..VSCODE
        }),
        ..BLANK
    },
    RawScheme {
        id: "dayfox",
        name: "Dayfox",
        foreground: "#3d2b5a",
        background: "#f6f2ee",
        cursor: "#3d2b5a",
        selection_background: "#e7d2be",
        ansi: ansi(
            "#352c24 #a5222f #396847 #ac5402 #2848a9 #6e33ce #287980 #f2e9e1",
            "#534c45 #b3434e #577f63 #b86e28 #4863b6 #8452d5 #488d93 #f4ece6",
        ),
        source: Some(SchemeSource {
            url: gh!(
                "EdenEast/nightfox.nvim",
                "extra/dayfox/windows_terminal.json"
            ),
            ..NIGHTFOX
        }),
        ..BLANK
    },
    RawScheme {
        id: "nightfox",
        name: "Nightfox",
        foreground: "#cdcecf",
        background: "#192330",
        cursor: "#cdcecf",
        selection_background: "#2b3b51",
        ansi: ansi(
            "#393b44 #c94f6d #81b29a #dbc074 #719cd6 #9d79d6 #63cdcf #dfdfe0",
            "#575860 #d16983 #8ebaa4 #e0c989 #86abdc #baa1e2 #7ad5d6 #e4e4e5",
        ),
        source: Some(SchemeSource {
            url: gh!(
                "EdenEast/nightfox.nvim",
                "extra/nightfox/windows_terminal.json"
            ),
            ..NIGHTFOX
        }),
        ..BLANK
    },
    RawScheme {
        // 同一仓库 Windows Terminal 文件中的 KanagawaLotus 光标 #c9cbd1、选区 #b5cbd2 对背景都只有约 1.4:1，
        // 改取 ghostty 移植（16 色与之相同，光标、选区与主题源码一致）
        id: "kanagawa-lotus",
        name: "Kanagawa Lotus",
        foreground: "#545464",
        background: "#f2ecbc",
        cursor: "#43436c",
        selection_background: "#c9cbd1",
        selection_foreground: Some("#43436c"),
        ansi: ansi(
            "#1f1f28 #c84053 #6f894e #77713f #4d699b #b35b79 #597b75 #545464",
            "#8a8980 #d7474b #6e915f #836f4a #6693bf #624c83 #5e857a #43436c",
        ),
        source: Some(SchemeSource {
            url: gh!(
                "rebelot/kanagawa.nvim",
                "extras/ghostty/kanagawa-lotus",
                "master"
            ),
            ..KANAGAWA
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
            url: gh!(
                "rebelot/kanagawa.nvim",
                "extras/windows-terminal/kanagawa.json",
                "master"
            ),
            ..KANAGAWA
        }),
        ..BLANK
    },
    RawScheme {
        // Windows Terminal 的默认配色。原数据没有选区色（Windows Terminal 缺省画不透明的白色、文字自动换成黑或白），
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
