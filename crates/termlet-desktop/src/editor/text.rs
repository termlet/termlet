//! 换行符：编辑器只把 `\n` 当换行（gpui-component 的编辑器按 LF 分行，`\r` 是普通字符）。
//!
//! - LF 与 CRLF 文件在编辑器中统一按 LF 编辑（粘贴进来的 `\r\n`、单独的 `\r` 也换成 `\n`），保存时还原为原文件的换行符；
//! - 混用的文件只把 `\n` 当换行，单独的 `\r` 作为普通字符保留，保存时原样写回；
//! - 不做修改就保存时，写回的内容与原文件逐字节一致。
//!
//! 文件开头的 BOM（核心读出的内容以 U+FEFF 开头，UTF-8、UTF-16 都是）不放进编辑器，保存时加回（[`split_bom`]）：
//! 它看不见，留在编辑器里时 Ctrl+Home 后输入的文字会跑到它前面，保存后文件就不再以 BOM 开头了。

use std::borrow::Cow;

/// 去掉开头的 BOM：返回有没有 BOM 与其余的内容。
pub fn split_bom(text: &str) -> (bool, &str) {
    match text.strip_prefix('\u{feff}') {
        Some(rest) => (true, rest),
        None => (false, text),
    }
}

/// 文件的换行符。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
    /// 混用（含单独的 CR）
    Mixed,
}

/// 检测换行符：全部 LF、全部 CRLF，或混用。
pub fn detect_eol(text: &str) -> Eol {
    if !text.contains('\r') {
        return Eol::Lf;
    }
    let bytes = text.as_bytes();
    // 有单独的 CR，或不带 CR 的 LF
    let mixed = bytes.iter().enumerate().any(|(i, &b)| match b {
        b'\r' => bytes.get(i + 1) != Some(&b'\n'),
        b'\n' => i == 0 || bytes[i - 1] != b'\r',
        _ => false,
    });
    if mixed { Eol::Mixed } else { Eol::Crlf }
}

/// 放进编辑器的文本：CRLF 文件去掉每个换行前的 `\r`，其余原样。
pub fn to_editor(text: &str, eol: Eol) -> Cow<'_, str> {
    match eol {
        Eol::Crlf => Cow::Owned(text.replace("\r\n", "\n")),
        Eol::Lf | Eol::Mixed => Cow::Borrowed(text),
    }
}

/// 保存时写回的文本：按原文件的换行符还原。
pub fn to_disk(text: &str, eol: Eol) -> Cow<'_, str> {
    match eol {
        Eol::Lf => unify_breaks(text),
        Eol::Crlf => Cow::Owned(unify_breaks(text).replace('\n', "\r\n")),
        Eol::Mixed => Cow::Borrowed(text),
    }
}

/// 粘贴进编辑器的文本：LF、CRLF 文件中统一成 `\n`；混用的原样。不需要改动时为 `Cow::Borrowed`。
pub fn normalize_paste(text: &str, eol: Eol) -> Cow<'_, str> {
    match eol {
        Eol::Lf | Eol::Crlf => unify_breaks(text),
        Eol::Mixed => Cow::Borrowed(text),
    }
}

/// 把 `\r\n` 与单独的 `\r` 都换成 `\n`。
fn unify_breaks(text: &str) -> Cow<'_, str> {
    if !text.contains('\r') {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 编辑器中的行（编辑器只按 `\n` 分行）。
    fn lines(text: &str) -> Vec<&str> {
        text.split('\n').collect()
    }

    #[test]
    fn detects_lf_crlf_and_mixed() {
        assert_eq!(detect_eol(""), Eol::Lf);
        assert_eq!(detect_eol("a\nb\n"), Eol::Lf);
        assert_eq!(detect_eol("a\r\nb\r\n"), Eol::Crlf);
        assert_eq!(detect_eol("a\r\nb"), Eol::Crlf);
        assert_eq!(detect_eol("a\r\nb\nc"), Eol::Mixed);
        assert_eq!(detect_eol("a\rb"), Eol::Mixed);
        assert_eq!(detect_eol("a\r\r\n"), Eol::Mixed);
        assert_eq!(detect_eol("\nb\r\n"), Eol::Mixed);
    }

    #[test]
    fn unchanged_text_saves_byte_for_byte() {
        for text in [
            "",
            "a",
            "a\nb\n",
            "a\r\nb\r\n",
            "a\r\nb",
            "a\r\nb\nc\rd\r\n",
            "\r",
            "\n\n",
            "中文\r\n第二行",
        ] {
            let eol = detect_eol(text);
            let editor = to_editor(text, eol);
            assert_eq!(to_disk(&editor, eol), text, "{text:?}");
        }
    }

    #[test]
    fn crlf_file_edits_as_lf_and_saves_as_crlf() {
        let text = "a\r\nb\r\n";
        let eol = detect_eol(text);
        let editor = to_editor(text, eol).into_owned();
        assert_eq!(lines(&editor), ["a", "b", ""]);
        // 新输入的（含粘贴进来的 LF 文本）保存为 CRLF
        let edited = editor + "c\nd";
        assert_eq!(to_disk(&edited, eol), "a\r\nb\r\nc\r\nd");
        // 粘贴的 CRLF 文本先统一成 LF，不会留下多余的 CR
        let pasted = normalize_paste("x\r\ny\rz", eol);
        assert_eq!(pasted, "x\ny\nz");
        assert_eq!(to_disk(&format!("a\n{pasted}"), eol), "a\r\nx\r\ny\r\nz");
    }

    #[test]
    fn mixed_file_keeps_lone_cr_as_text() {
        let text = "a\r\nb\nc\rd";
        let eol = detect_eol(text);
        assert_eq!(eol, Eol::Mixed);
        let editor = to_editor(text, eol).into_owned();
        assert_eq!(lines(&editor)[0], "a\r");
        assert_eq!(lines(&editor)[2], "c\rd");
        let edited = editor + "\ne";
        assert_eq!(to_disk(&edited, eol), "a\r\nb\nc\rd\ne");
        // 粘贴的内容原样保留
        assert!(matches!(
            normalize_paste("x\r\ny", eol),
            Cow::Borrowed("x\r\ny")
        ));
    }

    // BOM 不进编辑器，加回后与原文逐字节一致；只去掉开头的一个
    #[test]
    fn bom_is_kept_out_of_the_editor() {
        assert_eq!(split_bom("\u{feff}a\r\n"), (true, "a\r\n"));
        assert_eq!(split_bom("a\u{feff}"), (false, "a\u{feff}"));
        assert_eq!(split_bom("\u{feff}\u{feff}x"), (true, "\u{feff}x"));
        assert_eq!(split_bom(""), (false, ""));
        let file = "\u{feff}Windows Registry Editor Version 5.00\r\n\r\n";
        let (bom, body) = split_bom(file);
        let eol = detect_eol(body);
        let editor = to_editor(body, eol);
        assert!(!editor.starts_with('\u{feff}'));
        let saved = format!(
            "{}{}",
            if bom { "\u{feff}" } else { "" },
            to_disk(&editor, eol)
        );
        assert_eq!(saved, file);
    }

    #[test]
    fn lf_file_turns_stray_cr_into_line_breaks() {
        let eol = detect_eol("a\nb");
        assert!(matches!(
            normalize_paste("plain\ntext", eol),
            Cow::Borrowed(_)
        ));
        assert_eq!(to_disk("a\r\nb\rc\n", eol), "a\nb\nc\n");
    }
}
