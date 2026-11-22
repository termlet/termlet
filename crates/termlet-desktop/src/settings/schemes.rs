//! 设置中的“终端配色”：界面为浅色、深色时各用哪套（任何配色都能放进任一个槽）、保证文字可读，
//! 以及配色库（卡片，可搜索、按深浅筛选；导入、导出、复制为自定义、编辑、删除）。
//!
//! 与设置的其他分页一样修改的是草稿：槽与“保证文字可读”在保存设置时写进终端设置，自定义配色的增、改、删
//! 也在保存时写进存储（[`SchemesPage::commit`]）。新建的配色保存前用临时 ID（`new:<n>`），槽可以指向它，
//! 保存时换成存储分配的 ID。删除的配色在保存时一并清掉主机、分组对它的引用（改回跟随全局设置）。

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonRounded, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::form::field;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::CoreError;
use termlet_core::id::SchemeId;
use termlet_core::model::{
    ColorScheme, DEFAULT_SCHEME_DARK, DEFAULT_SCHEME_LIGHT, Group, Host, Settings, TerminalSettings,
};
use termlet_core::schemes::{MAX_CUSTOM_SCHEMES, MIN_CONTRAST_ON, find_scheme, to_custom_scheme};
use termlet_core::store::{Position, Store};

use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state};
use crate::prose::prose;
use crate::schemes::{
    SampleHighlights, SchemeEntry, SchemeKind, SchemeSample, SchemeSwatch, builtin_entries, editor,
    export, import, mode_label, sample_font, unreadable_names,
};
use crate::t;

/// 新建、还没有保存的配色的临时 ID 的前缀（内置配色与存储的 ID 中都没有冒号）。
const NEW_PREFIX: &str = "new:";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Light,
    Dark,
}

impl Filter {
    const ALL: [Filter; 3] = [Filter::All, Filter::Light, Filter::Dark];
}

/// 一套自定义配色（草稿）。
struct Custom {
    /// 存储中的 ID，或新建的临时 ID（`new:<n>`）
    id: String,
    scheme: ColorScheme,
}

type SlotSelect = Entity<ChoiceState<String>>;

