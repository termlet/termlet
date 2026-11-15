//! 自定义的终端配色（导入的、复制内置配色后修改的）。颜色一律为小写的 `#rrggbb`。

use serde::{Deserialize, Serialize};

use super::{Extra, trimmed};
use crate::Invalid;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ColorScheme {
    pub name: String,
    pub foreground: String,
    pub background: String,
    pub cursor: String,
    pub cursor_accent: String,
    pub selection_background: String,
    pub selection_foreground: String,
    /// 16 色：black、red、green、yellow、blue、magenta、cyan、white，之后是同顺序的 bright 一组
    pub ansi: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl ColorScheme {
    pub(crate) fn normalize(&mut self) -> Result<(), Invalid> {
        trimmed(&mut self.name, "name", 64)?;
        if self.name.is_empty() {
            return Err(Invalid::new("name", "required"));
        }
        if self.ansi.len() != 16 {
            return Err(Invalid::new("ansi", "need_16_colors"));
        }
        for color in [
            &mut self.foreground,
            &mut self.background,
            &mut self.cursor,
            &mut self.selection_background,
        ]
        .into_iter()
        .chain(self.ansi.iter_mut())
        {
            *color = normalize_color(color).ok_or(Invalid::new("color", "invalid"))?;
        }
        for optional in [&mut self.cursor_accent, &mut self.selection_foreground] {
            if !optional.is_empty() {
                *optional = normalize_color(optional).ok_or(Invalid::new("color", "invalid"))?;
            }
        }
        Ok(())
    }
}

/// `#rgb`、`#rrggbb`（大小写均可）→ 小写的 `#rrggbb`。
pub fn normalize_color(value: &str) -> Option<String> {
    let hex = value.trim().strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        6 => Some(format!("#{}", hex.to_ascii_lowercase())),
        3 => Some(format!(
            "#{}",
            hex.chars()
                .flat_map(|c| [c, c])
                .collect::<String>()
                .to_ascii_lowercase()
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_are_normalized() {
        assert_eq!(normalize_color("#ABCDEF").as_deref(), Some("#abcdef"));
        assert_eq!(normalize_color("#fA0").as_deref(), Some("#ffaa00"));
        assert_eq!(normalize_color("red"), None);
        assert_eq!(normalize_color("#12345"), None);
    }
}
