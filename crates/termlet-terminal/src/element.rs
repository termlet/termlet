//! 终端的绘制：每帧取可见区的快照（[`GridSnapshot`]，引擎线程填好交给界面，内容没变时就是上一帧的那份），画背景、
//! 选区、文字、光标与输入法的预编辑文字，以及命令补全提示（[`crate::suggest`]，画在光标之后的空白格子上，不在网格
//! 中）。
//!
//! - 相同背景的连续格子合并成一个矩形。
//! - 文字按样式分段：窄字符连成一段，用 `force_width` 让每个字形对齐到一格（含后备字体中的制表线、符号）；
//!   宽字符（中日韩文字等）各自单独一段，放在自己的格子上，占两格。
//! - 鼠标事件在这里注册（拖动选区时捕获指针，移出终端也能继续选），交给 [`TerminalView`] 处理。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{
    App, Bounds, ElementId, ElementInputHandler, Entity, FocusHandle, Font, FontStyle, FontWeight,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollEvent, SharedString,
    StrikethroughStyle, Style, TextRun, UnderlineStyle, Window, fill, outline, point, px, relative,
    size,
};

use crate::colors::{ColorSlot, Rgb, bold_bright, dim, to_hsla};
use crate::grid::{CellFlags, CursorShape, GridPoint, GridSnapshot};
use crate::highlight::{HighlightCache, visible_highlights};
use crate::suggest;
use crate::view::{TerminalStyle, TerminalView};
use termlet_io::GridSize;

/// 回放时网格放不下而缩小字号，最小到这么大（像素）。
const MIN_FIT_FONT_SIZE: f32 = 6.;

/// 布局的结果：格子大小、原点与网格尺寸，鼠标坐标换算与输入法候选框定位都要用。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutMetrics {
    pub bounds: Bounds<Pixels>,
    /// 网格左上角（扣除内边距之后）
    pub origin: gpui_kit::Point<Pixels>,
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub columns: usize,
    pub rows: usize,
}

impl LayoutMetrics {
    /// 窗口坐标对应的格子（可见区的行、列，限制在网格内）与落在格子的哪一半。
    pub fn cell_at(&self, position: gpui_kit::Point<Pixels>) -> (usize, usize, bool) {
        let x = (position.x - self.origin.x).max(px(0.));
        let y = (position.y - self.origin.y).max(px(0.));
        let column = ((x / self.cell_width) as usize).min(self.columns.saturating_sub(1));
        let row = ((y / self.line_height) as usize).min(self.rows.saturating_sub(1));
        let right_half = (x - self.cell_width * column as f32) > self.cell_width / 2.;
        (row, column, right_half)
    }

    pub fn cell_bounds(&self, row: usize, column: usize, width: usize) -> Bounds<Pixels> {
        Bounds::new(
            point(
                self.origin.x + self.cell_width * column as f32,
                self.origin.y + self.line_height * row as f32,
            ),
            size(self.cell_width * width as f32, self.line_height),
        )
    }
}

/// 一段相同样式的文字。
struct TextSegment {
    row: usize,
    column: usize,
    text: String,
    /// 宽字符，占两格
    wide: bool,
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    undercurl: bool,
    strikethrough: bool,
    /// 占几个格子（窄字符一段中的字符数）
    cells: usize,
}

/// 一帧要画的内容。
pub struct Frame {
    metrics: LayoutMetrics,
    background: Hsla,
    backgrounds: Vec<(usize, usize, usize, Hsla)>,
    /// 查找的匹配（画在背景之上、文字之下）
    highlights: Vec<(usize, usize, usize, Hsla)>,
    segments: Vec<TextSegment>,
    /// 鼠标指着的链接（画在文字之下一像素处）
    underlines: Vec<(usize, usize, usize, Hsla)>,
    cursor: Option<CursorPaint>,
    scrollbar: Option<(Bounds<Pixels>, Hsla)>,
    hitbox: Hitbox,
    link_hovered: bool,
}

