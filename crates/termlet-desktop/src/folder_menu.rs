//! 资源管理器右键菜单中的“在此处打开 Termlet”（只在 Windows 上；写注册表见 termlet-core 的 `folder_menu`）：
//! 设置“本地终端”中的开关（[`FolderMenuSection`]），与启动时跟随程序的位置（[`follow`]）。
//!
//! 开关立即生效，与设置的“保存”无关：改的是系统的设置，不记在 Termlet 的设置中，开关显示的就是注册表现在的样子。
//! 菜单上的文字按打开开关时的界面语言写入。菜单指向别处还在的 Termlet（另一份便携版）时开关是关的，下面写明
//! 指向哪里，打开就改为指向这一个。

use std::path::PathBuf;

use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Global, IntoElement, ParentElement as _, Render, Styled as _,
    Task, Window, div, prelude::FluentBuilder as _,
};
use termlet_core::folder_menu::{self, MenuState};

use crate::prose::prose;
use crate::t;

/// 放菜单键的位置（`HKCU` 下）：平时是 [`folder_menu::CLASSES`]，测试中换成临时的键。
struct Classes(String);

impl Global for Classes {}

fn classes(cx: &App) -> String {
    cx.try_global::<Classes>()
        .map_or_else(|| folder_menu::CLASSES.to_string(), |c| c.0.clone())
}

/// 测试中换成临时的键，不碰真实的右键菜单。
#[cfg(test)]
pub(crate) fn use_classes(path: &str, cx: &mut App) {
    cx.set_global(Classes(path.to_string()));
}

/// 启动时调用：菜单指向的程序已不存在（便携版移动了位置、重装到了别处）时改为指向这个程序。在后台线程读写注册表；
/// 日志不记路径（其中常有用户名）。
pub fn follow(cx: &App) {
    if !folder_menu::supported() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let classes = classes(cx);
    let _ = std::thread::Builder::new()
        .name("folder-menu".into())
        .spawn(move || match folder_menu::follow(&classes, &exe) {
            Ok(Some(_)) => tracing::info!("程序的位置变了，资源管理器右键菜单已改为指向这里"),
            Ok(None) => {}
            Err(e) => tracing::warn!("更新资源管理器右键菜单失败：{e}"),
        });
}

/// 开关的状态。
enum Status {
    /// 正在读注册表
    Checking,
    Known(MenuState),
    /// 读不了（系统的报错）
    Failed(String),
}

/// 设置“本地终端”中的开关。
pub(crate) struct FolderMenuSection {
    /// 这个程序；读不到时是系统的报错
    exe: Result<PathBuf, String>,
    status: Status,
    /// 正在修改
    busy: bool,
    _task: Option<Task<()>>,
}

impl FolderMenuSection {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            exe: std::env::current_exe().map_err(|e| e.to_string()),
            status: Status::Checking,
            busy: false,
            _task: None,
        };
        this.run(None, cx);
        this
    }

    /// 在后台按 `change`（`Some(true)` 打开、`Some(false)` 关闭、`None` 只读）改注册表，再读出现在的状态。
    fn run(&mut self, change: Option<bool>, cx: &mut Context<Self>) {
        let exe = match &self.exe {
            Ok(exe) => exe.clone(),
            Err(e) => {
                self.status = Status::Failed(e.clone());
                return;
            }
        };
        let classes = classes(cx);
        let label = t!("open_folder.menu_label").to_string();
        self.busy = change.is_some();
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    match change {
                        Some(true) => folder_menu::enable(&classes, &exe, &label)?,
                        Some(false) => folder_menu::disable(&classes)?,
                        None => {}
                    }
                    folder_menu::state(&classes, &exe)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.status = match result {
                    Ok(state) => {
                        if let Some(on) = change {
                            tracing::info!(on, state = ?state_name(&state), "资源管理器右键菜单");
                        }
                        Status::Known(state)
                    }
                    Err(e) => {
                        tracing::warn!(?change, "资源管理器右键菜单：{e}");
                        Status::Failed(e.to_string())
                    }
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 打开或关闭。
    fn set(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.run(Some(on), cx);
    }
}

/// 日志中的状态（不带路径）。
fn state_name(state: &MenuState) -> &'static str {
    match state {
        MenuState::Off => "off",
        MenuState::This => "this",
        MenuState::Other(_) => "other",
    }
}

impl Render for FolderMenuSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let checked = matches!(self.status, Status::Known(MenuState::This));
        let note = match &self.status {
            Status::Known(MenuState::Other(program)) => Some((
                t!(
                    "local_shell.folder_menu_other",
                    path = program.display().to_string()
                ),
                muted,
            )),
            Status::Failed(error) => Some((
                t!("local_shell.folder_menu_failed", error = error.clone()),
                danger,
            )),
            _ => None,
        };
        v_flex()
            .gap_1()
            .child(
                Switch::new("settings-folder-menu")
                    .label(t!("local_shell.folder_menu"))
                    .checked(checked)
                    .disabled(self.busy || matches!(self.status, Status::Checking))
                    .on_change(cx.listener(|this, on: &bool, _, cx| this.set(*on, cx))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("local_shell.folder_menu_hint"))),
            )
            .when_some(note, |this, (note, color)| {
                this.child(div().text_xs().text_color(color).child(prose(note)))
            })
    }
}
