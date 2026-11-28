//! 分组：新建（只填名称）、编辑（名称与默认值）、删除。
//!
//! 分组可以设默认的用户名、凭据档案、跳板机、代理命令与终端配色，分组及其子分组中没有设置这些项的主机逐级向上沿用。
//! 改上级分组靠在侧边栏中拖动。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{AlertDialog, DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::id::{GroupId, HostId, ProfileId};
use termlet_core::model::{ColorScheme, Group, Host, Profile};
use termlet_core::schemes;
use termlet_core::store::{Position, Store};

use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::prose::prose;
use crate::{store_errors, t};

enum Mode {
    New { parent: Option<GroupId> },
    Edit { id: GroupId },
}

struct GroupForm {
    mode: Mode,
    store: std::sync::Arc<Store>,
    name: Entity<InputState>,
    username: Entity<InputState>,
    profile: Entity<ChoiceState<Option<ProfileId>>>,
    jump: Entity<ChoiceState<Option<HostId>>>,
    proxy_command: Entity<InputState>,
    scheme: Entity<ChoiceState<String>>,
    error: Option<SharedString>,
}

/// 新建分组（`parent` 下的子分组，`None` 为顶层）。
pub fn open_new(parent: Option<GroupId>, window: &mut Window, cx: &mut App) {
    open(Mode::New { parent }, window, cx);
}

/// 编辑分组的名称与默认值。
pub fn open_edit(id: GroupId, window: &mut Window, cx: &mut App) {
    open(Mode::Edit { id }, window, cx);
}

fn open(mode: Mode, window: &mut Window, cx: &mut App) {
    let editing = matches!(mode, Mode::Edit { .. });
    let form = cx.new(|cx| GroupForm::new(mode, window, cx));
    let first_focus = Rc::new(Cell::new(true));
    let width = rems(if editing { 32. } else { 24. }).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false) {
            form.read(cx).name.focus_handle(cx).focus(window, cx);
        }
        let submit = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.submit(window, cx))
            }
        };
        dialog
            .title(if editing {
                t!("group.edit.title")
            } else {
                t!("group.new.title")
            })
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
            .on_ok(submit)
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("group-cancel").label(t!("common.cancel"))),
                    )
                    .child(
                        DialogAction::new().child(Button::new("group-save").primary().label(
                            if editing {
                                t!("common.save")
                            } else {
                                t!("group.new.create")
                            },
                        )),
                    ),
            )
    });
}

impl GroupForm {
    fn new(mode: Mode, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = AppState::store(cx);
        let group = match &mode {
            Mode::Edit { id } => store.get::<Group>(id).map(|r| r.doc).unwrap_or_default(),
            Mode::New { .. } => Group::default(),
        };
        let defaults = &group.defaults;
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("group.field.name_placeholder"))
                .default_value(group.name.clone())
        });
        let username = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("group.field.not_set"))
                .default_value(defaults.username.clone())
        });
        let profile = choice_state(profile_choices(&store), &defaults.profile, true, window, cx);
        let jump = choice_state(jump_choices(&store, None), &defaults.jump, true, window, cx);
        let proxy_command = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("group.field.not_set"))
                .default_value(defaults.proxy_command.clone())
        });
        let scheme = choice_state(
            scheme_choices(&store, t!("group.field.scheme_global")),
            &defaults.color_scheme,
            true,
            window,
            cx,
        );
        Self {
            mode,
            store,
            name,
            username,
            profile,
            jump,
            proxy_command,
            scheme,
            error: None,
        }
    }

    /// 保存；成功时返回 true（关闭对话框）。
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let name = self.name.read(cx).value().trim().to_string();
        let logged = name.clone();
        let creating = matches!(self.mode, Mode::New { .. });
        let result = match &self.mode {
            Mode::New { parent } => {
                let group = Group {
                    name,
                    ..Default::default()
                };
                self.store
                    .create(
                        group,
                        parent.as_ref().map(|p| p.as_str().to_string()),
                        Position::Last,
                    )
                    .map(|id| {
                        // 新分组折叠着；它所在的上级分组折叠时展开，看得到建在了哪里
                        crate::workspace::Workspace::reveal_in_sidebar(
                            crate::sidebar::rows::RowKey::Group(id),
                            window,
                            cx,
                        )
                    })
            }
            Mode::Edit { id } => match self.store.get::<Group>(id) {
                Some(record) => {
                    let mut group = record.doc;
                    group.name = name;
                    group.defaults.username = self.username.read(cx).value().to_string();
                    group.defaults.profile = chosen(&self.profile, cx).flatten();
                    group.defaults.jump = chosen(&self.jump, cx).flatten();
                    group.defaults.proxy_command = self.proxy_command.read(cx).value().to_string();
                    group.defaults.color_scheme = chosen(&self.scheme, cx).unwrap_or_default();
                    self.store.update(id, group)
                }
                None => Err(termlet_core::CoreError::NotFound("分组")),
            },
        };
        match result {
            Ok(()) => {
                tracing::info!(name = %logged, creating, "已保存分组");
                true
            }
            Err(e) => {
                tracing::warn!(name = %logged, creating, "保存分组失败：{e}");
                self.error = Some(store_errors::describe(&e));
                cx.notify();
                false
            }
        }
    }
}