struct CursorPaint {
    row: usize,
    column: usize,
    wide: bool,
    shape: CursorShape,
    color: Hsla,
    /// 块状光标盖住的字符（用光标下的文字颜色重画）
    text: Option<(String, Hsla)>,
}

pub struct TerminalElement {
    view: Entity<TerminalView>,
    focus: FocusHandle,
    style: TerminalStyle,
    layout: Rc<Cell<Option<LayoutMetrics>>>,
    focused: bool,
    cursor_visible: bool,
    marked_text: Option<String>,
}

impl TerminalElement {
    pub fn new(
        view: Entity<TerminalView>,
        focus: FocusHandle,
        style: TerminalStyle,
        layout: Rc<Cell<Option<LayoutMetrics>>>,
        focused: bool,
        cursor_visible: bool,
        marked_text: Option<String>,
    ) -> Self {
        Self {
            view,
            focus,
            style,
            layout,
            focused,
            cursor_visible,
            marked_text,
        }
    }

    fn font(&self, bold: bool, italic: bool) -> Font {
        Font {
            family: self.style.font_family.clone(),
            features: gpui_kit::FontFeatures::disable_ligatures(),
            fallbacks: self.style.font_fallbacks.clone(),
            weight: if bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            style: if italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            },
        }
    }

    /// 这个字号下格子的宽度与行高。
    fn cell_size(&self, font_size: Pixels, window: &mut Window) -> (Pixels, Pixels) {
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&self.font(false, false));
        let cell_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(font_size * 0.6);
        (cell_width, (font_size * self.style.line_height).round())
    }

    /// 量出格子与网格的大小。`fixed` 是回放时录制的网格大小：网格按它，放不下时缩小字号（改在 `self.style` 中，
    /// 绘制随之用小的字号）。
    fn measure(
        &mut self,
        bounds: Bounds<Pixels>,
        fixed: Option<GridSize>,
        window: &mut Window,
    ) -> LayoutMetrics {
        let (mut cell_width, mut line_height) = self.cell_size(self.style.font_size, window);
        let padding = self.style.padding;
        let inner_width = (bounds.size.width - padding * 2.).max(px(0.));
        let inner_height = (bounds.size.height - padding * 2.).max(px(0.));
        let (columns, rows) = match fixed {
            Some(size) => {
                let (columns, rows) = (size.columns.max(1) as usize, size.rows.max(1) as usize);
                let mut font_size = self.style.font_size;
                // 行高取整，缩一次不一定正好放下：最多再试几次
                for _ in 0..6 {
                    let scale = (inner_width / (cell_width * columns as f32))
                        .min(inner_height / (line_height * rows as f32));
                    if scale >= 1. || font_size <= px(MIN_FIT_FONT_SIZE) {
                        break;
                    }
                    font_size = (font_size * (scale * 0.98)).max(px(MIN_FIT_FONT_SIZE));
                    (cell_width, line_height) = self.cell_size(font_size, window);
                }
                self.style.font_size = font_size;
                (columns, rows)
            }
            None => (
                ((inner_width / cell_width) as usize).max(2),
                ((inner_height / line_height) as usize).max(1),
            ),
        };
        LayoutMetrics {
            bounds,
            origin: point(bounds.origin.x + padding, bounds.origin.y + padding),
            cell_width,
            line_height,
            columns,
            rows,
        }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui_kit::Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Frame;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let fixed = {
            let view = self.view.read(cx);
            view.is_replay().then(|| view.engine().size())
        };
        let metrics = self.measure(bounds, fixed, window);
        self.layout.set(Some(metrics));
        // 网格大小随窗口变化（引擎在大小没变时什么也不做）
        let view = self.view.read(cx);
        view.resize_to(&metrics);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        build_frame(
            view,
            &self.style,
            metrics,
            self.focused,
            self.cursor_visible,
            hitbox,
        )
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        frame: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let metrics = frame.metrics;
        window.with_content_mask(Some(gpui_kit::ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, frame.background));
            for &(row, start, end, color) in frame.backgrounds.iter().chain(&frame.highlights) {
                window.paint_quad(fill(metrics.cell_bounds(row, start, end - start), color));
            }
            self.paint_text(frame, window, cx);
            for &(row, start, end, color) in &frame.underlines {
                let cell = metrics.cell_bounds(row, start, end - start);
                window.paint_quad(fill(
                    Bounds::new(
                        point(cell.origin.x, cell.bottom() - px(2.)),
                        size(cell.size.width, px(1.)),
                    ),
                    color,
                ));
            }
            self.paint_cursor(frame, window, cx);
            if let Some((bounds, color)) = frame.scrollbar {
                window.paint_quad(fill(bounds, color).corner_radii(px(3.)));
            }
        });

        window.handle_input(
            &self.focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
        let cursor = if frame.link_hovered {
            gpui_kit::CursorStyle::PointingHand
        } else {
            gpui_kit::CursorStyle::IBeam
        };
        window.set_cursor_style(cursor, &frame.hitbox);
        self.register_mouse(frame, window);
    }
}

