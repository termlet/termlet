//! 本机栏（SFTP 双栏的左侧）：浏览本机的文件，上传到右侧的远程目录，接收从远程栏拖来的文件（下载到这里）。
//!
//! - 打开时依次尝试上次所在的目录、上次下载到的目录、“下载”、主目录。
//! - 双击目录进入；双击文件用默认程序打开，会直接运行的文件（程序、脚本、快捷方式）先确认。
//! - 访问网络位置（`\\server\share`）前确认一次（Windows 会把当前账户的登录信息发给那台服务器），本次运行中记住。
//! - 删除是移到回收站。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, EntityId, EventEmitter, Focusable as _,
    Global, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::sftp::{self, LocalEntry, LocalPlaces};

use super::file_list::{self, DragSource, DraggedFiles, FileList, FileListEvent, FileRow, PARENT};
use super::{errors, list_model, runnable};
use crate::app_state::AppState;
use crate::i18n::LocalizedPlaceholder as _;
use crate::prose::prose;
use crate::t;

/// 本次运行中允许访问过的网络位置（服务器名，小写）。
#[derive(Default)]
struct AllowedServers(HashSet<String>);

impl Global for AllowedServers {}

/// 本机栏请求所在的面板做的事。
pub enum LocalPaneEvent {
    /// 上传这些本机文件或目录到远程的当前目录
    Upload(Vec<PathBuf>),
    /// 远程栏的文件拖到了这里：下载到 `into`
    DropRemote { files: DraggedFiles, into: PathBuf },
}

