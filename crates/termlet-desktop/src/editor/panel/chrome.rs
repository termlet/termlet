//! 编辑器 Tab 的外框：Tab 标题、工具栏、提示条、状态栏。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex};
use gpui_kit::{
    AnyElement, App, Context, Div, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _,
    rems,
};
use termlet_core::sftp::TextEncoding;

use super::{Banner, CloseTab, EditorPanel, Find, KEY_CONTEXT, Link, Save};
use crate::editor::text::Eol;
use crate::prose::prose;
use crate::sftp::list_model::{format_size, visible_name};
use crate::t;
use crate::truncated::Clip;

/// 提示条：浅色底、同色文字；文字太长时换行。
fn bar(background: Hsla, foreground: Hsla) -> Div {
    h_flex()
        .flex_none()
        .flex_wrap()
        .px_3()
        .py_0p5()
        .gap_x_3()
        .gap_y_1()
        .min_h(rems(1.5))
        .text_xs()
        .bg(background)
        .text_color(foreground)
}

impl EditorPanel {
    /// Tab 上的名称：文件名，有未保存的修改时前面加“● ”。
    fn tab_label(&self) -> SharedString {
        if self.dirty {
            format!("● {}", self.file_name).into()
        } else {
            self.file_name.clone()
        }
    }

    /// 连接断开、重连中时 Tab 上的状态圆点的颜色。
    fn tab_status(&self, cx: &App) -> Option<Hsla> {
        match self.link {
            Link::Ready => None,
            Link::Reconnecting | Link::Waiting => Some(cx.theme().warning),
            Link::Lost => Some(cx.theme().danger),
        }
    }

    /// 竖排的 Tab 列表中这个 Tab 的一行：与横排的 Tab 上相同的图标、名称与状态，悬停显示完整的远程路径；按主机分组时
    /// 与这台主机的终端、文件管理归在一起。
    pub(crate) fn tab_face(&self, cx: &App) -> crate::workspace::TabFace {
        let host = self.host_key();
        let host = host.strip_suffix("#root").unwrap_or(&host).to_string();
        let theme = cx.theme();
        let status = self.tab_status(cx);
        crate::workspace::TabFace {
            icon: Icon::new(IconName::FileText)
                .xsmall()
                .text_color(theme.warning)
                .into_any_element(),
            rail_icon: Icon::new(IconName::FileText)
                .with_size(crate::workspace::RAIL_ICON)
                .text_color(theme.warning)
                .into_any_element(),
            name: self.tab_label(),
            detail: Some(visible_name(&self.path).into()),
            target: None,
            badges: status
                .map(|color| div().size_1p5().rounded_full().bg(color).into_any_element())
                .into_iter()
                .collect(),
            status: match self.link {
                Link::Ready => None,
                Link::Reconnecting | Link::Waiting => Some(crate::workspace::TabStatus::dot(
                    theme.warning,
                    t!("tab_list.status.connecting"),
                )),
                Link::Lost => Some(crate::workspace::TabStatus::dot(
                    theme.danger,
                    t!("tab_list.status.disconnected"),
                )),
            }
            .into_iter()
            .collect(),
            host: Some((host.into(), self.host_name.clone())),
        }
    }