impl TerminalElement {
    fn paint_text(&self, frame: &Frame, window: &mut Window, cx: &mut App) {
        let metrics = frame.metrics;
        let font_size = self.style.font_size;
        for segment in &frame.segments {
            let run = TextRun {
                len: segment.text.len(),
                font: self.font(segment.bold, segment.italic),
                color: segment.color,
                background_color: None,
                underline: (segment.underline || segment.undercurl).then(|| UnderlineStyle {
                    thickness: px(1.),
                    color: Some(segment.color),
                    wavy: segment.undercurl,
                }),
                strikethrough: segment.strikethrough.then(|| StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(segment.color),
                }),
            };
            let force_width = (!segment.wide).then_some(metrics.cell_width);
            let shaped = window.text_system().shape_line(
                SharedString::from(segment.text.clone()),
                font_size,
                &[run],
                force_width,
            );
            let origin = metrics.cell_bounds(segment.row, segment.column, 1).origin;
            let _ = shaped.paint(
                origin,
                metrics.line_height,
                gpui_kit::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }

    fn paint_cursor(&self, frame: &Frame, window: &mut Window, cx: &mut App) {
        let metrics = frame.metrics;
        let Some(cursor) = &frame.cursor else {
            return;
        };
        // 输入法的预编辑文字画在光标处，代替光标
        if let Some(marked) = self.marked_text.as_ref().filter(|t| !t.is_empty()) {
            let width = marked.chars().map(char_cells).sum::<usize>().max(1);
            let bounds = metrics.cell_bounds(cursor.row, cursor.column, width);
            window.paint_quad(fill(bounds, frame.background));
            let run = TextRun {
                len: marked.len(),
                font: self.font(false, false),
                color: cursor.color,
                background_color: None,
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(cursor.color),
                    wavy: false,
                }),
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(marked.clone()),
                self.style.font_size,
                &[run],
                None,
            );
            let _ = shaped.paint(
                bounds.origin,
                metrics.line_height,
                gpui_kit::TextAlign::Left,
                None,
                window,
                cx,
            );
            return;
        }
        let width = if cursor.wide { 2 } else { 1 };
        let cell = metrics.cell_bounds(cursor.row, cursor.column, width);
        let thickness = px(2.);
        match cursor.shape {
            CursorShape::Block if self.focused => {
                window.paint_quad(fill(cell, cursor.color));
                if let Some((text, color)) = &cursor.text {
                    let run = TextRun {
                        len: text.len(),
                        font: self.font(false, false),
                        color: *color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let shaped = window.text_system().shape_line(
                        SharedString::from(text.clone()),
                        self.style.font_size,
                        &[run],
                        (!cursor.wide).then_some(metrics.cell_width),
                    );
                    let _ = shaped.paint(
                        cell.origin,
                        metrics.line_height,
                        gpui_kit::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }
            // 失去焦点时块状光标画成空心
            CursorShape::Block => {
                window.paint_quad(outline(cell, cursor.color, gpui_kit::BorderStyle::Solid));
            }
            CursorShape::Bar => {
                window.paint_quad(fill(
                    Bounds::new(cell.origin, size(thickness, cell.size.height)),
                    cursor.color,
                ));
            }
            CursorShape::Underline => {
                window.paint_quad(fill(
                    Bounds::new(
                        point(cell.origin.x, cell.bottom() - thickness),
                        size(cell.size.width, thickness),
                    ),
                    cursor.color,
                ));
            }
        }
    }

    fn register_mouse(&self, frame: &Frame, window: &mut Window) {
        let hitbox = frame.hitbox.clone();
        let view = self.view.clone();
        let focus = self.focus.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase.bubble() && hitbox.is_hovered(window) {
                focus.focus(window, cx);
                // 终端不处理的按下（右键设为由应用弹出菜单时）接着冒泡：外层的右键菜单在子元素之前注册监听器，
                // 冒泡时排在终端之后，截住就收不到了
                if view.update(cx, |view, cx| view.mouse_down(event, window, cx)) {
                    window.capture_pointer(hitbox.id);
                    cx.stop_propagation();
                }
            }
        });
        let view = self.view.clone();
        let hitbox = frame.hitbox.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase.bubble() {
                let hovered = hitbox.is_hovered(window);
                view.update(cx, |view, cx| view.mouse_move(event, hovered, window, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase.bubble() {
                window.release_pointer();
                view.update(cx, |view, cx| view.mouse_up(event, window, cx));
            }
        });
        let view = self.view.clone();
        let hitbox = frame.hitbox.clone();
        window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
            if phase.bubble() && hitbox.is_hovered(window) {
                view.update(cx, |view, cx| view.scroll_wheel(event, window, cx));
                cx.stop_propagation();
            }
        });
    }
}

