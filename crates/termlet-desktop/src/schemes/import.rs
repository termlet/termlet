//! 导入终端配色：拖入或选择文件、粘贴文本，或读取本机已装的 Windows Terminal、Xshell、MobaXterm、Tabby 中的配色。
//!
//! 格式由核心自动识别（`termlet_core::schemes::parse_scheme_bytes` 等），这里列出解析出的配色（预览加勾选），
//! 与已有配色同名的改名为“名称 (1)”，默认只勾选不重名的。导入的配色交给打开它的一方（设置中的配色库草稿）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, ExternalPaths, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, rems,
};
use termlet_core::model::ColorScheme;
use termlet_core::schemes::{
    ColorField, ImportWarning, MAX_CUSTOM_SCHEMES, ParseResult, SchemeClient, SchemeFormat,
    installed_color_schemes, parse_scheme_bytes, parse_schemes, to_custom_scheme, unique_name,
};

use super::{SampleHighlights, SchemeSample, ansi_name};
use crate::prose::prose;
use crate::t;

/// 超过这个大小的文件不读。
const MAX_FILE: u64 = 8 << 20;

/// 打开导入对话框要给的东西。
pub struct ImportOptions {
    /// 已有的配色名称（内置与自定义）：同名的改名
    pub taken: Vec<String>,
    /// 还能导入多少套（自定义配色有上限）
    pub remaining: usize,
    pub highlights: Rc<SampleHighlights>,
    pub font_family: SharedString,
    /// 按“导入”时调用，参数是勾选的配色（已改名、颜色已规范）
    pub on_import: Rc<dyn Fn(Vec<ColorScheme>, &mut Window, &mut App)>,
}

/// 打开导入对话框。
pub fn open(options: ImportOptions, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| SchemeImport::new(options, window, cx));
    let width = rems(48.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, _, _| {
        // Enter 与点“导入”相同（能导入时）；导入后由 submit 关闭对话框，这里总是返回 false
        let on_ok = {
            let view = view.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                view.update(cx, |view, cx| {
                    if view.can_submit() {
                        view.submit(window, cx);
                    }
                });
                false
            }
        };
        dialog
            .title(t!("schemes.import.title"))
            .w(width)
            .overlay_closable(false)
            .child(view.clone())
            .on_ok(on_ok)
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    File,
    Paste,
    Installed,
}

impl Source {
    const ALL: [Source; 3] = [Source::File, Source::Paste, Source::Installed];
}

/// 一批配色来自哪里。
#[derive(Clone)]
enum Origin {
    /// 文件名
    File(String),
    Pasted,
    /// 客户端与文件名
    Installed(SchemeClient, String),
}

impl Origin {
    fn label(&self) -> SharedString {
        match self {
            Origin::File(name) => name.clone().into(),
            Origin::Pasted => t!("schemes.import.pasted"),
            Origin::Installed(client, file) => format!("{} · {file}", client_name(*client)).into(),
        }
    }
}

/// 一个文件（或粘贴的文本）的读取结果。
enum Outcome {
    Parsed {
        format: Option<SchemeFormat>,
        warnings: Vec<ImportWarning>,
    },
    TooLarge,
    ReadFailed(String),
}

struct Batch {
    origin: Origin,
    outcome: Outcome,
}

/// 解析出的一套配色。
struct Found {
    /// 在这次读取结果中的位置（重新读取后整批替换）
    key: usize,
    scheme: ColorScheme,
    origin: Origin,
    /// 与已有的配色同名（“只选不重名的”据此排除）
    duplicate: bool,
}

#[derive(Clone, Copy)]
enum Bulk {
    All,
    Clear,
    New,
    Invert,
}

struct SchemeImport {
    options: ImportOptions,
    /// 已有配色名称的小写，判断重名用
    taken: HashSet<String>,
    source: Source,
    text: Entity<TextareaState>,
    /// 解析出的配色；还没有读取时为 `None`
    found: Option<Vec<Found>>,
    batches: Vec<Batch>,
    selected: HashSet<usize>,
    busy: bool,
    /// 读取的序号：每次读取、切换来源时加一，返回时已不是最新的结果丢弃
    read_seq: u64,
    error: Option<SharedString>,
    _read: Option<Task<()>>,
    _text_subscription: Subscription,
}