    /// Tab 标题：文件名（有未保存的修改时前面加“● ”），悬停显示完整的远程路径；连接断开时带状态圆点。
    /// `menu_open`：Tab 的右键菜单等开着，关闭按钮不挂悬停提示（会浮在菜单上，见 [`crate::menu_tooltip`]）。
    pub(super) fn render_title(&self, menu_open: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let warning = cx.theme().warning;
        let id = cx.entity_id().as_u64() as usize;
        let label = self.tab_label();
        let status = self.tab_status(cx);
        let path: SharedString = visible_name(&self.path).into();
        h_flex()
            .gap_1()
            .child(
                h_flex()
                    .id(("editor-tab-title", id))
                    .gap_1()
                    .child(Icon::new(IconName::FileText).xsmall().text_color(warning))
                    // 文件名太长时截断；提示是完整的路径，不用另补
                    .child(crate::dock_skin::tab_name(&Clip::default(), label))
                    .tooltip(crate::menu_tooltip::quiet(move |window, cx| {
                        Tooltip::new(path.clone()).build(window, cx)
                    })),
            )
            .when_some(status, |this, color| {
                this.child(div().size_1p5().rounded_full().bg(color))
            })
            .child(
                // 自己的关闭按钮（Dock 自带的不显示）：有未保存的修改时先确认
                Button::new(("editor-tab-close", id))
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tab_stop(false)
                    .when(!menu_open, |this| {
                        this.tooltip_with_action(t!("panel.close"), &CloseTab, Some(KEY_CONTEXT))
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.request_close(window, cx);
                    })),
            )
    }

    /// 工具栏：保存、重新加载、查找与替换、远程路径（等宽、放不下时截断，悬停显示全部）。
    pub(super) fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (mono, muted, border) = (
            theme.mono_font_family.clone(),
            theme.muted_foreground,
            theme.border,
        );
        let path: SharedString = visible_name(&self.path).into();
        let tip = path.clone();
        let reload_enabled = self.is_ready() && !self.saving;
        h_flex()
            .flex_none()
            .gap_0p5()
            .px_1()
            .py_1()
            .border_b_1()
            .border_color(border)
            .child(
                Button::new("editor-save")
                    .ghost()
                    .small()
                    .icon(IconName::Save)
                    .tooltip_with_action(t!("common.save"), &Save, Some(KEY_CONTEXT))
                    .loading(self.saving)
                    .disabled(!self.can_save())
                    .on_click(cx.listener(|this, _, window, cx| this.save(false, window, cx))),
            )
            .child(
                Button::new("editor-reload")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip(t!("editor.reload"))
                    .loading(self.loading)
                    .disabled(!reload_enabled)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_reload(window, cx))),
            )
            .child(
                Button::new("editor-find")
                    .ghost()
                    .small()
                    .icon(IconName::Search)
                    .tooltip_with_action(t!("editor.find"), &Find, Some(KEY_CONTEXT))
                    .on_click(cx.listener(|this, _, window, cx| this.open_find(window, cx))),
            )
            .child(
                div()
                    .id("editor-path")
                    .flex_1()
                    .min_w_0()
                    .mx_2()
                    .truncate()
                    .font_family(mono)
                    .text_xs()
                    .text_color(muted)
                    .child(path)
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)),
            )
            .into_any_element()
    }

    /// 连接状态的提示条：正在自动重连、已断开（重新连接；SFTP 面板也关了时提示重新打开）、正在等新的会话。
    pub(super) fn render_link_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (danger, warning) = (theme.danger, theme.warning);
        let reconnect = || {
            Button::new("editor-reconnect")
                .ghost()
                .xsmall()
                .label(t!("sftp.reconnect"))
                .on_click(cx.listener(|this, _, window, cx| this.reconnect(window, cx)))
        };
        let message = |text: SharedString| div().flex_1().min_w_0().child(prose(text));
        let element = match self.link {
            Link::Ready => return None,
            Link::Reconnecting => bar(warning.opacity(0.15), warning)
                .child(message(t!("editor.reconnecting")))
                .into_any_element(),
            // SFTP 面板那边连不上时可以再试
            Link::Waiting => bar(warning.opacity(0.15), warning)
                .child(message(t!("editor.waiting")))
                .child(reconnect())
                .into_any_element(),
            Link::Lost if !self.has_sftp_panel() => bar(danger.opacity(0.15), danger)
                .child(message(t!(
                    "editor.lost_sftp_closed",
                    host = self.host_name.clone()
                )))
                .into_any_element(),
            Link::Lost => bar(danger.opacity(0.15), danger)
                .child(message(t!("editor.lost")))
                .child(reconnect())
                .into_any_element(),
        };
        Some(element)
    }

    /// 冲突、读取失败与其他错误的提示条。
    pub(super) fn render_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (danger, warning) = (theme.danger, theme.warning);
        let message = |text: SharedString| div().flex_1().min_w_0().child(prose(text));
        let dismiss = || {
            Button::new("editor-banner-close")
                .ghost()
                .small()
                .icon(IconName::Close)
                .tooltip(t!("common.close"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.banner = None;
                    cx.notify();
                }))
        };
        let ready = self.is_ready() && !self.saving;
        let element = match self.banner.as_ref()? {
            Banner::Conflict { deleted } => {
                let deleted = *deleted;
                let encoding = self.encoding;
                bar(warning.opacity(0.15), warning)
                    .child(message(if deleted {
                        t!("editor.conflict_deleted")
                    } else {
                        t!("editor.conflict")
                    }))
                    .child(
                        Button::new("editor-overwrite")
                            .outline()
                            .xsmall()
                            .label(if deleted {
                                t!("editor.save_anyway")
                            } else {
                                t!("editor.overwrite")
                            })
                            .disabled(!ready)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save(true, window, cx)),
                            ),
                    )
                    // 已被删除时没有可以重新加载的
                    .when(!deleted, |this| {
                        this.child(
                            Button::new("editor-discard")
                                .ghost()
                                .xsmall()
                                .label(t!("editor.discard"))
                                .disabled(!ready)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.reload(encoding, window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("editor-keep-editing")
                            .ghost()
                            .xsmall()
                            .label(t!("editor.keep_editing"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.banner = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }
            Banner::LoadFailed {
                message: error,
                encoding,
            } => {
                let encoding = *encoding;
                bar(danger.opacity(0.15), danger)
                    .child(message(t!("editor.load_failed", error = error.clone())))
                    .child(
                        Button::new("editor-retry")
                            .ghost()
                            .xsmall()
                            .label(t!("editor.retry"))
                            .disabled(!ready)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.reload(encoding, window, cx)
                            })),
                    )
                    .child(dismiss())
                    .into_any_element()
            }
            Banner::Error(error) => bar(danger.opacity(0.15), danger)
                .child(message(error.clone()))
                .child(dismiss())
                .into_any_element(),
        };
        Some(element)
    }

    /// 状态栏：保存状态；语言、换行符、编码（可换一种重新打开）、权限、大小。
    pub(super) fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (mono, muted, border) = (
            theme.mono_font_family.clone(),
            theme.muted_foreground,
            theme.border,
        );
        let state = if self.saving {
            Some(t!("editor.saving"))
        } else if self.loading {
            Some(t!("editor.reloading"))
        } else if self.dirty {
            Some(t!("editor.unsaved"))
        } else if self.saved_once {
            Some(t!("editor.saved"))
        } else {
            None
        };
        let language: SharedString = self
            .language
            .map_or_else(|| t!("editor.plain_text"), |l| l.label().into());
        let eol: SharedString = match self.eol {
            Eol::Lf => "LF".into(),
            Eol::Crlf => "CRLF".into(),
            Eol::Mixed => t!("editor.eol_mixed"),
        };
        let current = self.encoding;
        let this = cx.entity().downgrade();
        let encoding = Button::new("editor-encoding")
            .ghost()
            .xsmall()
            .label(if self.bom {
                SharedString::from(format!("{} BOM", current.label()))
            } else {
                current.label().into()
            })
            .tooltip(t!("editor.encoding"))
            .disabled(!self.is_ready() || self.saving)
            .dropdown_menu(move |menu, _, _| {
                TextEncoding::ALL.into_iter().fold(menu, |menu, value| {
                    let this = this.clone();
                    menu.item(
                        PopupMenuItem::new(value.label())
                            .checked(value == current)
                            .on_click(move |_, window, cx| {
                                let _ =
                                    this.update(cx, |this, cx| this.reopen_with(value, window, cx));
                            }),
                    )
                })
            });
        h_flex()
            .flex_none()
            .px_2()
            .gap_3()
            .text_xs()
            .text_color(muted)
            .border_t_1()
            .border_color(border)
            .child(div().flex_1().min_w_0().truncate().children(state))
            .child(language)
            .child(eol)
            .child(encoding)
            .child(div().font_family(mono).child(self.meta.mode.clone()))
            .child(format_size(self.meta.size))
            .into_any_element()
    }
}