/// 字符占几格（输入法预编辑文字的宽度）。
fn char_cells(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c)
        .unwrap_or(1)
        .max(1)
}

/// 取这一帧的快照（复用视图的缓冲），算出要画的全部内容。
fn build_frame(
    view: &TerminalView,
    style: &TerminalStyle,
    metrics: LayoutMetrics,
    focused: bool,
    cursor_visible: bool,
    hitbox: Hitbox,
) -> Frame {
    // 关键词高亮按逻辑行匹配：快照带上跨出可见区的逻辑行（最多上下各一屏）
    let context = if style.highlighter.is_some() {
        metrics.rows
    } else {
        0
    };
    // 命令补全提示：引擎线程随快照读出正在输入的命令，与快照一致（没有开启时不读）
    let suggesting = view.suggesting(focused);
    let fill = |snapshot: &mut GridSnapshot| {
        if suggesting {
            view.engine().snapshot_with_prompt_input(snapshot, context);
        } else {
            view.engine().snapshot(snapshot, context);
        }
        let input = snapshot.prompt_input()?;
        Some((input.cursor, view.complete_input(input)?))
    };
    // 缓冲只在这里借用、用完即还；万一重入（同一个视图同时画两次）就这一帧用新的，不会 panic
    match view.snapshot_buffer().try_borrow_mut() {
        Ok(mut snapshot) => {
            let ghost = fill(&mut snapshot);
            frame_from(
                &snapshot,
                view,
                style,
                metrics,
                focused,
                cursor_visible,
                hitbox,
                ghost,
            )
        }
        Err(_) => {
            let mut snapshot = GridSnapshot::default();
            let ghost = fill(&mut snapshot);
            frame_from(
                &snapshot,
                view,
                style,
                metrics,
                focused,
                cursor_visible,
                hitbox,
                ghost,
            )
        }
    }
}

