//! 程序的 logo（与 `docs/brand/logo.svg` 相同）：标题栏、开始页、锁屏、设置的“关于”中显示。
//!
//! logo 是带渐变的彩色 SVG，用 `img` 栅格化显示（`svg()` 只能单色）。原图的画布是 1024，按原尺寸栅格化太大，
//! 改成 128 像素：标题栏、“关于”里的小图在高分屏上也清楚，开始页的大图也够用。

use std::sync::{Arc, LazyLock};

use gpui_kit::{Image, ImageFormat, Img, Rems, Styled as _, img};

static LOGO: LazyLock<Arc<Image>> = LazyLock::new(|| {
    let svg = include_str!("../../../docs/brand/logo.svg").replacen(
        "<svg ",
        r#"<svg width="128" height="128" "#,
        1,
    );
    Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()))
});

/// 边长为 `size` 的 logo。
pub fn logo(size: Rems) -> Img {
    img(LOGO.clone()).size(size).flex_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_is_rasterized_at_a_small_size() {
        let svg = std::str::from_utf8(LOGO.bytes()).unwrap();
        assert!(svg.starts_with(r#"<svg width="128" height="128" xmlns="#));
        assert!(svg.contains(r#"viewBox="0 0 1024 1024""#));
    }
}
