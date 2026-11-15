//! 构建脚本：
//! - 文案在编译时由 rust-i18n 读进程序：`locales/` 下的文件有变化（含新增文件）时重新编译。
//! - Windows：给程序嵌入图标与版本信息。图标的资源 ID 为 1：资源管理器、任务栏与快捷方式显示它，GPUI 也按这个 ID
//!   加载窗口图标；任务管理器按版本信息中的 FileDescription 显示名称。图标由 `packaging/render-icons.mjs` 从
//!   `docs/brand/logo.svg` 渲染。清单（DPI 感知等）由 GPUI 嵌入，这里不再加。

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=locales");
    windows_resources();
}

fn windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let icon = PathBuf::from(env("CARGO_MANIFEST_DIR")).join("resources/windows/termlet.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let numeric = format!(
        "{},{},{},0",
        env("CARGO_PKG_VERSION_MAJOR"),
        env("CARGO_PKG_VERSION_MINOR"),
        env("CARGO_PKG_VERSION_PATCH")
    );
    let version = env("CARGO_PKG_VERSION");
    // 资源脚本中的字符串：反斜杠与引号要转义
    let icon = icon.display().to_string().replace('\\', "\\\\");
    let rc = format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "ruan"
            VALUE "FileDescription", "Termlet"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "Termlet"
            VALUE "LegalCopyright", "Copyright (C) 2026 ruan"
            VALUE "OriginalFilename", "Termlet.exe"
            VALUE "ProductName", "Termlet"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    );
    let out = PathBuf::from(env("OUT_DIR")).join("termlet.rc");
    std::fs::write(&out, rc).expect("写入资源脚本失败");
    // 找不到资源编译器时（例如在别的系统上交叉编译）没有图标，照常构建
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_optional()
        .expect("编译 Windows 资源失败");
}
