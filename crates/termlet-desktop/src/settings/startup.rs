//! 设置“通用”页的“启动时打开”：不额外打开 / 默认的本地终端 / 指定的一台或几台主机（可调整先后）。本机设置，
//! 不同步；规则见 [`crate::startup_open`]。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::SelectEvent;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, TestSupportExt as _, Window, div,
    prelude::FluentBuilder as _,
};
use termlet_core::id::HostId;
use termlet_core::model::{Host, LocalSettings, StartupOpen, StartupSettings};

use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state};
use crate::prose::prose;
use crate::{local_shell, t};

/// 单选的几项，按界面上的顺序。
const CHOICES: [StartupOpen; 3] = [StartupOpen::Nothing, StartupOpen::Local, StartupOpen::Hosts];

pub(super) struct StartupSection {
    open: StartupOpen,
    /// 选中的主机，按打开的先后（打开设置时已删除的去掉）
    hosts: Vec<HostId>,
    /// 设置中这一组原来的值（保留不认识的字段）
    initial: StartupSettings,
    /// 默认的本地终端的名称（单选项中写出来）
    local_name: Option<SharedString>,
    /// “添加主机”的下拉框：列出还没选的主机
    add: Entity<ChoiceState<HostId>>,
    _add_subscription: Subscription,
}

impl StartupSection {
    pub(super) fn new(local: &LocalSettings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let initial = local.window.startup.clone();
        let store = AppState::store(cx);
        let hosts: Vec<HostId> = initial
            .hosts
            .iter()
            .filter(|id| store.get::<Host>(id).is_some())
            .cloned()
            .collect();
        let local_name = local_shell::default_target(local, cx).map(|t| t.title());
        let add = choice_state(Vec::new(), &HostId::from_raw(""), true, window, cx);
        let add_subscription = cx.subscribe_in(
            &add,
            window,
            |this, _, event: &SelectEvent<SearchableVec<Choice<HostId>>>, window, cx| {
                if let SelectEvent::Confirm(Some(id)) = event
                    && !this.hosts.contains(id)
                {
                    this.hosts.push(id.clone());
                    this.refresh_choices(window, cx);
                }
            },
        );
        let mut this = Self {
            open: initial.open,
            hosts,
            initial,
            local_name,
            add,
            _add_subscription: add_subscription,
        };
        this.refresh_choices(window, cx);
        this
    }

    /// 写进本机设置的草稿。
    pub(super) fn apply(&self, local: &mut LocalSettings) {
        local.window.startup = StartupSettings {
            open: self.open,
            hosts: self.hosts.clone(),
            ..self.initial.clone()
        };
    }

    /// “添加主机”的下拉框列出还没选的主机，不选中任何一项。
    fn refresh_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let choices: Vec<Choice<HostId>> = AppState::store(cx)
            .list::<Host>()
            .into_iter()
            .filter(|h| !self.hosts.contains(&h.id))
            .map(|h| Choice::new(host_label(&h.doc), h.id))
            .collect();
        self.add.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(choices), window, cx);
            state.set_selected_index(None, window, cx);
        });
        cx.notify();
    }

    fn remove(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        self.hosts.retain(|h| h != id);
        self.refresh_choices(window, cx);
    }

    fn move_host(&mut self, id: &HostId, up: bool, cx: &mut Context<Self>) {
        let Some(ix) = self.hosts.iter().position(|h| h == id) else {
            return;
        };
        let target = if up {
            ix.checked_sub(1)
        } else {
            (ix + 1 < self.hosts.len()).then_some(ix + 1)
        };
        if let Some(target) = target {
            self.hosts.swap(ix, target);
            cx.notify();
        }
    }

    fn render_hosts(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = AppState::store(cx);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let count = self.hosts.len();
        let rows: Vec<AnyElement> = self
            .hosts
            .iter()
            .enumerate()
            .map(|(ix, id)| {
                let label = store
                    .get::<Host>(id)
                    .map(|h| host_label(&h.doc))
                    .unwrap_or_else(|| id.as_str().to_string().into());
                let key = |name: &str| SharedString::from(format!("startup-host-{name}-{id}"));
                let last = ix + 1 == count;
                let (up, down, remove) = (id.clone(), id.clone(), id.clone());
                h_flex()
                    .id(key("row"))
                    .px_3()
                    .py_1()
                    .gap_2()
                    .when(!last, |row| row.border_b_1().border_color(theme.border))
                    .child(Icon::new(IconName::Server).small().text_color(muted))
                    .child(div().flex_1().min_w_0().truncate().text_sm().child(label))
                    .child(
                        Button::new(key("up"))
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowUp)
                            .tooltip(t!("common.move_up"))
                            .disabled(ix == 0)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.move_host(&up, true, cx)),
                            ),
                    )
                    .child(
                        Button::new(key("down"))
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowDown)
                            .tooltip(t!("common.move_down"))
                            .disabled(last)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.move_host(&down, false, cx)),
                            ),
                    )
                    .child(
                        Button::new(key("remove"))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t!("startup_open.remove"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove(&remove, window, cx)
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        // 主机都选上了（或者还没有主机）：没有可添加的
        let none_left = store
            .list::<Host>()
            .iter()
            .all(|h| self.hosts.contains(&h.id));
        v_flex()
            .pl_6()
            .gap_2()
            .when(rows.is_empty(), |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(prose(t!("startup_open.no_hosts"))),
                )
            })
            .when(!rows.is_empty(), |el| {
                el.child(
                    v_flex()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .children(rows),
                )
            })
            .child(
                crate::choice::select(&self.add)
                    .id("startup-add-host")
                    .small()
                    .placeholder(t!("startup_open.add_host"))
                    .search_placeholder(t!("startup_open.search_host"))
                    .disabled(none_left),
            )
            .into_any_element()
    }
}

