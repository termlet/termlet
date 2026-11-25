//! 设置“主机指纹”中的信任的主机 CA（OpenSSH known_hosts 中的 `@cert-authority`）：它们签发的主机证书在第一次连接时
//! 免去指纹确认。可以添加（主机名模式 + CA 公钥，或粘贴一整行 `@cert-authority`）、从 known_hosts 文件导入、
//! 不再信任（立即生效，与“保存”无关）。只在这台电脑上，不同步。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _,
    Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::model::TrustedHostCa;
use termlet_core::ssh::host_ca::{self, HostCaError, KnownHostsReadError};

use crate::app_state::AppState;
use crate::dialog_token::{DialogToken, close_own};
use crate::notify::{self, Notice};
use crate::prose::prose;
use crate::{store_errors, t};

pub(super) struct HostCasSection {
    cas: Vec<TrustedHostCa>,
}

impl HostCasSection {
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self { cas: Vec::new() };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.cas = host_ca::list(&AppState::store(cx));
        cx.notify();
    }

    fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| this.import_file(path, window, cx));
        })
        .detach();
    }

    /// 从选好的 known_hosts 文件导入。结果写在设置底部的提示行里（[`notify::show`]），靠近做操作的地方。
    fn import_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let store = AppState::store(cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { host_ca::import_known_hosts(&store, &path) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.reload(cx);
                let notice = match result {
                    Ok(report) if report.added + report.existing + report.invalid == 0 => {
                        Notice::warning(t!("known_hosts.ca_import_none"))
                    }
                    Ok(report) => Notice::success(t!(
                        "known_hosts.ca_imported",
                        added = report.added,
                        existing = report.existing,
                        invalid = report.invalid
                    )),
                    Err(error) => Notice::error(t!(
                        "known_hosts.ca_import_failed",
                        error = read_error(&error)
                    )),
                };
                notify::show(notice, window, cx);
            });
        })
        .detach();
    }

    fn confirm_remove(&mut self, ca: TrustedHostCa, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let hosts = ca.hosts.clone();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (this, ca) = (this.clone(), ca.clone());
            alert
                .confirm()
                .title(t!("known_hosts.ca_delete_title", hosts = hosts.clone()))
                .description(prose(t!("known_hosts.ca_delete_description")))
                .ok_text(t!("known_hosts.ca_delete"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    if let Err(e) = host_ca::remove(&AppState::store(cx), &ca) {
                        notify::show(Notice::error(store_errors::describe(&e)), window, cx);
                    }
                    let _ = this.update(cx, |this, cx| this.reload(cx));
                    true
                })
        });
    }
}

fn read_error(error: &KnownHostsReadError) -> SharedString {
    match error {
        KnownHostsReadError::NotFound => t!("keys.error.file_not_found_short"),
        KnownHostsReadError::TooLarge => t!("known_hosts.ca_file_too_large"),
        KnownHostsReadError::Io(reason) | KnownHostsReadError::Store(reason) => {
            reason.clone().into()
        }
    }
}

/// CA 公钥的显示：算法与 SHA256 指纹；解析不了时原样显示。
fn key_summary(public_key: &str) -> String {
    match host_ca::describe_key(public_key) {
        Some((algorithm, fingerprint)) => format!("{algorithm} {fingerprint}"),
        None => public_key.to_string(),
    }
}

impl Render for HostCasSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let this = cx.entity().downgrade();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_medium()
                            .child(t!("known_hosts.ca_title")),
                    )
                    .child(
                        Button::new("host-ca-import")
                            .ghost()
                            .small()
                            .icon(IconName::FileDown)
                            .label(t!("known_hosts.ca_import"))
                            .on_click(cx.listener(|this, _, window, cx| this.import(window, cx))),
                    )
                    .child(
                        Button::new("host-ca-add")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .label(t!("known_hosts.ca_add"))
                            .on_click(move |_, window, cx| open_add(this.clone(), window, cx)),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("known_hosts.ca_description"))),
            )
            .when(self.cas.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(prose(t!("known_hosts.ca_empty"))),
                )
            })
            .children(self.cas.iter().map(|ca| {
                // 同一个 CA 可以对不同的主机名模式各有一条：两者一起作为标识
                let key = format!("{}|{}", ca.hosts, key_summary(&ca.public_key));
                let remove = ca.clone();
                h_flex()
                    .id(ElementId::from(SharedString::from(format!(
                        "host-ca-{key}"
                    ))))
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .child(t!("known_hosts.ca_hosts", hosts = ca.hosts.clone())),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                                    .child(key_summary(&ca.public_key)),
                            )
                            .when(ca.added_at > 0, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(termlet_core::time::format_local(ca.added_at)),
                                )
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("host-ca-delete-{key}")))
                            .ghost()
                            .small()
                            .icon(IconName::Trash)
                            .tooltip(t!("known_hosts.ca_delete"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_remove(remove.clone(), window, cx)
                            })),
                    )
            }))
    }
}