pub(super) struct SchemesPage {
    light: String,
    dark: String,
    min_contrast: bool,
    /// 打开时的最低对比度：没有改动开关时原样保留（同步来的可能不是 4.5）
    initial_min_contrast: f32,
    /// 自定义配色：存储中的按存储的顺序，新建的在后
    custom: Vec<Custom>,
    /// 存储中的自定义配色（打开时的，保存后更新）：保存时据此比较出新增、修改与删除
    stored: Vec<(SchemeId, ColorScheme)>,
    next_new: u64,
    query: Entity<InputState>,
    filter: Filter,
    light_select: SlotSelect,
    dark_select: SlotSelect,
    highlights: Rc<SampleHighlights>,
    font_family: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl SchemesPage {
    pub(super) fn new(settings: &Settings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = AppState::store(cx);
        let stored: Vec<(SchemeId, ColorScheme)> = store
            .list::<ColorScheme>()
            .into_iter()
            .map(|r| (r.id, r.doc))
            .collect();
        let custom: Vec<Custom> = stored
            .iter()
            .map(|(id, scheme)| Custom {
                id: id.as_str().to_string(),
                scheme: scheme.clone(),
            })
            .collect();
        let terminal = &settings.terminal;
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(t!("schemes.search")));
        let entries = all_entries(&custom);
        let light_select = choice_state(
            slot_choices(&entries, &terminal.scheme_light),
            &terminal.scheme_light,
            true,
            window,
            cx,
        );
        let dark_select = choice_state(
            slot_choices(&entries, &terminal.scheme_dark),
            &terminal.scheme_dark,
            true,
            window,
            cx,
        );
        let subscriptions = vec![
            cx.subscribe(&query, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe(
                &light_select,
                |this, _, event: &SelectEvent<SearchableVec<Choice<String>>>, cx| {
                    if let SelectEvent::Confirm(Some(id)) = event {
                        this.light = id.clone();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(
                &dark_select,
                |this, _, event: &SelectEvent<SearchableVec<Choice<String>>>, cx| {
                    if let SelectEvent::Confirm(Some(id)) = event {
                        this.dark = id.clone();
                        cx.notify();
                    }
                },
            ),
        ];
        Self {
            light: terminal.scheme_light.clone(),
            dark: terminal.scheme_dark.clone(),
            min_contrast: terminal.min_contrast > 1.,
            initial_min_contrast: terminal.min_contrast,
            custom,
            stored,
            next_new: 0,
            query,
            filter: Filter::All,
            light_select,
            dark_select,
            highlights: SampleHighlights::new(&settings.highlight),
            font_family: sample_font(settings),
            _subscriptions: subscriptions,
        }
    }

    /// 写进终端设置：两个槽与“保证文字可读”。槽可能是新建配色的临时 ID，[`Self::commit`] 之后再写一次。
    pub(super) fn apply(&self, terminal: &mut TerminalSettings) {
        terminal.scheme_light = self.light.clone();
        terminal.scheme_dark = self.dark.clone();
        terminal.min_contrast = match (self.min_contrast, self.initial_min_contrast > 1.) {
            (true, true) => self.initial_min_contrast,
            (true, false) => MIN_CONTRAST_ON as f32,
            (false, _) => 0.,
        };
    }

    /// 把自定义配色的增、改、删写进存储。删除的先清掉主机、分组对它的引用；新建的换成存储分配的 ID
    /// （槽中的也换）。中途出错时已写入的保留，草稿随之更新，再次保存不会重复写入。
    pub(super) fn commit(
        &mut self,
        store: &Store,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), CoreError> {
        let deleted: Vec<SchemeId> = self
            .stored
            .iter()
            .filter(|(id, _)| !self.custom.iter().any(|c| c.id == id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &deleted {
            for host in store.list::<Host>() {
                if host.doc.terminal.color_scheme == id.as_str() {
                    let mut doc = host.doc;
                    doc.terminal.color_scheme.clear();
                    store.update(&host.id, doc)?;
                }
            }
            for group in store.list::<Group>() {
                if group.doc.defaults.color_scheme == id.as_str() {
                    let mut doc = group.doc;
                    doc.defaults.color_scheme.clear();
                    store.update(&group.id, doc)?;
                }
            }
            match store.delete::<ColorScheme>(id) {
                // 别处（云同步）已经删掉了
                Ok(()) | Err(CoreError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
            self.stored.retain(|(s, _)| s != id);
        }
        let mut renamed = false;
        for ix in 0..self.custom.len() {
            let custom = &self.custom[ix];
            let stored = self
                .stored
                .iter_mut()
                .find(|(id, _)| id.as_str() == custom.id);
            let exists = match stored {
                Some((_, scheme)) if *scheme == custom.scheme => true,
                Some((id, scheme)) => match store.update(id, custom.scheme.clone()) {
                    Ok(()) => {
                        *scheme = custom.scheme.clone();
                        true
                    }
                    // 编辑期间别处（云同步）删掉了：照新建处理
                    Err(CoreError::NotFound(_)) => false,
                    Err(e) => return Err(e),
                },
                None => false,
            };
            // 新建的（或存储中已经没有了）：建一套
            if !exists {
                let id = store.create(self.custom[ix].scheme.clone(), None, Position::Last)?;
                let old = std::mem::replace(&mut self.custom[ix].id, id.as_str().to_string());
                self.stored.retain(|(s, _)| s.as_str() != old);
                for slot in [&mut self.light, &mut self.dark] {
                    if *slot == old {
                        *slot = id.as_str().to_string();
                    }
                }
                self.stored.push((id, self.custom[ix].scheme.clone()));
                renamed = true;
            }
        }
        if renamed || !deleted.is_empty() {
            self.refresh_selects(window, cx);
        }
        Ok(())
    }

    /// 界面为浅色 / 深色时用的配色（“终端”分页显示当前的）；槽中的 ID 不认识时是默认配色。
    pub(super) fn slot_entry(&self, dark: bool) -> SchemeEntry {
        let entries = all_entries(&self.custom);
        let id = self.slot(dark);
        let fallback = if dark {
            DEFAULT_SCHEME_DARK
        } else {
            DEFAULT_SCHEME_LIGHT
        };
        entries
            .iter()
            .find(|e| e.id == id)
            .or_else(|| entries.iter().find(|e| e.id == fallback))
            .cloned()
            .unwrap_or_else(|| entries[0].clone())
    }

    fn slot(&self, dark: bool) -> &str {
        if dark { &self.dark } else { &self.light }
    }

    fn entries(&self) -> Vec<SchemeEntry> {
        all_entries(&self.custom)
    }

    fn is_full(&self) -> bool {
        self.custom.len() >= MAX_CUSTOM_SCHEMES
    }

    fn set_slot(&mut self, dark: bool, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let select = if dark {
            self.dark = id.clone();
            self.dark_select.clone()
        } else {
            self.light = id.clone();
            self.light_select.clone()
        };
        select.update(cx, |state, cx| state.set_selected_value(&id, window, cx));
        cx.notify();
    }

    /// 自定义配色变了：两个槽的下拉框重新列出选项。
    fn refresh_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.entries();
        for (select, value) in [
            (self.light_select.clone(), self.light.clone()),
            (self.dark_select.clone(), self.dark.clone()),
        ] {
            let choices = slot_choices(&entries, &value);
            select.update(cx, |state, cx| {
                state.set_items(SearchableVec::new(choices), window, cx);
                state.set_selected_value(&value, window, cx);
            });
        }
        cx.notify();
    }

    /// 全部配色的名称（重名检查用），`except` 的那套除外。
    fn names_except(&self, except: Option<&str>) -> Vec<String> {
        self.entries()
            .into_iter()
            .filter(|e| Some(e.id.as_str()) != except)
            .map(|e| e.scheme.name)
            .collect()
    }

    fn open_editor(
        &mut self,
        scheme: ColorScheme,
        existing: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = cx.entity().downgrade();
        let target = existing.clone();
        editor::open(
            editor::EditorOptions {
                scheme,
                is_new: existing.is_none(),
                taken: self.names_except(existing.as_deref()),
                highlights: self.highlights.clone(),
                font_family: self.font_family.clone(),
                on_save: Rc::new(move |scheme, window, cx| {
                    let target = target.clone();
                    let _ = page.update(cx, |page, cx| page.on_edited(target, scheme, window, cx));
                }),
            },
            window,
            cx,
        );
    }

    /// 编辑器按了“确定”：修改原来的那套，或加入新的一套。
    fn on_edited(
        &mut self,
        target: Option<String>,
        scheme: ColorScheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target.and_then(|id| self.custom.iter_mut().find(|c| c.id == id)) {
            Some(custom) => custom.scheme = scheme,
            None => self.push_new(scheme),
        }
        self.refresh_selects(window, cx);
    }

    fn push_new(&mut self, scheme: ColorScheme) {
        self.next_new += 1;
        self.custom.push(Custom {
            id: format!("{NEW_PREFIX}{}", self.next_new),
            scheme,
        });
    }

    /// 复制为自定义配色：在编辑器中打开副本（名称已去重），确定后才加入。
    fn copy(&mut self, entry: &SchemeEntry, window: &mut Window, cx: &mut Context<Self>) {
        let names = self.names_except(None);
        if let Some(copy) = to_custom_scheme(&entry.scheme, names.iter().map(String::as_str)) {
            self.open_editor(copy, None, window, cx);
        }
    }

    fn edit(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(custom) = self.custom.iter().find(|c| c.id == id) {
            let scheme = custom.scheme.clone();
            self.open_editor(scheme, Some(id.to_string()), window, cx);
        }
    }

    /// 确认后删除自定义配色（草稿中删除，保存设置时才写入）；在用它的主机、分组数一并说明。
    fn confirm_delete(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(custom) = self.custom.iter().find(|c| c.id == id) else {
            return;
        };
        let name = custom.scheme.name.clone();
        let (hosts, groups) = usage(&AppState::store(cx), id);
        let page = cx.entity().downgrade();
        let id = id.to_string();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let page = page.clone();
            let id = id.clone();
            let alert = alert
                .confirm()
                .title(t!("schemes.delete_title", name = name.clone()))
                .ok_text(t!("common.delete"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = page.update(cx, |page, cx| page.remove(&id, window, cx));
                    true
                });
            if hosts + groups > 0 {
                alert.description(prose(t!(
                    "schemes.delete_in_use",
                    hosts = hosts,
                    groups = groups
                )))
            } else {
                alert.description(prose(t!("schemes.delete_unsaved")))
            }
        });
    }

    fn remove(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.custom.retain(|c| c.id != id);
        // 槽中用的就是它：改回默认配色
        if self.light == id {
            self.light = DEFAULT_SCHEME_LIGHT.to_string();
        }
        if self.dark == id {
            self.dark = DEFAULT_SCHEME_DARK.to_string();
        }
        self.refresh_selects(window, cx);
    }

    /// 打开导入对话框，停在 `source` 页签。
    fn open_import(&mut self, source: import::Source, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity().downgrade();
        import::open(
            import::ImportOptions {
                taken: self.names_except(None),
                remaining: MAX_CUSTOM_SCHEMES.saturating_sub(self.custom.len()),
                highlights: self.highlights.clone(),
                font_family: self.font_family.clone(),
                source,
                on_import: Rc::new(move |schemes, window, cx| {
                    let _ = page.update(cx, |page, cx| page.on_imported(schemes, window, cx));
                }),
            },
            window,
            cx,
        );
    }

    fn on_imported(
        &mut self,
        schemes: Vec<ColorScheme>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = schemes.len();
        if count > 0 {
            termlet_core::usage::mark(termlet_core::usage::Feature::SchemeImport);
        }
        for scheme in schemes {
            self.push_new(scheme);
        }
        self.refresh_selects(window, cx);
        crate::notify::show(
            crate::notify::Notice::success(t!("schemes.imported", count = count)),
            window,
            cx,
        );
    }

    fn open_export(&self, initial: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        export::open(self.entries(), initial, window, cx);
    }

    /// 搜索与筛选后列出的配色：每个词都要出现在名称或出处中（不区分大小写）。
    fn visible(&self, cx: &App) -> Vec<SchemeEntry> {
        let query = self.query.read(cx).value().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        self.entries()
            .into_iter()
            .filter(|e| match self.filter {
                Filter::All => true,
                Filter::Light => !e.is_dark(),
                Filter::Dark => e.is_dark(),
            })
            .filter(|e| {
                let text = format!("{} {}", e.scheme.name, e.project()).to_lowercase();
                words.iter().all(|w| text.contains(w))
            })
            .collect()
    }

    // ------------------------------------------------------------ 渲染

    fn render_card(&self, entry: SchemeEntry, dark_ui: bool, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let id = entry.id.clone();
        let in_light = self.light == id;
        let in_dark = self.dark == id;
        let current = if dark_ui { in_dark } else { in_light };
        let custom = !entry.is_builtin();
        let full = self.is_full();
        // 看不清的颜色：内置配色建议复制后调整，自定义配色建议编辑
        let unreadable: Option<SharedString> = unreadable_names(&entry.scheme).map(|colors| {
            if custom {
                t!("schemes.unreadable_custom", colors = colors)
            } else {
                t!("schemes.unreadable_builtin", colors = colors)
            }
        });
        let page = cx.entity().downgrade();
        let menu_entry = entry.clone();
        let menu = Button::new(SharedString::from(format!("scheme-menu-{id}")))
            .ghost()
            .small()
            .icon(IconName::Ellipsis)
            .tooltip(t!("schemes.actions"))
            .dropdown_menu(move |menu, _, _| {
                let action = |f: fn(
                    &mut SchemesPage,
                    &SchemeEntry,
                    &mut Window,
                    &mut Context<SchemesPage>,
                )| {
                    let page = page.clone();
                    let entry = menu_entry.clone();
                    move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = page.update(cx, |page, cx| f(page, &entry, window, cx));
                    }
                };
                menu.item(
                    PopupMenuItem::new(t!("schemes.use_light"))
                        .on_click(action(|p, e, w, cx| p.set_slot(false, e.id.clone(), w, cx))),
                )
                .item(
                    PopupMenuItem::new(t!("schemes.use_dark"))
                        .on_click(action(|p, e, w, cx| p.set_slot(true, e.id.clone(), w, cx))),
                )
                .separator()
                .item(
                    PopupMenuItem::new(t!("schemes.copy"))
                        .disabled(full)
                        .on_click(action(|p, e, w, cx| p.copy(e, w, cx))),
                )
                .when(custom, |menu| {
                    menu.item(
                        PopupMenuItem::new(t!("schemes.edit"))
                            .on_click(action(|p, e, w, cx| p.edit(&e.id, w, cx))),
                    )
                })
                .item(
                    PopupMenuItem::new(t!("schemes.export_one")).on_click(action(|p, e, w, cx| {
                        p.open_export(vec![e.id.clone()], w, cx)
                    })),
                )
                .when(custom, |menu| {
                    menu.separator().item(
                        PopupMenuItem::new(t!("common.delete"))
                            .on_click(action(|p, e, w, cx| p.confirm_delete(&e.id, w, cx))),
                    )
                })
            });
        let footer: AnyElement = match entry.kind {
            SchemeKind::Builtin(Some(source)) => {
                let url = source.url;
                let tip: SharedString = format!("{}\n{}", source.copyright, source.url).into();
                // 链接只占文字的宽度，与其他卡片的说明左对齐
                h_flex()
                    .child(
                        Button::new(SharedString::from(format!("scheme-source-{id}")))
                            .link()
                            .xsmall()
                            .label(t!(
                                "schemes.source.project",
                                project = source.project,
                                license = source.license.spdx()
                            ))
                            .tooltip(tip)
                            .on_click(move |_, _, cx| cx.open_url(url)),
                    )
                    .into_any_element()
            }
            SchemeKind::Builtin(None) => {
                div().child(t!("schemes.source.termlet")).into_any_element()
            }
            SchemeKind::Custom => div().child(t!("schemes.source.custom")).into_any_element(),
        };
        let use_id = id.clone();
        v_flex()
            .id(SharedString::from(format!("scheme-card-{id}")))
            .test_support()
            .aria_selected(current)
            .overflow_hidden()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(if current { theme.primary } else { theme.border })
            .bg(theme.background)
            .child(
                h_flex()
                    .h_8()
                    .pl_2p5()
                    .pr_1()
                    .gap_1p5()
                    .text_xs()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_medium()
                            .child(entry.scheme.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(muted)
                            .child(mode_label(entry.is_dark())),
                    )
                    .when_some(unreadable, |this, tip| {
                        this.child(
                            div()
                                .id(SharedString::from(format!("scheme-unreadable-{id}")))
                                .flex_none()
                                .text_color(theme.warning)
                                .child(Icon::new(IconName::TriangleAlert).xsmall())
                                .tooltip(move |window, cx| {
                                    Tooltip::new(tip.clone()).build(window, cx)
                                }),
                        )
                    })
                    .child(div().flex_1())
                    .when(in_light, |this| {
                        this.child(
                            Tag::secondary()
                                .small()
                                .flex_none()
                                .child(t!("schemes.used_light")),
                        )
                    })
                    .when(in_dark, |this| {
                        this.child(
                            Tag::secondary()
                                .small()
                                .flex_none()
                                .child(t!("schemes.used_dark")),
                        )
                    })
                    .child(menu),
            )
            .child(
                // 单击示例：用于界面当前深浅对应的槽
                Button::new(SharedString::from(format!("scheme-use-{id}")))
                    .ghost()
                    .rounded(ButtonRounded::None)
                    .w_full()
                    .h_auto()
                    .p_0()
                    .accessibility_label(entry.scheme.name.clone())
                    .tooltip(t!("schemes.use_current"))
                    .child(SchemeSample::new(
                        &entry.scheme,
                        &self.highlights,
                        &self.font_family,
                    ))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let dark = cx.theme().is_dark();
                        this.set_slot(dark, use_id.clone(), window, cx);
                    })),
            )
            .child(
                div()
                    .px_2p5()
                    .py_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(footer),
            )
            .into_any_element()
    }
}

impl Render for SchemesPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let dark_ui = theme.is_dark();
        let full = self.is_full();
        let filter_ix = Filter::ALL
            .iter()
            .position(|f| *f == self.filter)
            .unwrap_or(0);
        let visible = self.visible(cx);
        let cards = visible
            .into_iter()
            .map(|entry| self.render_card(entry, dark_ui, cx))
            .collect::<Vec<_>>();
        let custom_ids: Vec<String> = self.custom.iter().map(|c| c.id.clone()).collect();
        v_flex()
            .id("settings-schemes")
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_start()
                            .child(div().flex_1().child(
                                field().label(t!("schemes.slot_light")).child(
                                    Select::new(&self.light_select).id("settings-scheme-light"),
                                ),
                            ))
                            .child(div().flex_1().child(
                                field().label(t!("schemes.slot_dark")).child(
                                    Select::new(&self.dark_select).id("settings-scheme-dark"),
                                ),
                            )),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!("schemes.slots_hint"))),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Checkbox::new("settings-min-contrast")
                            .label(t!("schemes.min_contrast"))
                            .checked(self.min_contrast)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                this.min_contrast = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .pl_6()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!("schemes.min_contrast_hint"))),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(rems(12.))
                            // 搜索框在设置对话框中：Enter 不能触发对话框的“确定”（会关掉设置、丢掉修改）
                            .on_action(|_: &gpui_kit::base::actions::Confirm, _, _| {})
                            .child(
                                Input::new(&self.query)
                                    .small()
                                    .prefix(Icon::new(IconName::Search).xsmall().text_color(muted))
                                    .id("scheme-search"),
                            ),
                    )
                    .child(
                        TabBar::new("scheme-filter")
                            .segmented()
                            .small()
                            .selected_index(filter_ix)
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                if let Some(filter) = Filter::ALL.get(*ix) {
                                    this.filter = *filter;
                                    cx.notify();
                                }
                            }))
                            .child(Tab::new().label(t!("schemes.filter.all")))
                            .child(Tab::new().label(t!("schemes.filter.light")))
                            .child(Tab::new().label(t!("schemes.filter.dark"))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("scheme-import")
                            .outline()
                            .small()
                            .icon(IconName::Download)
                            .label(t!("schemes.import"))
                            .disabled(full)
                            .when(full, |this| {
                                this.tooltip(t!("schemes.full", count = MAX_CUSTOM_SCHEMES))
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_import(import::Source::File, window, cx)
                            })),
                    )
                    .child(
                        Button::new("scheme-export")
                            .outline()
                            .small()
                            .icon(IconName::Upload)
                            .label(t!("schemes.export"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_export(custom_ids.clone(), window, cx)
                            })),
                    ),
            )
            .child(if cards.is_empty() {
                // 搜索不到：给出导入其他终端的配色、读取本机已装客户端的配色这两条路
                v_flex()
                    .gap_2()
                    .items_start()
                    .text_sm()
                    .text_color(muted)
                    .child(t!("schemes.no_match"))
                    .child(div().text_xs().child(prose(t!("schemes.no_match_hint"))))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("scheme-no-match-import")
                                    .outline()
                                    .small()
                                    .icon(IconName::Download)
                                    .label(t!("schemes.import"))
                                    .disabled(full)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_import(import::Source::File, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("scheme-no-match-installed")
                                    .outline()
                                    .small()
                                    .label(t!("schemes.import.read_installed"))
                                    .disabled(full)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_import(import::Source::Installed, window, cx)
                                    })),
                            ),
                    )
                    .into_any_element()
            } else {
                div()
                    .grid()
                    .grid_cols(2)
                    .gap_3()
                    .children(cards)
                    .into_any_element()
            })
    }
}