pub struct LocalPane {
    dir: Option<PathBuf>,
    parent: Option<PathBuf>,
    entries: Vec<LocalEntry>,
    path: Entity<InputState>,
    list: Entity<FileList>,
    places: Option<LocalPlaces>,
    loading: bool,
    /// 每次加载的编号：只采用最后一次的结果
    seq: u64,
    error: Option<SharedString>,
    /// 远程可用（上传按钮）
    remote_ready: bool,
    _load: Option<Task<()>>,
    _places: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<LocalPaneEvent> for LocalPane {}

impl LocalPane {
    /// `panel` 为所在的 SFTP 面板：只接收它的远程栏拖来的文件。
    pub fn new(panel: EntityId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = Self::path_input(window, cx);
        let list = cx.new(|cx| {
            let mut list = FileList::new("local-list", DragSource::Local, t!("local.empty"), cx);
            list.set_peer(DragSource::Remote(panel));
            list
        });
        let pane = cx.entity().downgrade();
        list.update(cx, |list, _| {
            list.set_menu(move |menu, window, cx| match pane.upgrade() {
                Some(pane) => pane.read(cx).build_menu(menu, &pane, window, cx),
                None => menu,
            })
        });
        let subscriptions = Self::subscribe(&list, &path, window, cx);
        let places = cx.spawn_in(window, async move |this, cx| {
            let places = termlet_core::runtime::spawn(sftp::local_places()).await;
            let Ok(places) = places else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let ui = AppState::global(cx).ui.read(cx).state().clone();
                let candidates: Vec<PathBuf> = [
                    ui.sftp_local_dir,
                    ui.sftp_download_dir,
                    places.downloads.clone(),
                    places.home.clone(),
                ]
                .into_iter()
                .flatten()
                .collect();
                this.places = Some(places);
                this.open_first(candidates, window, cx);
            });
        });
        Self {
            dir: None,
            parent: None,
            entries: Vec::new(),
            path,
            list,
            places: None,
            loading: false,
            seq: 0,
            error: None,
            remote_ready: false,
            _load: None,
            _places: places,
            _subscriptions: subscriptions,
        }
    }

    /// 路径框（与所在的窗口绑定）。
    fn path_input(window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        cx.new(|cx| {
            InputState::new(window, cx).localized_placeholder(|| t!("local.path"), window, cx)
        })
    }

    /// 对列表与路径框的订阅。
    fn subscribe(
        list: &Entity<FileList>,
        path: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        vec![
            cx.subscribe_in(list, window, Self::on_list_event),
            // 选择变了：“上传”按钮可用与否
            cx.observe(list, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                path,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        let text = input.read(cx).value().trim().to_string();
                        if !text.is_empty() {
                            this.load(PathBuf::from(text), window, cx);
                            this.list.focus_handle(cx).focus(window, cx);
                        }
                    }
                },
            ),
        ]
    }

    /// 所在的文件管理移到别的窗口（见 `sftp/panel/rebind.rs`）：路径框绑在打开它的窗口上，在新窗口中重建（保留没提交的
    /// 输入），订阅随之重新登记。
    pub(crate) fn rebind_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.path.read(cx).value();
        self.path = Self::path_input(window, cx);
        self.path
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self._subscriptions = Self::subscribe(&self.list, &self.path, window, cx);
    }

    /// 列表（测试中核对选择）。
    #[cfg(test)]
    pub(crate) fn list(&self) -> Entity<FileList> {
        self.list.clone()
    }

    /// 当前目录（还没打开时为 `None`）。
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn set_remote_ready(&mut self, ready: bool, cx: &mut Context<Self>) {
        if self.remote_ready != ready {
            self.remote_ready = ready;
            cx.notify();
        }
    }

    /// 依次尝试打开，用第一个能打开的。
    fn open_first(
        &mut self,
        candidates: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.seq += 1;
        let seq = self.seq;
        self.loading = true;
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let job = termlet_core::runtime::spawn(async move {
                for dir in candidates {
                    // 网络位置要先确认，打开时不自动访问
                    if sftp::network_server(&dir).is_some() {
                        continue;
                    }
                    if let Ok(listing) = sftp::list_local(&dir).await {
                        return Some(listing);
                    }
                }
                None
            });
            let listing = job.await.ok().flatten();
            let _ = this.update_in(cx, |this, window, cx| {
                if this.seq != seq {
                    return;
                }
                this.loading = false;
                if let Some(listing) = listing {
                    this.apply(listing, window, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 打开一个目录。网络位置先确认。
    pub fn load(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(server) = sftp::network_server(&dir)
            && !cx
                .try_global::<AllowedServers>()
                .is_some_and(|allowed| allowed.0.contains(&server))
        {
            let pane = cx.entity().downgrade();
            crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
                let (allow, deny) = (pane.clone(), pane.clone());
                let (server, dir) = (server.clone(), dir.clone());
                alert
                    .confirm()
                    .title(t!("local.network_title"))
                    .description(prose(t!("local.network_detail")))
                    .ok_text(t!("local.network_allow"))
                    .cancel_text(t!("common.cancel"))
                    .on_ok(move |_, window, cx| {
                        cx.default_global::<AllowedServers>()
                            .0
                            .insert(server.clone());
                        let _ = allow.update(cx, |this, cx| this.load(dir.clone(), window, cx));
                        true
                    })
                    .on_cancel(move |_, window, cx| {
                        let _ = deny.update(cx, |this, cx| {
                            this.error = Some(t!("local.error_network"));
                            this.restore_path(window, cx);
                            cx.notify();
                        });
                        true
                    })
            });
            return;
        }
        self.seq += 1;
        let seq = self.seq;
        self.loading = true;
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let job = termlet_core::runtime::spawn(async move { sftp::list_local(&dir).await });
            let result = match job.await {
                Ok(result) => result,
                Err(_) => return,
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if this.seq != seq {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(listing) => this.apply(listing, window, cx),
                    Err(error) => {
                        this.error = Some(errors::describe(&error));
                        this.restore_path(window, cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 重新读取当前目录（保留仍然存在的选择）。
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dir) = self.dir.clone() {
            self.load(dir, window, cx);
        }
    }

    /// 新建、改名后刷新，选中名为 `name` 的新项（被筛选筛掉时先清空筛选）。
    fn refresh_to(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.list
            .update(cx, |list, _| list.reveal_after_refresh(name));
        self.refresh(window, cx);
    }

    fn apply(&mut self, listing: sftp::LocalListing, window: &mut Window, cx: &mut Context<Self>) {
        let same = self.dir.as_deref() == Some(listing.path.as_path());
        let rows: Vec<FileRow> = listing
            .entries
            .iter()
            .map(|e| FileRow {
                name: e.name.clone(),
                is_dir: e.is_dir(),
                is_link: e.is_link(),
                hidden: e.hidden,
                size: e.size,
                modified: (e.modified > 0).then_some(e.modified * 1000),
                mode: None,
            })
            .collect();
        let dir_text = listing.path.to_string_lossy().to_string();
        let parent = listing.parent.is_some();
        self.list.update(cx, |list, cx| {
            if same {
                list.refresh_entries(rows, cx)
            } else {
                list.set_entries(dir_text.clone(), rows, parent, cx)
            }
        });
        self.error = None;
        self.entries = listing.entries;
        self.parent = listing.parent;
        let dir = listing.path;
        self.path
            .update(cx, |input, cx| input.set_value(dir_text, window, cx));
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.sftp_local_dir = Some(dir.clone()), cx)
        });
        self.dir = Some(dir);
    }

    fn restore_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .dir
            .as_ref()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default();
        self.path
            .update(cx, |input, cx| input.set_value(text, window, cx));
    }

    fn go_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.parent.clone() {
            self.load(parent, window, cx);
        }
    }

    fn selected_paths(&self, cx: &App) -> Vec<PathBuf> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        self.list
            .read(cx)
            .selected()
            .into_iter()
            .map(|row| dir.join(row.name))
            .collect()
    }

    fn upload_selected(&mut self, cx: &mut Context<Self>) {
        let paths = self.selected_paths(cx);
        if self.remote_ready && !paths.is_empty() {
            cx.emit(LocalPaneEvent::Upload(paths));
        }
    }

    /// 双击或 Enter：目录进入，文件用默认程序打开（会直接运行的先确认）。
    fn open(&mut self, row: &FileRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let path = dir.join(&row.name);
        if row.is_dir {
            self.load(path, window, cx);
            return;
        }
        if !runnable::is_runnable(&row.name, runnable::system_pathext().as_deref()) {
            tracing::info!(path = %path.display(), "用默认程序打开本机文件");
            cx.open_with_system(&path);
            return;
        }
        let name = list_model::visible_name(&row.name);
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let path = path.clone();
            alert
                .confirm()
                .title(t!("local.run_title"))
                .description(prose(t!("local.run_detail", name = name.clone())))
                .ok_text(t!("local.run"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, _, cx| {
                    tracing::info!(path = %path.display(), "确认后运行本机的程序或脚本");
                    cx.open_with_system(&path);
                    true
                })
        });
    }

    fn copy_paths(&self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected_paths(cx);
        if paths.is_empty() {
            return;
        }
        let text = paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        let message = if paths.len() == 1 {
            t!("sftp.copied", text = text)
        } else {
            t!("sftp.paths_copied", count = paths.len())
        };
        window.push_notification(Notification::success(message), cx);
    }

    fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let pane = cx.entity().downgrade();
        super::prompt_name(
            t!("sftp.new_folder"),
            String::new(),
            "local-new-folder",
            Box::new(move |name, window, cx| {
                let dir = dir.clone();
                let pane = pane.clone();
                tracing::info!(dir = %dir.display(), name = %name, "本机新建文件夹");
                let created = name.clone();
                let job =
                    termlet_core::runtime::spawn(
                        async move { sftp::make_local_dir(&dir, &name).await },
                    );
                window.spawn(cx, async move |cx| {
                    let result = job.await.map_err(|e| SharedString::from(e.to_string()))?;
                    result.map_err(|e| errors::describe(&e))?;
                    let _ =
                        pane.update_in(cx, |this, window, cx| this.refresh_to(created, window, cx));
                    Ok(())
                })
            }),
            window,
            cx,
        );
    }

    fn rename(&mut self, row: &FileRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let pane = cx.entity().downgrade();
        let from = row.name.clone();
        super::prompt_name(
            t!("sftp.rename"),
            from.clone(),
            "local-rename",
            Box::new(move |name, window, cx| {
                let (dir, from, pane) = (dir.clone(), from.clone(), pane.clone());
                let unchanged = name == from;
                if !unchanged {
                    tracing::info!(dir = %dir.display(), from = %from, to = %name, "本机重命名");
                }
                let renamed = name.clone();
                let job = termlet_core::runtime::spawn(async move {
                    if unchanged {
                        return Ok(());
                    }
                    sftp::rename_local(&dir, &from, &name).await
                });
                window.spawn(cx, async move |cx| {
                    let result = job.await.map_err(|e| SharedString::from(e.to_string()))?;
                    result.map_err(|e| errors::describe(&e))?;
                    let _ =
                        pane.update_in(cx, |this, window, cx| this.refresh_to(renamed, window, cx));
                    Ok(())
                })
            }),
            window,
            cx,
        );
    }

    fn confirm_trash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let names: Vec<String> = self
            .list
            .read(cx)
            .selected()
            .into_iter()
            .map(|r| r.name)
            .collect();
        if names.is_empty() {
            return;
        }
        let shown = super::join_names(&names);
        let pane = cx.entity().downgrade();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (dir, names, pane) = (dir.clone(), names.clone(), pane.clone());
            alert
                .confirm()
                .title(t!("local.confirm_trash", names = shown.clone()))
                .ok_text(t!("local.trash"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = pane.update(cx, |this, cx| {
                        this.trash(dir.clone(), names.clone(), window, cx)
                    });
                    true
                })
        });
    }

    fn trash(
        &mut self,
        dir: PathBuf,
        names: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(dir = %dir.display(), count = names.len(), "本机移到回收站");
        let job =
            termlet_core::runtime::spawn(async move { sftp::trash_local(&dir, &names).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Ok(Err(error)) = result {
                    tracing::warn!("移到回收站失败：{error}");
                    this.error = Some(errors::describe(&error));
                }
                this.refresh(window, cx);
            });
        })
        .detach();
    }

    fn on_list_event(
        &mut self,
        _: &Entity<FileList>,
        event: &FileListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            FileListEvent::Open(row) => self.open(row, window, cx),
            FileListEvent::Up => self.go_up(window, cx),
            FileListEvent::Rename(row) => {
                // 本机栏只在单选时重命名
                if self.list.read(cx).selected().len() == 1 {
                    self.rename(row, window, cx)
                }
            }
            FileListEvent::Refresh => self.refresh(window, cx),
            FileListEvent::Delete => self.confirm_trash(window, cx),
            FileListEvent::CopyPaths => self.copy_paths(window, cx),
            FileListEvent::PasteFiles
            | FileListEvent::DropExternal { .. }
            | FileListEvent::FindInTerminal(_) => {}
            FileListEvent::Drop { files, into } => {
                let Some(dir) = self.dir.clone() else {
                    return;
                };
                let into = match into.as_deref() {
                    // 放在“..”上：上一级目录
                    Some(PARENT) => self.parent.clone().unwrap_or(dir),
                    Some(name) => dir.join(name),
                    None => dir,
                };
                cx.emit(LocalPaneEvent::DropRemote {
                    files: files.clone(),
                    into,
                });
            }
            FileListEvent::NewFolder => self.new_folder(window, cx),
            FileListEvent::FocusPath => self.path.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            }),
        }
    }

    fn build_menu(
        &self,
        menu: gpui_kit::component::menu::PopupMenu,
        pane: &Entity<Self>,
        _: &mut Window,
        cx: &App,
    ) -> gpui_kit::component::menu::PopupMenu {
        let selected = self.list.read(cx).selected();
        let has_dir = self.dir.is_some();
        let weak = pane.downgrade();
        let act = |f: fn(&mut LocalPane, &mut Window, &mut Context<LocalPane>)| {
            let weak = weak.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = weak.update(cx, |this, cx| f(this, window, cx));
            }
        };
        let mut menu = menu;
        if !selected.is_empty() {
            menu = menu.item(
                PopupMenuItem::new(t!("local.upload"))
                    .disabled(!self.remote_ready)
                    .on_click(act(|this, _, cx| this.upload_selected(cx))),
            );
            if let [row] = selected.as_slice() {
                let open_row = row.clone();
                let weak_open = weak.clone();
                menu = menu.item(
                    PopupMenuItem::new(if row.is_dir {
                        t!("local.enter")
                    } else {
                        t!("local.open")
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak_open.update(cx, |this, cx| this.open(&open_row, window, cx));
                    }),
                );
                if let Some(dir) = &self.dir {
                    let path = dir.join(&row.name);
                    menu = menu.item(
                        PopupMenuItem::new(t!("sftp.show_in_folder"))
                            .on_click(move |_, _, cx| cx.reveal_path(&path)),
                    );
                }
                let rename_row = row.clone();
                let weak_rename = weak.clone();
                menu = menu.item(PopupMenuItem::new(t!("sftp.rename")).on_click(
                    move |_, window, cx| {
                        let _ =
                            weak_rename.update(cx, |this, cx| this.rename(&rename_row, window, cx));
                    },
                ));
            }
            menu = menu
                .item(
                    PopupMenuItem::new(t!("sftp.copy_path"))
                        .on_click(act(|this, window, cx| this.copy_paths(window, cx))),
                )
                .item(
                    PopupMenuItem::new(t!("local.trash"))
                        .on_click(act(|this, window, cx| this.confirm_trash(window, cx))),
                )
                .separator();
        }
        menu.item(
            PopupMenuItem::new(t!("sftp.new_folder_menu"))
                .disabled(!has_dir)
                .on_click(act(|this, window, cx| this.new_folder(window, cx))),
        )
        .item(
            PopupMenuItem::new(t!("sftp.refresh"))
                .disabled(!has_dir)
                .on_click(act(|this, window, cx| this.refresh(window, cx))),
        )
    }

    fn render_places(&self, cx: &Context<Self>) -> gpui_kit::AnyElement {
        let places = self.places.clone().unwrap_or_default();
        let pane = cx.entity().downgrade();
        Button::new("local-places")
            .ghost()
            .small()
            .icon(IconName::HardDrive)
            .tooltip(t!("local.places"))
            .dropdown_menu(move |menu, _, _| {
                let go = |dir: PathBuf| {
                    let pane = pane.clone();
                    move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = pane.update(cx, |this, cx| this.load(dir.clone(), window, cx));
                    }
                };
                let named = [
                    (t!("local.place_desktop"), places.desktop.clone()),
                    (t!("local.place_downloads"), places.downloads.clone()),
                    (t!("local.place_documents"), places.documents.clone()),
                    (t!("local.place_home"), places.home.clone()),
                ];
                let mut menu = menu;
                for (label, dir) in named {
                    if let Some(dir) = dir {
                        menu = menu.item(PopupMenuItem::new(label).on_click(go(dir)));
                    }
                }
                menu = menu.separator();
                for root in &places.roots {
                    menu = menu.item(
                        PopupMenuItem::new(root.to_string_lossy().to_string())
                            .on_click(go(root.clone())),
                    );
                }
                menu
            })
            .into_any_element()
    }
}