/// 由快照算出这一帧要画的全部内容。`ghost` 是命令补全提示：从哪个格子（光标处）画起，画什么。
fn frame_from(
    snapshot: &GridSnapshot,
    view: &TerminalView,
    style: &TerminalStyle,
    metrics: LayoutMetrics,
    focused: bool,
    cursor_visible: bool,
    hitbox: Hitbox,
    ghost: Option<(GridPoint, String)>,
) -> Frame {
    let palette = &style.palette;
    let colors = snapshot.colors();
    let background_rgb = palette.query(ColorSlot::Background, colors);
    let background = to_hsla(background_rgb);
    let selection = snapshot.selection();
    let display_offset = snapshot.display_offset();
    let cursor = snapshot.cursor();
    let cursor_point = cursor.point;
    // 方块光标所在的格子在选区的端点上时不画成选中
    let block_cursor =
        (cursor.visible && cursor.shape == CursorShape::Block).then_some(cursor_point);
    let selection_bg = to_hsla(palette.selection_background);
    let selection_fg = palette.selection_foreground.map(to_hsla);
    let (matches, current_match) = view.search_highlights();
    let match_bg = to_hsla(palette.ansi[3]).opacity(0.45);
    let current_bg = to_hsla(palette.ansi[11]).opacity(0.85);
    let link = view.hovered_link();
    let link_color = to_hsla(palette.foreground);
    let mut highlights: Vec<(usize, usize, usize, Hsla)> = Vec::new();
    let mut underlines: Vec<(usize, usize, usize, Hsla)> = Vec::new();

    // 保证文字可读：同一对颜色只算一次
    let mut contrast_cache: std::collections::HashMap<(u32, u32), Rgb> =
        std::collections::HashMap::new();
    let mut backgrounds: Vec<(usize, usize, usize, Hsla)> = Vec::new();
    let mut segments: Vec<TextSegment> = Vec::new();
    let mut cursor_text: Option<(String, bool)> = None;
    let rows = metrics.rows.min(snapshot.screen_lines());
    let columns = metrics.columns.min(snapshot.columns());

    // 关键词高亮：每个可见格子的颜色（按逻辑行缓存匹配结果），颜色取当前配色中的。
    // 缓存只在这里借用、用完即还；万一重入（同一个视图同时画两次）就这一帧不用缓存，不会 panic
    let keywords = style.highlighter.as_ref().and_then(|highlighter| {
        match view.highlight_cache().try_borrow_mut() {
            Ok(mut cache) => visible_highlights(snapshot, highlighter, &mut cache, rows, columns),
            Err(_) => visible_highlights(
                snapshot,
                highlighter,
                &mut HighlightCache::default(),
                rows,
                columns,
            ),
        }
    });
    let keyword_colors: [Rgb; 6] = style
        .highlight_colors
        .unwrap_or_else(|| std::array::from_fn(|i| palette.ansi[i + 1]));

    // 可见区的格子：（可见区的行、列，网格中的位置，格子）
    let visible = (0..rows).flat_map(|row| {
        let line = snapshot.line_of_row(row);
        let cells = snapshot.row(line).unwrap_or_default();
        cells
            .iter()
            .take(columns)
            .enumerate()
            .map(move |(column, cell)| (row, column, GridPoint::new(line, column), cell))
    });
    for (row, column, point, cell) in visible {
        let flags = cell.flags;
        if cell.is_spacer() {
            continue;
        }
        let wide = cell.is_wide();

        let mut fg_color = cell.fg;
        if style.bold_is_bright && flags.contains(CellFlags::BOLD) {
            fg_color = bold_bright(fg_color);
        }
        let mut fg: Rgb = palette.resolve(fg_color, colors);
        // 关键词高亮只换程序没有着色的文字的颜色，之后照常暗淡、调整对比度，选中时用选区的文字色
        if let Some(slot) = keywords
            .as_ref()
            .and_then(|k| k[row * columns + column].checked_sub(1))
        {
            fg = keyword_colors[slot as usize];
        }
        let mut bg: Rgb = palette.resolve(cell.bg, colors);
        if flags.contains(CellFlags::DIM) {
            fg = dim(fg, background_rgb);
        }
        if flags.contains(CellFlags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if let Some(adjust) = &style.contrast {
            let key = (rgb_key(fg), rgb_key(bg));
            fg = *contrast_cache
                .entry(key)
                .or_insert_with(|| (adjust.0)(fg, bg).unwrap_or(fg));
        }
        let mut fg = to_hsla(fg);
        let mut bg_hsla =
            (bg != background_rgb || flags.contains(CellFlags::INVERSE)).then(|| to_hsla(bg));
        let selected = selection.is_some_and(|s| s.contains_cell(point, wide, block_cursor));
        if selected {
            bg_hsla = Some(selection_bg);
            if let Some(selection_fg) = selection_fg {
                fg = selection_fg;
            }
        }
        if flags.contains(CellFlags::HIDDEN) {
            fg = bg_hsla.unwrap_or(background);
        }

        let span = if wide { 2 } else { 1 };
        // 查找的匹配：matches 按位置排列，二分找到可能包含这个格子的那一处
        let ix = matches.partition_point(|m| m.end < point);
        if let Some(found) = matches.get(ix).filter(|m| m.contains(point)) {
            let color = if Some(*found) == current_match {
                current_bg
            } else {
                match_bg
            };
            push_span(&mut highlights, row, column, span, color);
        }
        if link.as_ref().is_some_and(|l| l.contains(point)) {
            push_span(&mut underlines, row, column, span, link_color);
        }
        if let Some(bg) = bg_hsla {
            match backgrounds.last_mut() {
                Some((r, _, end, color)) if *r == row && *end == column && *color == bg => {
                    *end = column + span;
                }
                _ => backgrounds.push((row, column, column + span, bg)),
            }
        }

        // 格子的文字：字符加上组合字符。直接接到所在的一段文字后面，不为每个格子单独分配
        let grapheme = flags.contains(CellFlags::GRAPHEME);
        let zerowidth = if grapheme {
            snapshot.zerowidth(point)
        } else {
            &[]
        };
        if point == cursor_point {
            let text = std::iter::once(cell.c).chain(zerowidth.iter().copied());
            cursor_text = Some((text.collect(), wide));
        }
        if cell.c == ' ' && !grapheme {
            // 空格不画字形，但会打断同一段文字的连续性，下一段从新的位置开始
            if !flags.intersects(CellFlags::ALL_UNDERLINES | CellFlags::STRIKEOUT) {
                continue;
            }
        }
        let bold = flags.contains(CellFlags::BOLD);
        let italic = flags.contains(CellFlags::ITALIC);
        let underline = flags.intersects(
            CellFlags::UNDERLINE
                | CellFlags::DOUBLE_UNDERLINE
                | CellFlags::DOTTED_UNDERLINE
                | CellFlags::DASHED_UNDERLINE,
        );
        let undercurl = flags.contains(CellFlags::UNDERCURL);
        let strikethrough = flags.contains(CellFlags::STRIKEOUT);

        if !wide
            && let Some(last) = segments.last_mut()
            && !last.wide
            && last.row == row
            && last.column + last.cells == column
            && last.color == fg
            && last.bold == bold
            && last.italic == italic
            && last.underline == underline
            && last.undercurl == undercurl
            && last.strikethrough == strikethrough
        {
            last.text.push(cell.c);
            last.text.extend(zerowidth);
            last.cells += 1;
            continue;
        }
        let mut text = String::with_capacity(16);
        text.push(cell.c);
        text.extend(zerowidth);
        segments.push(TextSegment {
            row,
            column,
            text,
            wide,
            color: fg,
            bold,
            italic,
            underline,
            undercurl,
            strikethrough,
            cells: 1,
        });
    }

    // 命令补全提示：从光标处起画在空白的格子上（不在网格中，不影响选区、复制与录制），用次要文字色；
    // 块状光标盖住的第一个字照常画成光标下的文字
    let mut ghost_under_cursor = None;
    if let Some((at, rest)) = &ghost {
        let color = to_hsla(suggest::ghost_color(
            palette.query(ColorSlot::Foreground, colors),
            background_rgb,
        ));
        let cells = suggest::ghost_cells(snapshot, *at, rest, rows, columns);
        if *at == cursor_point {
            ghost_under_cursor = cells.first().map(|cell| (cell.text.clone(), cell.wide));
        }
        for cell in cells {
            if !cell.wide
                && let Some(last) = segments.last_mut()
                && !last.wide
                && last.row == cell.row
                && last.column + last.cells == cell.column
                && last.color == color
            {
                last.text.push_str(&cell.text);
                last.cells += 1;
                continue;
            }
            segments.push(TextSegment {
                row: cell.row,
                column: cell.column,
                text: cell.text,
                wide: cell.wide,
                color,
                bold: false,
                italic: false,
                underline: false,
                undercurl: false,
                strikethrough: false,
                cells: 1,
            });
        }
    }

    // 光标在可见区中的行
    let cursor_row = usize::try_from(cursor_point.line + display_offset as i32).ok();
    let cursor = cursor_row
        .filter(|&row| {
            (cursor_visible || !focused)
                && cursor.visible
                && row < rows
                && cursor_point.column < columns
        })
        .map(|row| {
            let under = cursor_text.clone().unwrap_or((" ".into(), false));
            // 光标所在的空白格子上画着提示的第一个字时，光标盖住的是它
            let (text, wide) = match ghost_under_cursor.clone() {
                Some(ghost) if under.0 == " " && !under.1 => ghost,
                _ => under,
            };
            let cursor_color = to_hsla(palette.query(ColorSlot::Cursor, colors));
            let text_color = to_hsla(palette.cursor_text.unwrap_or(background_rgb));
            CursorPaint {
                row,
                column: cursor_point.column,
                wide,
                shape: cursor.shape,
                color: cursor_color,
                text: (text.trim() != "").then_some((text, text_color)),
            }
        });

    // 滚动条：有回滚时在右边画一条，表示可见区在全部内容中的位置
    let history = snapshot.history_size();
    let scrollbar = (history > 0).then(|| {
        let total = (history + rows) as f32;
        let track = metrics.bounds;
        let height = (track.size.height * (rows as f32 / total)).max(px(16.));
        let travel = track.size.height - height;
        let from_top = (history - display_offset) as f32 / history as f32;
        let bounds = Bounds::new(
            point(track.right() - px(6.), track.top() + travel * from_top),
            size(px(4.), height),
        );
        let alpha = if display_offset > 0 { 0.55 } else { 0.25 };
        (bounds, to_hsla(palette.foreground).opacity(alpha))
    });

    Frame {
        metrics,
        background,
        backgrounds,
        highlights,
        segments,
        underlines,
        cursor,
        scrollbar,
        hitbox,
        link_hovered: link.is_some(),
    }
}

/// 把一个格子加进同一行的连续区间（相邻且颜色相同时合并）。
fn push_span(
    spans: &mut Vec<(usize, usize, usize, Hsla)>,
    row: usize,
    column: usize,
    span: usize,
    color: Hsla,
) {
    match spans.last_mut() {
        Some((r, _, end, c)) if *r == row && *end == column && *c == color => *end = column + span,
        _ => spans.push((row, column, column + span, color)),
    }
}

fn rgb_key(c: Rgb) -> u32 {
    (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32
}