/// 全部配色：内置的在前，自定义的（草稿）在后。
fn all_entries(custom: &[Custom]) -> Vec<SchemeEntry> {
    let mut entries = builtin_entries();
    entries.extend(custom.iter().map(|c| SchemeEntry {
        id: c.id.clone(),
        scheme: c.scheme.clone(),
        kind: SchemeKind::Custom,
    }));
    entries
}

/// 槽的下拉框的选项：内置的浅色、深色，然后是自定义的。槽中的 ID 不认识（新版本的内置配色、已删除的自定义
/// 配色）时仍列出，说明会改用默认配色。
fn slot_choices(entries: &[SchemeEntry], current: &str) -> Vec<Choice<String>> {
    let light = entries.iter().filter(|e| e.is_builtin() && !e.is_dark());
    let dark = entries.iter().filter(|e| e.is_builtin() && e.is_dark());
    let custom = entries.iter().filter(|e| !e.is_builtin());
    let known = find_scheme(current, |id| {
        entries.iter().find(|e| e.id == id).map(|e| &e.scheme)
    })
    .is_some();
    (!known)
        .then(|| Choice::new(t!("schemes.unknown", id = current), current.to_string()))
        .into_iter()
        .chain(
            light
                .chain(dark)
                .chain(custom)
                .map(|e| Choice::new(e.scheme.name.clone(), e.id.clone())),
        )
        .collect()
}

/// 使用这套配色的主机数、分组数（新建、还没有保存的配色没有）。
fn usage(store: &Store, id: &str) -> (usize, usize) {
    let hosts = store
        .list::<Host>()
        .iter()
        .filter(|h| h.doc.terminal.color_scheme == id)
        .count();
    let groups = store
        .list::<Group>()
        .iter()
        .filter(|g| g.doc.defaults.color_scheme == id)
        .count();
    (hosts, groups)
}

/// “终端”分页中的配色字段：当前界面深浅所用配色的色块与名称，单击跳到“终端配色”分页。
pub(super) fn slot_button(entry: &SchemeEntry) -> Button {
    Button::new("settings-open-schemes")
        .outline()
        .w_full()
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .child(SchemeSwatch::new(&entry.scheme))
                .child(div().min_w_0().truncate().child(entry.scheme.name.clone())),
        )
}