impl Render for LocalPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let places = self.render_places(cx);
        let danger = cx.theme().danger;
        let mono = cx.theme().mono_font_family.clone();
        let has_dir = self.dir.is_some();
        let has_selection = !self.list.read(cx).selected().is_empty();
        let icon_button = |id: &'static str, icon: IconName, tip: SharedString| {
            Button::new(id).ghost().small().icon(icon).tooltip(tip)
        };
        v_flex()
            .size_full()
            .min_w_0()
            .child(
                h_flex()
                    .flex_none()
                    .gap_0p5()
                    .px_1()
                    .py_1()
                    .overflow_hidden()
                    .child(places)
                    .child(
                        icon_button("local-up", IconName::FolderUp, t!("sftp.up"))
                            .disabled(self.parent.is_none())
                            .on_click(cx.listener(|this, _, window, cx| this.go_up(window, cx))),
                    )
                    .child(
                        icon_button("local-refresh", IconName::RefreshCw, t!("sftp.refresh"))
                            .loading(self.loading)
                            .disabled(!has_dir)
                            .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                    )
                    .child(file_list::hidden_files_button(
                        "local-hidden",
                        DragSource::Local,
                        cx,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w(rems(4.))
                            // Esc：放弃没有提交的输入，回到列表
                            .key_context(file_list::PATH_CONTEXT)
                            .on_action(cx.listener(|this, _: &file_list::FocusList, window, cx| {
                                this.restore_path(window, cx);
                                this.list.focus_handle(cx).focus(window, cx);
                            }))
                            .child(
                                Input::new(&self.path)
                                    .small()
                                    .id("local-path")
                                    .font_family(mono),
                            ),
                    )
                    .child(
                        icon_button(
                            "local-new-folder-button",
                            IconName::FolderPlus,
                            t!("sftp.new_folder"),
                        )
                        .disabled(!has_dir)
                        .on_click(cx.listener(|this, _, window, cx| this.new_folder(window, cx))),
                    )
                    .child(
                        icon_button("local-upload", IconName::ArrowRight, t!("local.upload"))
                            .disabled(!self.remote_ready || !has_selection)
                            .on_click(cx.listener(|this, _, _, cx| this.upload_selected(cx))),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .flex_none()
                        .px_3()
                        .gap_2()
                        .min_h(rems(1.5))
                        .text_xs()
                        .bg(danger.opacity(0.15))
                        .text_color(danger)
                        .child(div().flex_1().min_w_0().truncate().child(prose(error)))
                        .child(
                            Button::new("local-error-close")
                                .ghost()
                                .small()
                                .icon(IconName::Close)
                                .tooltip(t!("common.close"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(div().flex_1().min_h_0().child(self.list.clone()))
    }
}

impl gpui_kit::Focusable for LocalPane {
    fn focus_handle(&self, cx: &App) -> gpui_kit::FocusHandle {
        self.list.focus_handle(cx)
    }
}
