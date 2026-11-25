//! 应用的资源：Termlet 用到的图标在 gpui-kit 的默认图标之外按名单嵌入，不带整套 Lucide。

use std::borrow::Cow;

use gpui_kit::assets::icon_assets;
use gpui_kit::{AssetSource, Result, SharedString};

use crate::brand_icons;
use crate::sftp::file_icons;

// 默认图标（gpui_kit::assets::Assets）没有、Termlet 又用到的 Lucide 图标
icon_assets!(
    ExtraIcons,
    [
        Activity,
        AppWindow,
        ArrowDownLeft,
        ArrowDownToLine,
        ArrowDownUp,
        ArrowLeftToLine,
        ArrowRightToLine,
        ArrowUpRight,
        BrushCleaning,
        Cable,
        CircleArrowUp,
        Clapperboard,
        Clock,
        Cloud,
        Container,
        DatabaseBackup,
        ArrowRightLeft,
        Download,
        FastForward,
        FileDown,
        FilePen,
        FileUp,
        FingerprintPattern,
        FolderInput,
        FolderPlus,
        FolderUp,
        GitCompare,
        Highlighter,
        House,
        Keyboard,
        KeyRound,
        Laptop,
        Layers,
        LayoutGrid,
        Link,
        ListFilter,
        ListTree,
        Lock,
        LocateFixed,
        LocateOff,
        PanelTop,
        Pencil,
        PictureInPicture2,
        Pin,
        PinOff,
        Power,
        PowerOff,
        Radar,
        Regex,
        RotateCcw,
        Save,
        ScrollText,
        Send,
        Server,
        ServerCog,
        ShieldCheck,
        Skull,
        SlidersHorizontal,
        SquarePen,
        Sparkles,
        Terminal,
        TextWrap,
        Trash,
        Unplug,
        Upload,
        UserRound,
        Users,
        WholeWord,
        Zap
    ]
);

/// 先找 SFTP 的彩色文件图标（`file-icons/`）、系统与品牌标志（`brand-icons/`）与 Termlet 额外的图标，
/// 再找 gpui-kit 的默认资源。
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(data) = file_icons::load_asset(path) {
            return Ok(Some(data));
        }
        if let Some(data) = brand_icons::load_asset(path) {
            return Ok(Some(data));
        }
        if let Some(data) = ExtraIcons.load(path)? {
            return Ok(Some(data));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = file_icons::list_assets(path);
        paths.extend(brand_icons::list_assets(path));
        paths.extend(ExtraIcons.list(path)?);
        paths.extend(gpui_kit::assets::Assets.list(path)?);
        Ok(paths)
    }
}