impl Render for GroupForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = matches!(self.mode, Mode::Edit { .. });
        let form = v_form().child(
            field()
                .label(t!("group.field.name"))
                .child(Input::new(&self.name).id("group-name")),
        );
        let form = if editing {
            form.child(
                field()
                    .label(t!("group.field.username"))
                    .child(Input::new(&self.username)),
            )
            .child(
                field()
                    .label(t!("group.field.profile"))
                    .description(prose(t!("group.field.profile_hint")))
                    .child(crate::choice::select(&self.profile)),
            )
            .child(
                field()
                    .label(t!("group.field.jump"))
                    .child(crate::choice::select(&self.jump)),
            )
            .child(
                field()
                    .label(t!("group.field.proxy_command"))
                    .description(prose(t!("group.field.proxy_command_hint")))
                    .child(Input::new(&self.proxy_command).id("group-proxy-command")),
            )
            .child(
                field()
                    .label(t!("group.field.scheme"))
                    .child(crate::choice::select(&self.scheme)),
            )
        } else {
            form
        };
        v_flex()
            .gap_3()
            .when(editing, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(prose(t!("group.edit.hint"))),
                )
            })
            .child(form)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(prose(error)),
                )
            })
    }
}

/// 凭据档案的选项："不设置"，然后按名称排列，写作"名称（用户名）"。
pub fn profile_choices(store: &Store) -> Vec<Choice<Option<ProfileId>>> {
    let mut profiles = store.list::<Profile>();
    profiles.sort_by_key(|p| p.doc.name.to_lowercase());
    std::iter::once(Choice::new(t!("group.field.not_set"), None))
        .chain(profiles.into_iter().map(|p| {
            Choice::new(
                t!(
                    "choice.name_with_detail",
                    name = p.doc.name,
                    detail = p.doc.username
                ),
                Some(p.id),
            )
        }))
        .collect()
}

/// 跳板机的选项："不设置"，然后是全部 SSH 主机（除了 `exclude`），写作"名称（用户@地址）"。
pub fn jump_choices(store: &Store, exclude: Option<&HostId>) -> Vec<Choice<Option<HostId>>> {
    std::iter::once(Choice::new(t!("group.field.not_set"), None))
        .chain(jump_hosts(store, exclude))
        .collect()
}

/// 可以做跳板机的主机（SSH，按名称排列）。
pub fn jump_hosts(store: &Store, exclude: Option<&HostId>) -> Vec<Choice<Option<HostId>>> {
    let mut hosts: Vec<_> = store
        .list::<Host>()
        .into_iter()
        .filter(|h| h.doc.is_ssh() && Some(&h.id) != exclude)
        .collect();
    hosts.sort_by_key(|h| h.doc.name.to_lowercase());
    hosts
        .into_iter()
        .map(|h| {
            let detail = if h.doc.username.is_empty() {
                h.doc.address.clone()
            } else {
                format!("{}@{}", h.doc.username, h.doc.address)
            };
            Choice::new(
                t!(
                    "choice.name_with_detail",
                    name = h.doc.name,
                    detail = detail
                ),
                Some(h.id),
            )
        })
        .collect()
}

/// 终端配色的选项：`empty` 表示不指定（跟随全局或沿用分组），然后是内置的浅色、深色，最后是自定义的。
pub fn scheme_choices(store: &Store, empty: SharedString) -> Vec<Choice<String>> {
    let builtin = schemes::builtin_schemes();
    let light = builtin.iter().filter(|s| !s.is_dark());
    let dark = builtin.iter().filter(|s| s.is_dark());
    let mut custom = store.list::<ColorScheme>();
    custom.sort_by_key(|s| s.doc.name.to_lowercase());
    std::iter::once(Choice::new(empty, String::new()))
        .chain(
            light
                .chain(dark)
                .map(|s| Choice::new(s.name().to_string(), s.id().to_string())),
        )
        .chain(
            custom
                .into_iter()
                .map(|s| Choice::new(s.doc.name.clone(), s.id.as_str().to_string())),
        )
        .collect()
}

/// 确认后删除分组：子分组一并删除，其中的主机移到"未分组"。
pub fn confirm_delete(id: GroupId, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    let Some(group) = store.get::<Group>(&id) else {
        return;
    };
    let name = group.doc.name;
    crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
        let store = store.clone();
        let id = id.clone();
        alert
            .confirm()
            .title(t!("group.delete.title", name = name.clone()))
            .description(prose(t!("group.delete.description")))
            .ok_text(t!("common.delete"))
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("common.cancel"))
            .on_ok(move |_, window, cx| {
                tracing::info!(id = %id, "删除分组");
                if let Err(e) = store.delete::<Group>(&id) {
                    crate::notify::error(store_errors::describe(&e), window, cx);
                }
                true
            })
    });
}