impl SchemeImport {
    fn new(options: ImportOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .placeholder(t!("schemes.import.paste_placeholder"))
        });
        let text_subscription = cx.subscribe(&text, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let taken = options
            .taken
            .iter()
            .map(|n| n.trim().to_lowercase())
            .collect();
        Self {
            options,
            taken,
            source: Source::File,
            text,
            found: None,
            batches: Vec::new(),
            selected: HashSet::new(),
            busy: false,
            read_seq: 0,
            error: None,
            _read: None,
            _text_subscription: text_subscription,
        }
    }

    fn switch_source(&mut self, source: Source, cx: &mut Context<Self>) {
        if self.source == source {
            return;
        }
        // 之前还没返回的读取作废
        self.read_seq += 1;
        self.busy = false;
        self.source = source;
        cx.notify();
    }

    /// 显示读取结果：默认勾选不重名的。
    fn show(
        &mut self,
        results: Vec<(Origin, Result<ParseResult, Outcome>)>,
        cx: &mut Context<Self>,
    ) {
        let mut found = Vec::new();
        let mut batches = Vec::new();
        for (origin, result) in results {
            match result {
                Ok(parsed) => {
                    for scheme in parsed.schemes {
                        let duplicate = self.taken.contains(&scheme.name.trim().to_lowercase());
                        found.push(Found {
                            key: found.len(),
                            scheme,
                            origin: origin.clone(),
                            duplicate,
                        });
                    }
                    batches.push(Batch {
                        origin,
                        outcome: Outcome::Parsed {
                            format: parsed.format,
                            warnings: parsed.warnings,
                        },
                    });
                }
                Err(outcome) => batches.push(Batch { origin, outcome }),
            }
        }
        self.selected = found
            .iter()
            .filter(|f| !f.duplicate)
            .map(|f| f.key)
            .collect();
        self.found = Some(found);
        self.batches = batches;
        self.error = None;
        self.busy = false;
        cx.notify();
    }

    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        self._read = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| this.read_files(paths, window, cx));
        }));
    }

    /// 在后台读取并解析文件。
    fn read_files(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        self.source = Source::File;
        self.read_seq += 1;
        let seq = self.read_seq;
        self.busy = true;
        cx.notify();
        self._read = Some(cx.spawn_in(window, async move |this, cx| {
            let results = cx
                .background_spawn(async move {
                    paths.iter().map(|path| read_file(path)).collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.read_seq == seq {
                    this.show(results, cx);
                }
            });
        }));
    }

    fn parse_text(&mut self, cx: &mut Context<Self>) {
        self.read_seq += 1;
        let text = self.text.read(cx).value().to_string();
        let parsed = parse_schemes(&text, None);
        self.show(vec![(Origin::Pasted, Ok(parsed))], cx);
    }

    /// 在后台读取本机已装客户端的配色。
    fn read_installed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.read_seq += 1;
        let seq = self.read_seq;
        self.busy = true;
        cx.notify();
        self._read = Some(cx.spawn_in(window, async move |this, cx| {
            let results = cx
                .background_spawn(async move {
                    installed_color_schemes()
                        .into_iter()
                        .map(|source| {
                            let file = file_name(&source.path);
                            let parsed = parse_schemes(&source.content, Some(&source.path));
                            (Origin::Installed(source.client, file), Ok(parsed))
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.read_seq == seq {
                    this.show(results, cx);
                }
            });
        }));
    }

    fn bulk(&mut self, action: Bulk, cx: &mut Context<Self>) {
        let found = self.found.as_deref().unwrap_or_default();
        self.selected = match action {
            Bulk::All => found.iter().map(|f| f.key).collect(),
            Bulk::Clear => HashSet::new(),
            Bulk::New => found
                .iter()
                .filter(|f| !f.duplicate)
                .map(|f| f.key)
                .collect(),
            Bulk::Invert => found
                .iter()
                .map(|f| f.key)
                .filter(|k| !self.selected.contains(k))
                .collect(),
        };
        cx.notify();
    }

    fn picked(&self) -> Vec<&Found> {
        self.found
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|f| self.selected.contains(&f.key))
            .collect()
    }

    /// 勾选的配色最终的名称：与已有的、以及这次排在前面的同名时加编号。
    fn final_names(&self) -> Vec<(usize, String)> {
        let mut used = self.options.taken.clone();
        self.picked()
            .into_iter()
            .map(|f| {
                let name = unique_name(&f.scheme.name, used.iter().map(String::as_str));
                used.push(name.clone());
                (f.key, name)
            })
            .collect()
    }

    /// “导入”按钮是否可用：不在读取中，勾选了，没有超过还能导入的套数。
    fn can_submit(&self) -> bool {
        let picked = self.picked().len();
        !self.busy && picked > 0 && picked <= self.options.remaining
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut used = self.options.taken.clone();
        let mut out = Vec::new();
        for found in self.picked() {
            if let Some(scheme) = to_custom_scheme(&found.scheme, used.iter().map(String::as_str)) {
                used.push(scheme.name.clone());
                out.push(scheme);
            }
        }
        if out.is_empty() {
            self.error = Some(t!("schemes.import.invalid"));
            cx.notify();
            return;
        }
        tracing::info!(count = out.len(), "导入终端配色");
        (self.options.on_import)(out, window, cx);
        window.close_dialog(cx);
    }

    // ------------------------------------------------------------ 渲染

    fn render_source(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        match self.source {
            Source::File => Button::new("scheme-import-drop")
                .outline()
                .w_full()
                .h_auto()
                .py_4()
                .child(
                    v_flex()
                        .items_center()
                        .gap_1()
                        .text_xs()
                        .text_color(muted)
                        .whitespace_normal()
                        .child(Icon::new(IconName::FileUp).size_5())
                        .child(prose(t!("schemes.import.drop")))
                        .child(prose(t!("schemes.import.formats"))),
                )
                .loading(self.busy)
                // 拖着文件经过时标出放的位置（整个对话框都能放）
                .drag_over::<ExternalPaths>(|style, _, _, cx| {
                    style.border_color(cx.theme().primary)
                })
                .on_click(cx.listener(|this, _, window, cx| this.pick_files(window, cx)))
                .into_any_element(),
            Source::Paste => {
                v_flex()
                    .gap_2()
                    .items_start()
                    .child(div().w_full().child(
                        Textarea::new(&self.text).aria_label(t!("schemes.import.from_paste")),
                    ))
                    .child(
                        Button::new("scheme-import-parse")
                            .small()
                            .label(t!("schemes.import.parse"))
                            .disabled(self.text.read(cx).value().trim().is_empty())
                            .on_click(cx.listener(|this, _, _, cx| this.parse_text(cx))),
                    )
                    .into_any_element()
            }
            Source::Installed => h_flex()
                .gap_3()
                .child(
                    Button::new("scheme-import-installed")
                        .small()
                        .label(if self.busy {
                            t!("schemes.import.reading")
                        } else {
                            t!("schemes.import.read_installed")
                        })
                        .loading(self.busy)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.read_installed(window, cx)),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(muted)
                        .child(prose(t!("schemes.import.installed_hint"))),
                )
                .into_any_element(),
        }
    }

    fn render_batches(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, warning, danger) = (theme.muted_foreground, theme.warning, theme.danger);
        v_flex()
            .gap_0p5()
            .text_xs()
            .children(self.batches.iter().map(|batch| {
                let origin = batch.origin.label();
                let line = match &batch.outcome {
                    Outcome::Parsed {
                        format: Some(format),
                        ..
                    } => {
                        t!(
                            "schemes.import.detected",
                            origin = origin,
                            format = format_name(*format)
                        )
                    }
                    Outcome::Parsed { format: None, .. } => {
                        t!("schemes.import.unknown_format", origin = origin)
                    }
                    Outcome::TooLarge => t!("schemes.import.file_too_large", name = origin),
                    Outcome::ReadFailed(error) => {
                        t!("schemes.import.read_failed", name = origin, error = error)
                    }
                };
                let failed = !matches!(batch.outcome, Outcome::Parsed { .. });
                let warnings: Vec<SharedString> = match &batch.outcome {
                    Outcome::Parsed { warnings, .. } => warnings.iter().map(warning_text).collect(),
                    _ => Vec::new(),
                };
                v_flex()
                    .child(
                        div()
                            .text_color(if failed { danger } else { muted })
                            .child(line),
                    )
                    .children(
                        warnings
                            .into_iter()
                            .map(|w| div().pl_3().text_color(warning).child(w)),
                    )
            }))
    }

    fn render_found(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let found = self.found.as_deref().unwrap_or_default();
        if found.is_empty() {
            let text = if self.source == Source::Installed {
                t!("schemes.import.none_installed")
            } else {
                t!("schemes.import.none")
            };
            return div()
                .p_3()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(muted)
                .child(prose(text))
                .into_any_element();
        }
        let names = self.final_names();
        let cards = found
            .iter()
            .map(|f| {
                let key = f.key;
                let on = self.selected.contains(&key);
                let renamed = names
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, name)| name.clone())
                    .filter(|name| *name != f.scheme.name);
                v_flex()
                    .id(ElementId::from(("scheme-import-item", key)))
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(if on { theme.primary } else { theme.border })
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .text_xs()
                            .child(
                                Checkbox::new(ElementId::from(("scheme-import-check", key)))
                                    .checked(on)
                                    .accessibility_label(f.scheme.name.clone())
                                    .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                                        if *checked {
                                            this.selected.insert(key);
                                        } else {
                                            this.selected.remove(&key);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_medium()
                                    .child(f.scheme.name.clone()),
                            )
                            .child(
                                div()
                                    .ml_auto()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(muted)
                                    .child(f.origin.label()),
                            ),
                    )
                    .when_some(renamed, |this, name| {
                        this.child(
                            div()
                                .px_2()
                                .pb_1()
                                .text_xs()
                                .text_color(theme.warning)
                                .child(prose(t!("schemes.import.rename", name = name))),
                        )
                    })
                    .child(SchemeSample::new(
                        &f.scheme,
                        &self.options.highlights,
                        &self.options.font_family,
                    ))
            })
            .collect::<Vec<_>>();
        div()
            .grid()
            .grid_cols(2)
            .gap_2()
            .children(cards)
            .into_any_element()
    }
}