/// 添加信任的 CA 的对话框。
fn open_add(section: WeakEntity<HostCasSection>, window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| AddForm::new(section, window, cx));
    let first = Rc::new(Cell::new(true));
    let width = rems(32.).to_pixels(window.rem_size());
    let owner = form.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            form.read(cx).hosts.focus_handle(cx).focus(window, cx);
        }
        let ok = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.submit(window, cx));
                false
            }
        };
        dialog
            .title(t!("known_hosts.ca_add_title"))
            .w(width)
            .child(form.clone())
            .on_ok(ok)
    });
    let token = DialogToken::take(window, cx);
    owner.update(cx, |form, _| form.dialog = token);
}

struct AddForm {
    section: WeakEntity<HostCasSection>,
    hosts: Entity<InputState>,
    key: Entity<TextareaState>,
    error: Option<SharedString>,
    dialog: Option<DialogToken>,
    _subscriptions: Vec<Subscription>,
}

impl AddForm {
    fn new(
        section: WeakEntity<HostCasSection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let hosts = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("known_hosts.ca_hosts_placeholder"))
        });
        let key = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(3)
                .placeholder(t!("known_hosts.ca_key_placeholder"))
        });
        let subscriptions = vec![
            cx.subscribe(&hosts, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.error = None;
                    cx.notify();
                }
            }),
            cx.observe(&key, |this: &mut Self, _, cx| {
                this.error = None;
                cx.notify();
            }),
        ];
        Self {
            section,
            hosts,
            key,
            error: None,
            dialog: None,
            _subscriptions: subscriptions,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hosts = self.hosts.read(cx).value().trim().to_string();
        let key = self.key.read(cx).value().trim().to_string();
        // 粘贴的是一整行 @cert-authority：主机名模式取自这一行（另外填了的以填写的为准）
        let parsed = if key.starts_with("@cert-authority") {
            host_ca::parse_line(&key).and_then(|ca| {
                if hosts.is_empty() {
                    Ok(ca)
                } else {
                    host_ca::new_ca(&hosts, &ca.public_key)
                }
            })
        } else {
            host_ca::new_ca(&hosts, &key)
        };
        let ca = match parsed {
            Ok(ca) => ca,
            Err(error) => {
                self.error = Some(match error {
                    HostCaError::InvalidHosts => t!("known_hosts.ca_invalid_hosts"),
                    HostCaError::InvalidKey | HostCaError::NotCertAuthority => {
                        t!("known_hosts.ca_invalid_key")
                    }
                });
                cx.notify();
                return;
            }
        };
        if let Err(e) = host_ca::add(&AppState::store(cx), vec![ca]) {
            self.error = Some(store_errors::describe(&e));
            cx.notify();
            return;
        }
        let _ = self.section.update(cx, |section, cx| section.reload(cx));
        close_own(self.dialog.as_ref(), window, cx);
    }
}

impl Render for AddForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let empty = self.key.read(cx).value().trim().is_empty();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(prose(t!("known_hosts.ca_add_hint"))),
            )
            .child(Input::new(&self.hosts).id("host-ca-hosts"))
            .child(Textarea::new(&self.key).font_family(theme.mono_font_family.clone()))
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.danger).child(prose(error)))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("host-ca-cancel")
                            .ghost()
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("host-ca-submit")
                            .primary()
                            .label(t!("known_hosts.ca_trust"))
                            .disabled(empty)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestWindowExt as _;

    use super::*;
    use crate::i18n::{Language, set_language};
    use crate::notify::shown;
    use crate::settings::Page;
    use crate::testkit::{open_workspace, poll_until};

    // 设置开着时从 known_hosts 导入：结果写在设置底部的提示行里、靠近做操作的地方，不弹出通知
    #[gpui_kit::test]
    fn import_result_shows_inside_settings(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        let dir = std::env::temp_dir().join(format!(
            "termlet-host-ca-{}",
            termlet_core::id::random_token(8)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        let ca =
            termlet_core::sshkey::generate(termlet_core::sshkey::KeyKind::Ed25519, "").unwrap();
        std::fs::write(
            &path,
            format!("@cert-authority *.corp {}\n", ca.public_key()),
        )
        .unwrap();
        // 导入完成前一直拿着这一节（丢弃后导入的结果不再处理）
        let (toasts, _section) = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.click("open-settings", cx);
                window.render_frame(cx);
                window.click(Page::KnownHosts.nav_id(), cx);
                window.render_frame(cx);
                let toasts = window.notifications(cx).len();
                // 页上的那一节与它做的相同：选好文件后导入
                let section = cx.new(HostCasSection::new);
                section.update(cx, |section, cx| {
                    section.import_file(path.clone(), window, cx)
                });
                (toasts, section)
            })
            .unwrap();
        let expected = Notice::success(t!(
            "known_hosts.ca_imported",
            added = 1,
            existing = 0,
            invalid = 0
        ));
        poll_until(cx, "导入完成", |cx| {
            cx.update_window(handle, |_, window, cx| shown(window, cx).is_some())
                .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(shown(window, cx), Some(expected));
            assert_eq!(window.notifications(cx).len(), toasts, "不弹出通知");
            let tab = window.find("settings-tab").bounds();
            let notice = window.find("dialog-notice").bounds();
            assert!(
                notice.left() >= tab.left()
                    && notice.right() <= tab.right()
                    && notice.top() >= tab.top()
                    && notice.bottom() <= tab.bottom(),
                "提示在设置之内：{notice:?} / {tab:?}"
            );
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