impl Render for StartupSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let local = match &self.local_name {
            Some(name) => t!("startup_open.local_named", name = name),
            None => t!("startup_open.local"),
        };
        v_flex()
            .id("settings-startup")
            .test_support()
            .gap_2()
            .child(
                crate::radio::vertical("settings-startup-open")
                    .children([t!("startup_open.nothing"), local, t!("startup_open.hosts")])
                    .selected_index(CHOICES.iter().position(|c| *c == self.open))
                    .on_change(cx.listener(|this, ix: &usize, _, cx| {
                        if let Some(open) = CHOICES.get(*ix) {
                            this.open = *open;
                            cx.notify();
                        }
                    })),
            )
            .when(self.open == StartupOpen::Hosts, |el| {
                el.child(self.render_hosts(cx))
            })
    }
}

/// 主机在列表、下拉框中的写法：名称（用户名@地址）。
fn host_label(host: &Host) -> SharedString {
    let address = if host.username.is_empty() {
        host.address.clone()
    } else {
        format!("{}@{}", host.username, host.address)
    };
    if address.is_empty() || address == host.name {
        host.name.clone().into()
    } else {
        t!(
            "choice.name_with_detail",
            name = host.name,
            detail = address
        )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext};
    use termlet_core::id::HostId;
    use termlet_core::model::{Host, StartupOpen, StartupSettings};
    use termlet_core::store::Position;

    use super::super::{Page, SettingsForm};
    use crate::app_state::AppState;
    use crate::choice::chosen;
    use crate::testkit::open_workspace;

    fn add_host(cx: &mut gpui_kit::App, name: &str) -> HostId {
        AppState::store(cx)
            .create(
                Host {
                    name: name.into(),
                    address: "10.0.0.1".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    }

    // “启动时打开”存进本机设置（会同步的设置不变）：选指定的主机、添加、调整先后、移除后保存，再打开设置时照旧；
    // 设置中已删除的主机在打开设置时去掉
    #[gpui_kit::test]
    fn startup_choice_round_trips_locally(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        let (a, b, c) = cx.update(|cx| {
            let ids = (add_host(cx, "a"), add_host(cx, "b"), add_host(cx, "c"));
            AppState::store(cx)
                .update_local_settings(|s| {
                    s.window.startup.hosts = vec![HostId::from_raw("h_gone"), ids.1.clone()]
                })
                .unwrap();
            ids
        });
        let open_form = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                cx.new(|cx| SettingsForm::new(Page::General, window, cx))
            })
            .unwrap()
        };
        let form = open_form(cx);
        cx.update_window(handle, |_, window, cx| {
            form.update(cx, |form, cx| {
                form.startup.update(cx, |section, cx| {
                    assert_eq!(section.hosts, vec![b.clone()], "删掉的主机去掉了");
                    section.open = StartupOpen::Hosts;
                    for id in [&c, &a] {
                        section.hosts.push(id.clone());
                        section.refresh_choices(window, cx);
                    }
                    // b、c、a：a 上移两次排到最前，再移除 c
                    section.move_host(&a, true, cx);
                    section.move_host(&a, true, cx);
                    section.remove(&c, window, cx);
                });
                assert!(form.save(window, cx));
            })
        })
        .unwrap();
        cx.run_until_parked();
        let store = cx.update(|cx| AppState::store(cx));
        assert_eq!(
            store.local_settings().window.startup,
            StartupSettings {
                open: StartupOpen::Hosts,
                hosts: vec![a.clone(), b.clone()],
                ..Default::default()
            }
        );
        assert_eq!(store.settings(), Default::default(), "会同步的设置没有变");

        let reopened = open_form(cx);
        cx.update(|cx| {
            let section = reopened.read(cx).startup.read(cx);
            assert_eq!(section.open, StartupOpen::Hosts);
            assert_eq!(section.hosts, [a.clone(), b.clone()]);
            assert_eq!(chosen(&section.add, cx), None, "“添加主机”不选中任何一项");
        });
    }
}