impl Render for SchemeImport {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let source_ix = Source::ALL
            .iter()
            .position(|s| *s == self.source)
            .unwrap_or(0);
        let picked = self.picked().len();
        let total = self.found.as_ref().map_or(0, Vec::len);
        let remaining = self.options.remaining;
        let over = picked > remaining;
        // 高 40rem，窗口矮时缩到窗口内，解析结果在里面滚动
        let height = rems(40.)
            .to_pixels(window.rem_size())
            .min(window.viewport_size().height - rems(8.).to_pixels(window.rem_size()));
        let bulk_button =
            |id: &'static str, label: SharedString, action: Bulk, cx: &Context<Self>| {
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.bulk(action, cx)))
            };
        v_flex()
            .id("scheme-import")
            .h(height)
            .gap_3()
            // 拖入文件：整个对话框都能放
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.read_files(paths.paths().to_vec(), window, cx)
            }))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(prose(t!("schemes.import.hint"))),
            )
            .child(
                h_flex().child(
                    TabBar::new("scheme-import-source")
                        .segmented()
                        .small()
                        .selected_index(source_ix)
                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                            if let Some(source) = Source::ALL.get(*ix) {
                                this.switch_source(*source, cx);
                            }
                        }))
                        .child(Tab::new().label(t!("schemes.import.from_file")))
                        .child(Tab::new().label(t!("schemes.import.from_paste")))
                        .child(Tab::new().label(t!("schemes.import.from_installed"))),
                ),
            )
            .child(self.render_source(cx))
            .when(!self.batches.is_empty(), |this| {
                this.child(self.render_batches(cx))
            })
            .when(total > 0, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .child(div().flex_1().text_color(muted).child(t!(
                            "schemes.import.selected",
                            count = picked,
                            total = total
                        )))
                        .child(bulk_button(
                            "scheme-import-select-all",
                            t!("schemes.import.select_all"),
                            Bulk::All,
                            cx,
                        ))
                        .child(bulk_button(
                            "scheme-import-select-none",
                            t!("schemes.import.select_none"),
                            Bulk::Clear,
                            cx,
                        ))
                        .child(bulk_button(
                            "scheme-import-select-new",
                            t!("schemes.import.select_new"),
                            Bulk::New,
                            cx,
                        ))
                        .child(bulk_button(
                            "scheme-import-select-invert",
                            t!("schemes.import.select_invert"),
                            Bulk::Invert,
                            cx,
                        )),
                )
            })
            .child(
                div()
                    .id("scheme-import-found")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(self.found.is_some(), |this| {
                        this.child(self.render_found(cx))
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(theme.danger)
                            .when(over, |this| {
                                this.child(prose(t!(
                                    "schemes.import.over",
                                    count = remaining,
                                    max = MAX_CUSTOM_SCHEMES
                                )))
                            })
                            .when_some(self.error.clone(), |this, error| this.child(prose(error))),
                    )
                    .child(
                        Button::new("scheme-import-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("scheme-import-apply")
                            .primary()
                            .label(t!("schemes.import.apply", count = picked))
                            .disabled(self.busy || picked == 0 || over)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

/// 在后台线程中读取一个文件并解析。
fn read_file(path: &Path) -> (Origin, Result<ParseResult, Outcome>) {
    let name = file_name(&path.to_string_lossy());
    let origin = Origin::File(name.clone());
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_FILE => return (origin, Err(Outcome::TooLarge)),
        Err(e) => return (origin, Err(Outcome::ReadFailed(e.to_string()))),
        Ok(_) => {}
    }
    match std::fs::read(path) {
        Ok(bytes) => (origin, Ok(parse_scheme_bytes(&bytes, Some(&name)))),
        Err(e) => (origin, Err(Outcome::ReadFailed(e.to_string()))),
    }
}

/// 路径中的文件名（注册表位置等没有文件名时用整个路径）。
fn file_name(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn client_name(client: SchemeClient) -> &'static str {
    match client {
        SchemeClient::WindowsTerminal => "Windows Terminal",
        SchemeClient::Xshell => "Xshell",
        SchemeClient::MobaXterm => "MobaXterm",
        SchemeClient::Tabby => "Tabby",
    }
}

fn format_name(format: SchemeFormat) -> SharedString {
    match format {
        SchemeFormat::WindowsTerminal => t!("schemes.format.windows_terminal"),
        SchemeFormat::Iterm2 => t!("schemes.format.iterm2"),
        SchemeFormat::Xshell => t!("schemes.format.xshell"),
        SchemeFormat::MobaXterm => "MobaXterm".into(),
        SchemeFormat::Base16 => "base16 / base24".into(),
        SchemeFormat::Alacritty => "Alacritty".into(),
        SchemeFormat::Kitty => "Kitty".into(),
        SchemeFormat::Ghostty => "Ghostty".into(),
        SchemeFormat::Xresources => "Xresources".into(),
        SchemeFormat::Tabby => "Tabby".into(),
        SchemeFormat::Gogh => "Gogh".into(),
    }
}

fn field_name(field: ColorField) -> SharedString {
    match field {
        ColorField::Foreground => t!("schemes.field.foreground"),
        ColorField::Background => t!("schemes.field.background"),
        ColorField::Cursor => t!("schemes.field.cursor"),
        ColorField::CursorAccent => t!("schemes.field.cursor_accent"),
        ColorField::Selection => t!("schemes.field.selection_background"),
        ColorField::SelectionForeground => t!("schemes.field.selection_foreground"),
        ColorField::Ansi { color, bright } => ansi_name(color.index() + if bright { 8 } else { 0 }),
    }
}

/// 解析时跳过、补全的内容的说明。
fn warning_text(warning: &ImportWarning) -> SharedString {
    match warning {
        ImportWarning::TooLarge { max_bytes } => {
            t!("schemes.warning.too_large", size = max_bytes / (1 << 20))
        }
        ImportWarning::TooMany { max } => t!("schemes.warning.too_many", max = max),
        ImportWarning::NoSchemes => t!("schemes.warning.no_schemes"),
        ImportWarning::Skipped { count } => t!("schemes.warning.skipped", count = count),
        ImportWarning::BinaryPlist => t!("schemes.warning.binary_plist"),
        ImportWarning::ColorSpace { scheme, space } => {
            t!(
                "schemes.warning.color_space",
                scheme = scheme,
                space = space
            )
        }
        ImportWarning::InvalidColor {
            scheme,
            field,
            value,
        } => t!(
            "schemes.warning.invalid_color",
            scheme = scheme,
            field = field_name(*field),
            value = value
        ),
        ImportWarning::MissingAnsi { scheme, count } => {
            t!(
                "schemes.warning.missing_ansi",
                scheme = scheme,
                count = count
            )
        }
        ImportWarning::MissingBright { scheme, count } => {
            t!(
                "schemes.warning.missing_bright",
                scheme = scheme,
                count = count
            )
        }
        ImportWarning::MissingForeground { scheme } => {
            t!("schemes.warning.missing_foreground", scheme = scheme)
        }
        ImportWarning::MissingBackground { scheme } => {
            t!("schemes.warning.missing_background", scheme = scheme)
        }
        ImportWarning::MissingCursor { scheme } => {
            t!("schemes.warning.missing_cursor", scheme = scheme)
        }
        ImportWarning::MissingSelection { scheme } => {
            t!("schemes.warning.missing_selection", scheme = scheme)
        }
    }
}
