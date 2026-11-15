//! 导入导出与备份的界面（R11，作者：ruan）：
//!
//! - [`open_import`]：导入主机（OpenSSH、Xshell、FinalShell、MobaXterm、Tabby、Termora 与 Termlet 自己的主机文件），
//!   预览、搜索、筛选、勾选后导入，显示新建、合并、跳过与没能导入的项。
//! - [`open_export`]：导出主机（Termlet 主机文件或 ssh_config），可选包含凭据（整体用密码加密）与端口转发。
//! - [`BackupPage`]：设置中的“备份”页：完整备份与恢复、本机快照（列表、立即保存、恢复）、自动快照的设置。
//!
//! 导入、导出、备份、恢复都在核心的运行时上执行（`termlet_core::runtime::spawn`），界面只显示忙碌状态。

mod errors;
mod export;
mod import;
mod page;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, IntoElement, ParentElement as _,
    SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};
use termlet_core::backup::{Backups, Counts, MIN_PASSWORD_LEN};
use termlet_core::id::HostId;
use termlet_core::secret::SecretString;

use crate::app_state::AppState;
use crate::t;

pub(crate) use page::{BackupPage, BackupPageEvent};

/// 打开“导入主机”。
pub(crate) fn open_import(window: &mut Window, cx: &mut App) {
    import::open(window, cx);
}

/// 打开“导出主机”，勾选 `hosts`（为空时勾选全部主机）；对话框中还可以按分组或主机改选。
pub(crate) fn open_export(hosts: Vec<HostId>, window: &mut Window, cx: &mut App) {
    export::open(hosts, window, cx);
}

/// 备份、快照：应用共用的一个（核心的 `Backups` 克隆很便宜，同一时间只进行一项）。
fn backups(cx: &App) -> Backups {
    AppState::global(cx).backups.clone()
}

/// 列表中的数量：主机、密钥、凭据档案、快捷命令、端口转发。
fn counts_text(counts: &Counts) -> SharedString {
    t!(
        "backup_ui.counts",
        hosts = counts.hosts,
        keys = counts.keys,
        profiles = counts.profiles,
        snippets = counts.snippets,
        forwards = counts.forwards
    )
}

/// 其余的数量：分组、工作区、自定义配色、主机指纹、命令历史。
fn counts_more_text(counts: &Counts) -> SharedString {
    t!(
        "backup_ui.counts_more",
        groups = counts.groups,
        workspaces = counts.workspaces,
        schemes = counts.schemes,
        known_hosts = counts.known_hosts,
        history = counts.history
    )
}

/// 文件名中的时间：`2026-10-20-1530`（本地时间）。
fn file_stamp() -> String {
    termlet_core::time::format_local(termlet_core::time::now_ms())
        .replace(' ', "-")
        .replace(':', "")
}

/// 保存对话框默认打开的目录：用户的“文档”（存在时），否则主目录。
fn default_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default();
    let documents = home.join("Documents");
    if documents.is_dir() { documents } else { home }
}

fn element_id(prefix: &str, key: impl std::fmt::Display) -> ElementId {
    ElementId::from(SharedString::from(format!("{prefix}-{key}")))
}

/// 两次输入的新密码（备份、含凭据的导出）的检查结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PasswordCheck {
    Empty,
    TooShort,
    /// 第二次还没填
    Incomplete,
    Mismatch,
    Ready,
}

fn check_password(first: &str, second: &str) -> PasswordCheck {
    if first.is_empty() {
        PasswordCheck::Empty
    } else if first.chars().count() < MIN_PASSWORD_LEN {
        PasswordCheck::TooShort
    } else if second.is_empty() {
        PasswordCheck::Incomplete
    } else if first != second {
        PasswordCheck::Mismatch
    } else {
        PasswordCheck::Ready
    }
}

/// 设置新密码的两个输入框：第二次填了之后才提示太短、不一致（避免边输入边报错）。
struct PasswordPair {
    first: Entity<InputState>,
    second: Entity<InputState>,
}

impl PasswordPair {
    /// 建好两个输入框；返回的订阅在内容变化时刷新所在的视图，由视图保存。
    fn new<T: 'static>(window: &mut Window, cx: &mut Context<T>) -> (Self, Vec<Subscription>) {
        let first = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("backup_ui.password.new", min = MIN_PASSWORD_LEN))
        });
        let second = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("backup_ui.password.repeat"))
        });
        let subscriptions = [&first, &second]
            .into_iter()
            .map(|state| {
                cx.subscribe(state, |_: &mut T, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                })
            })
            .collect();
        (Self { first, second }, subscriptions)
    }

    fn check(&self, cx: &App) -> PasswordCheck {
        check_password(&self.first.read(cx).value(), &self.second.read(cx).value())
    }

    /// 两次一致且够长时的密码。
    fn secret(&self, cx: &App) -> Option<SecretString> {
        (self.check(cx) == PasswordCheck::Ready)
            .then(|| SecretString::new(self.first.read(cx).value().to_string()))
    }

    fn clear(&self, window: &mut Window, cx: &mut App) {
        for state in [&self.first, &self.second] {
            state.update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    /// 两个输入框并排，下面是提示（`prefix` 区分各处的元素 ID）。
    fn render(&self, prefix: &str, cx: &App) -> impl IntoElement + use<> {
        let check = self.check(cx);
        let repeated = !self.second.read(cx).value().is_empty();
        let problem = match check {
            PasswordCheck::TooShort if repeated => {
                Some(t!("backup_ui.password.too_short", min = MIN_PASSWORD_LEN))
            }
            PasswordCheck::Mismatch => Some(t!("backup_ui.password.mismatch")),
            _ => None,
        };
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.first)
                                .id(element_id(prefix, "password"))
                                .mask_toggle(),
                        ),
                    )
                    .child(
                        div().flex_1().child(
                            Input::new(&self.second)
                                .id(element_id(prefix, "password-repeat"))
                                .mask_toggle(),
                        ),
                    ),
            )
            .when_some(problem, |this, problem| {
                this.child(div().text_xs().text_color(cx.theme().danger).child(problem))
            })
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn new_password_needs_length_and_repeat() {
        assert_eq!(check_password("", ""), PasswordCheck::Empty);
        assert_eq!(check_password("short", "short"), PasswordCheck::TooShort);
        // 按字符计：8 个汉字够长
        assert_eq!(
            check_password("一二三四五六七八", "一二三四五六七八"),
            PasswordCheck::Ready
        );
        assert_eq!(check_password("long enough", ""), PasswordCheck::Incomplete);
        assert_eq!(
            check_password("long enough", "long enougH"),
            PasswordCheck::Mismatch
        );
        assert_eq!(
            check_password("long enough", "long enough"),
            PasswordCheck::Ready
        );
    }

    #[test]
    fn file_stamp_has_no_separators_that_windows_rejects() {
        let stamp = file_stamp();
        assert_eq!(stamp.len(), "2026-10-20-1530".len(), "{stamp}");
        assert!(!stamp.contains([':', ' ', '/', '\\']));
    }
}
