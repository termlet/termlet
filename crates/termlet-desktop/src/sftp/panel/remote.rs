//! SFTP 面板的远程栏：工具栏、提示条、右键菜单，以及新建、重命名、删除、修改权限、上传、下载、在线编辑。

use std::collections::HashSet;
use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{AlertDialog, DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _, h_flex,
};
use gpui_kit::{
    AnyElement, App, ClickEvent, ClipboardEntry, ClipboardItem, Context, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, SharedString,
    Styled as _, TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::sftp::{
    ChmodRequest, ChmodScope, DownloadRequest, Entry, MAX_EDIT_SIZE, SftpError, TextEncoding,
    UploadRequest, path,
};

use super::{COMPACT_TOOLBAR, SftpPanel, SftpPanelEvent, Status};
use crate::app_state::AppState;
use crate::prose::prose;
use crate::sftp::file_list::{self, DragSource, FileRow};
use crate::sftp::perm_dialog::{self, ChmodFailure, PermItem};
use crate::sftp::{errors, join_names};
use crate::t;

/// 列表中的一行。
pub(super) fn row_of(entry: &Entry) -> FileRow {
    FileRow {
        name: entry.name.clone(),
        is_dir: entry.is_dir(),
        is_link: entry.is_link(),
        hidden: entry.name.starts_with('.'),
        size: entry.size,
        modified: (entry.modified > 0).then_some(entry.modified * 1000),
        mode: Some(entry.mode.clone()),
    }
}

/// 远程的名称：不能为空、不能含 `/` 与 NUL、不能是 `.` 或 `..`。
fn invalid_name(name: &str) -> bool {
    name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0'])
}

impl SftpPanel {
    fn entry(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    fn selected_rows(&self, cx: &App) -> Vec<FileRow> {
        self.list.read(cx).selected()
    }

    fn selected_paths(&self, cx: &App) -> Vec<String> {
        self.selected_rows(cx)
            .into_iter()
            .map(|row| path::join(&self.dir, &row.name))
            .collect()
    }

    /// 显示错误：列表上方的提示条。
    fn show_error(&mut self, error: &SftpError, cx: &mut Context<Self>) {
        // 断线由会话状态处理
        if !error.is_disconnected() {
            tracing::warn!(sftp = %self.name, "SFTP 操作失败：{error}");
            self.error = Some(errors::describe(error));
            cx.notify();
        }
    }

    pub(super) fn go_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dir.is_empty() {
            return;
        }
        let parent = path::parent(&self.dir);
        self.load(parent, true, window, cx);
    }

    fn go_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.session() {
            let home = session.home().to_string();
            self.load(home, true, window, cx);
        }
    }

    /// 双击或 Enter：目录进入，文件在线编辑。
    pub(super) fn open_row(&mut self, row: &FileRow, window: &mut Window, cx: &mut Context<Self>) {
        let target = path::join(&self.dir, &row.name);
        if row.is_dir {
            self.load(target, true, window, cx);
        } else {
            self.edit(row.clone(), window, cx);
        }
    }

    // ------------------------------------------------------------ 在线编辑

    /// 读取文件，交给工作区打开编辑器。超过 5 MB 的问要不要下载；自动识别不了编码的（不是 UTF-8、UTF-16 文本）
    /// 问要下载还是选一种编码打开。
    fn edit(&mut self, row: FileRow, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_with(row, None, window, cx);
    }

    /// 按 `encoding` 读取并打开（为空时自动识别）。
    fn edit_with(
        &mut self,
        row: FileRow,
        encoding: Option<TextEncoding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(session), Some(lease)) = (self.session(), self.lease.clone()) else {
            return;
        };
        // 符号链接不按大小预判，读取时按目标判断
        if !row.is_link && row.size > MAX_EDIT_SIZE {
            self.offer_download(
                t!("editor.too_large", name = row.name.clone()),
                &row,
                window,
                cx,
            );
            return;
        }
        let target = path::join(&self.dir, &row.name);
        tracing::debug!(sftp = %self.name, path = %target, ?encoding, "读取远程文件以便编辑");
        self.reading = true;
        let job =
            termlet_core::runtime::spawn(async move { session.read_text(&target, encoding).await });
        self._read = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match job.await {
                Ok(result) => result,
                Err(e) => Err(SftpError::Unavailable(format!("任务异常结束：{e}"))),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.reading = false;
                match result {
                    Ok(file) => cx.emit(SftpPanelEvent::OpenEditor { lease, file }),
                    Err(SftpError::TooLarge { .. }) => this.offer_download(
                        t!("editor.too_large", name = row.name.clone()),
                        &row,
                        window,
                        cx,
                    ),
                    Err(SftpError::NotText { .. }) if encoding.is_none() => {
                        this.offer_other_encoding(&row, window, cx)
                    }
                    Err(error) => this.show_error(&error, cx),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 自动识别不了编码：可以下载，或选一种编码打开（GBK、Big5，没有 BOM、以汉字为主的 UTF-16 等）。
    fn offer_other_encoding(&mut self, row: &FileRow, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let row = row.clone();
        let paths = vec![path::join(&self.dir, &row.name)];
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (download, paths) = (this.clone(), paths.clone());
            let title = t!("editor.binary", name = row.name.clone());
            let (open, row) = (this.clone(), row.clone());
            let encodings = Button::new("sftp-open-encoding")
                .outline()
                .label(t!("editor.open_with_encoding"))
                .dropdown_caret(true)
                .dropdown_menu(move |menu, _, _| {
                    // UTF-8 刚试过
                    TextEncoding::ALL
                        .into_iter()
                        .filter(|e| *e != TextEncoding::Utf8)
                        .fold(menu, |menu, encoding| {
                            let (open, row) = (open.clone(), row.clone());
                            menu.item(PopupMenuItem::new(encoding.label()).on_click(
                                move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = open.update(cx, |this, cx| {
                                        this.edit_with(row.clone(), Some(encoding), window, cx)
                                    });
                                },
                            ))
                        })
                });
            alert
                .confirm()
                .title(title)
                .description(prose(t!("editor.binary_hint")))
                .on_ok(move |_, window, cx| {
                    let _ =
                        download.update(cx, |this, cx| this.download(paths.clone(), window, cx));
                    true
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .child(Button::new("sftp-open-cancel").label(t!("common.cancel"))),
                        )
                        .child(encodings)
                        .child(
                            DialogAction::new().child(
                                Button::new("sftp-open-download")
                                    .primary()
                                    .label(t!("sftp.download")),
                            ),
                        ),
                )
        });
    }

    /// 不能在线编辑：问要不要下载。
    fn offer_download(
        &mut self,
        title: SharedString,
        row: &FileRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let paths = vec![path::join(&self.dir, &row.name)];
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (this, paths) = (this.clone(), paths.clone());
            alert
                .confirm()
                .title(title.clone())
                .ok_text(t!("sftp.download"))
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.download(paths.clone(), window, cx));
                    true
                })
        });
    }

    // ------------------------------------------------------------ 新建、重命名、删除、权限

    pub(super) fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        // 提交时用打开对话框时的目录，不受期间列表切换的影响
        let dir = self.dir.clone();
        let this = cx.entity().downgrade();
        crate::sftp::prompt_name(
            t!("sftp.new_folder"),
            String::new(),
            "sftp-new-folder",
            Box::new(move |name, window, cx| {
                if invalid_name(&name) {
                    return gpui_kit::Task::ready(Err(t!("sftp.error.invalid_name")));
                }
                let (session, this) = (session.clone(), this.clone());
                let target = path::join(&dir, &name);
                tracing::info!(path = %target, "SFTP 新建文件夹");
                let job = termlet_core::runtime::spawn(async move { session.mkdir(&target).await });
                window.spawn(cx, async move |cx| {
                    let result = job.await.map_err(|e| SharedString::from(e.to_string()))?;
                    result.map_err(|e| errors::describe(&e))?;
                    let _ =
                        this.update_in(cx, |this, window, cx| this.refresh_to(name, window, cx));
                    Ok(())
                })
            }),
            window,
            cx,
        );
    }

    pub(super) fn rename(&mut self, row: FileRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        let dir = self.dir.clone();
        let this = cx.entity().downgrade();
        let from = row.name;
        crate::sftp::prompt_name(
            t!("sftp.rename"),
            from.clone(),
            "sftp-rename",
            Box::new(move |name, window, cx| {
                if invalid_name(&name) {
                    return gpui_kit::Task::ready(Err(t!("sftp.error.invalid_name")));
                }
                let (session, this) = (session.clone(), this.clone());
                let (old, new) = (path::join(&dir, &from), path::join(&dir, &name));
                let unchanged = name == from;
                if !unchanged {
                    tracing::info!(from = %old, to = %new, "SFTP 重命名");
                }
                let job = termlet_core::runtime::spawn(async move {
                    if unchanged {
                        return Ok(());
                    }
                    session.rename(&old, &new).await
                });
                window.spawn(cx, async move |cx| {
                    let result = job.await.map_err(|e| SharedString::from(e.to_string()))?;
                    result.map_err(|e| errors::describe(&e))?;
                    let _ =
                        this.update_in(cx, |this, window, cx| this.refresh_to(name, window, cx));
                    Ok(())
                })
            }),
            window,
            cx,
        );
    }

    pub(super) fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let names: Vec<String> = self.selected_rows(cx).into_iter().map(|r| r.name).collect();
        if names.is_empty() {
            return;
        }
        let paths: Vec<String> = names.iter().map(|n| path::join(&self.dir, n)).collect();
        let shown = join_names(&names);
        let this = cx.entity().downgrade();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (this, paths) = (this.clone(), paths.clone());
            alert
                .confirm()
                .title(t!("sftp.confirm_delete", names = shown.clone()))
                .description(prose(t!("sftp.confirm_delete_detail")))
                .ok_text(t!("common.delete"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.delete(paths.clone(), window, cx));
                    true
                })
        });
    }

    fn delete(&mut self, paths: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        tracing::info!(sftp = %self.name, count = paths.len(), first = ?paths.first(), "SFTP 删除");
        let job = termlet_core::runtime::spawn(async move {
            for p in &paths {
                session.remove(p).await?;
            }
            Ok::<(), SftpError>(())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Ok(Err(error)) = &result {
                    this.show_error(error, cx);
                }
                this.refresh(window, cx);
            });
        })
        .detach();
    }

    fn open_permissions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        let items: Vec<PermItem> = self
            .selected_rows(cx)
            .iter()
            .filter_map(|row| self.entry(&row.name))
            .map(|entry| PermItem {
                name: entry.name.clone(),
                path: path::join(&self.dir, &entry.name),
                perm: entry.perm,
                is_dir: entry.is_dir(),
                is_link: entry.is_link(),
            })
            .collect();
        let this = cx.entity().downgrade();
        perm_dialog::open(
            items,
            Box::new(move |plan, recursive, window, cx| {
                tracing::info!(groups = plan.len(), recursive, "SFTP 修改权限");
                let (session, this) = (session.clone(), this.clone());
                let job = termlet_core::runtime::spawn(async move {
                    let mut done = 0;
                    for (mode, paths) in plan {
                        let count = paths.len();
                        let request = ChmodRequest {
                            paths,
                            mode,
                            recursive,
                            scope: ChmodScope::All,
                        };
                        if let Err(error) = session.chmod(&request).await {
                            return Err((done, error));
                        }
                        done += count;
                    }
                    Ok(())
                });
                window.spawn(cx, async move |cx| {
                    let result = job.await.unwrap_or_else(|e| {
                        Err((0, SftpError::Unavailable(format!("任务异常结束：{e}"))))
                    });
                    // 成功或失败都刷新：列表显示现在的权限
                    let _ = this.update_in(cx, |this, window, cx| this.refresh(window, cx));
                    result.map_err(|(done, error)| ChmodFailure {
                        done,
                        error: errors::describe(&error),
                    })
                })
            }),
            window,
            cx,
        );
    }

    // ------------------------------------------------------------ 复制路径、粘贴上传

    pub(super) fn copy_paths(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected_paths(cx);
        if paths.is_empty() {
            return;
        }
        let text = paths.join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        let message = if paths.len() == 1 {
            t!("sftp.copied", text = text)
        } else {
            t!("sftp.paths_copied", count = paths.len())
        };
        window.push_notification(Notification::success(message), cx);
    }

    /// Ctrl+V：上传资源管理器中复制的文件；剪贴板中只有文字时什么也不做。
    pub(super) fn paste_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let paths: Vec<PathBuf> = item
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::ExternalPaths(paths) => Some(paths.paths().to_vec()),
                _ => None,
            })
            .flatten()
            .collect();
        if !paths.is_empty() {
            self.upload(paths, None, window, cx);
        }
    }

    // ------------------------------------------------------------ 上传、下载

    /// 上传到当前目录（`into` 为放在了哪个子目录上）。同名的先问是否覆盖：覆盖则整批覆盖，取消则跳过这些，其余照常上传。
    pub(super) fn upload(
        &mut self,
        paths: Vec<PathBuf>,
        into: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session() else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let in_current = into.is_none();
        let target = match &into {
            Some(name) => path::join(&self.dir, name),
            None => self.dir.clone(),
        };
        let known: Option<HashSet<String>> =
            in_current.then(|| self.entries.iter().map(|e| e.name.clone()).collect());
        let list_job = (!in_current).then(|| {
            let (session, target) = (session.clone(), target.clone());
            termlet_core::runtime::spawn(async move { session.list(&target).await })
        });
        cx.spawn_in(window, async move |this, cx| {
            let existing: HashSet<String> = match (known, list_job) {
                (Some(known), _) => known,
                (None, Some(job)) => match job.await {
                    Ok(Ok(listing)) => listing.entries.into_iter().map(|e| e.name).collect(),
                    Ok(Err(error)) => {
                        let _ = this.update(cx, |this, cx| this.show_error(&error, cx));
                        return;
                    }
                    Err(_) => return,
                },
                (None, None) => HashSet::new(),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let conflicts: Vec<String> = paths
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .filter(|name| existing.contains(name))
                    .collect();
                if conflicts.is_empty() {
                    this.start_upload(paths.clone(), target.clone(), false, cx);
                    return;
                }
                let weak = cx.entity().downgrade();
                let shown = join_names(&conflicts);
                crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
                    let (all, keep) = (weak.clone(), weak.clone());
                    let (paths_all, target_all) = (paths.clone(), target.clone());
                    let (paths_rest, target_rest) = (paths.clone(), target.clone());
                    let conflicts = conflicts.clone();
                    alert
                        .confirm()
                        .title(t!("sftp.confirm_overwrite_title"))
                        .description(prose(t!("sftp.confirm_overwrite", names = shown.clone())))
                        .ok_text(t!("sftp.overwrite"))
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text(t!("sftp.skip_existing"))
                        .on_ok(move |_, _, cx| {
                            let _ = all.update(cx, |this, cx| {
                                this.start_upload(paths_all.clone(), target_all.clone(), true, cx)
                            });
                            true
                        })
                        .on_cancel(move |_, _, cx| {
                            let rest: Vec<PathBuf> = paths_rest
                                .iter()
                                .filter(|p| {
                                    p.file_name().is_none_or(|n| {
                                        !conflicts.contains(&n.to_string_lossy().to_string())
                                    })
                                })
                                .cloned()
                                .collect();
                            if !rest.is_empty() {
                                let _ = keep.update(cx, |this, cx| {
                                    this.start_upload(rest, target_rest.clone(), false, cx)
                                });
                            }
                            true
                        })
                });
            });
        })
        .detach();
    }

    fn start_upload(
        &mut self,
        local_paths: Vec<PathBuf>,
        remote_dir: String,
        overwrite: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session() else {
            return;
        };
        let manager = self.manager.clone();
        tracing::info!(
            sftp = %self.name,
            count = local_paths.len(),
            remote_dir = %remote_dir,
            overwrite,
            "SFTP 上传"
        );
        let request = UploadRequest {
            local_paths,
            remote_dir,
            overwrite,
        };
        let job =
            termlet_core::runtime::spawn(async move { manager.upload(&session, request).await });
        cx.spawn(async move |this, cx| {
            if let Ok(Err(error)) = job.await {
                let _ = this.update(cx, |this, cx| this.show_error(&error, cx));
            }
        })
        .detach();
    }

    /// 下载所选的项：显示本机栏时下载到它的当前目录，否则选择一个目录。
    fn download_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected_paths(cx);
        self.download(paths, window, cx);
    }

    fn download(&mut self, paths: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        if self.shows_local(window, cx)
            && let Some(dir) = self.local.read(cx).dir().map(|d| d.to_path_buf())
        {
            self.download_to(paths, dir, cx);
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("sftp.download_to")),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(dirs))) = picked.await else {
                return;
            };
            let Some(dir) = dirs.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                AppState::ui(cx).update(cx, |ui, cx| {
                    ui.update(|s| s.sftp_download_dir = Some(dir.clone()), cx)
                });
                this.download_to(paths, dir, cx);
            });
        })
        .detach();
    }

    /// 下载到本机目录（本机已有同名项时核心自动改名，不需要确认）。
    pub(super) fn download_to(
        &mut self,
        remote_paths: Vec<String>,
        local_dir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session() else {
            return;
        };
        let manager = self.manager.clone();
        tracing::info!(
            sftp = %self.name,
            count = remote_paths.len(),
            local_dir = %local_dir.display(),
            "SFTP 下载"
        );
        let request = DownloadRequest {
            remote_paths,
            local_dir,
        };
        let job =
            termlet_core::runtime::spawn(async move { manager.download(&session, request).await });
        cx.spawn(async move |this, cx| {
            if let Ok(Err(error)) = job.await {
                let _ = this.update(cx, |this, cx| this.show_error(&error, cx));
            }
        })
        .detach();
    }

    fn pick_upload(&mut self, directories: bool, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: !directories,
            directories,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| this.upload(paths, None, window, cx));
        })
        .detach();
    }

    // ------------------------------------------------------------ 菜单与工具栏

    pub(super) fn build_menu(
        &self,
        menu: PopupMenu,
        panel: &Entity<Self>,
        window: &mut Window,
        cx: &App,
    ) -> PopupMenu {
        let selected = self.selected_rows(cx);
        let weak = panel.downgrade();
        let act = |f: fn(&mut SftpPanel, &mut Window, &mut Context<SftpPanel>)| {
            let weak = weak.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = weak.update(cx, |this, cx| f(this, window, cx));
            }
        };
        let mut menu = menu;
        if !selected.is_empty() {
            if let [row] = selected.as_slice()
                && !row.is_dir
            {
                let row = row.clone();
                let weak = weak.clone();
                menu = menu.item(
                    PopupMenuItem::new(t!("editor.edit"))
                        .icon(IconName::FilePen)
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| this.edit(row.clone(), window, cx));
                        }),
                );
            }
            menu = menu.item(
                PopupMenuItem::new(self.download_label(window, cx))
                    .on_click(act(|this, window, cx| this.download_selected(window, cx))),
            );
            if let [row] = selected.as_slice() {
                let rename_row = row.clone();
                let weak_rename = weak.clone();
                let full = path::join(&self.dir, &row.name);
                menu = menu
                    .item(
                        PopupMenuItem::new(t!("sftp.rename")).on_click(move |_, window, cx| {
                            let _ = weak_rename
                                .update(cx, |this, cx| this.rename(rename_row.clone(), window, cx));
                        }),
                    )
                    .item(PopupMenuItem::new(t!("sftp.copy_path")).on_click(
                        move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(full.clone()));
                            window.push_notification(
                                Notification::success(t!("sftp.copied", text = full.clone())),
                                cx,
                            );
                        },
                    ));
            }
            // SCP 模式不能修改权限：置灰并在名称中说明
            let scp = self.is_scp();
            menu = menu
                .item(
                    PopupMenuItem::new(if scp {
                        t!("sftp.permissions_scp")
                    } else {
                        t!("sftp.permissions")
                    })
                    .icon(IconName::ShieldCheck)
                    .disabled(scp)
                    .on_click(act(|this, window, cx| this.open_permissions(window, cx))),
                )
                .item(
                    PopupMenuItem::new(t!("sftp.delete"))
                        .on_click(act(|this, window, cx| this.confirm_delete(window, cx))),
                )
                .separator();
        }
        let menu = menu
            .item(
                PopupMenuItem::new(t!("sftp.upload_files"))
                    .on_click(act(|this, window, cx| this.pick_upload(false, window, cx))),
            )
            .item(
                PopupMenuItem::new(t!("sftp.upload_folder"))
                    .on_click(act(|this, window, cx| this.pick_upload(true, window, cx))),
            )
            .item(
                PopupMenuItem::new(t!("sftp.new_folder_menu"))
                    .on_click(act(|this, window, cx| this.new_folder(window, cx))),
            )
            .item(
                PopupMenuItem::new(t!("sftp.refresh"))
                    .on_click(act(|this, window, cx| this.refresh(window, cx))),
            );
        // 在 VS Code、Zed 中打开：选中一个目录时打开它，没有选中时打开当前目录（装了的、设置允许的）
        let editor_dir = match selected.as_slice() {
            [] if !self.dir.is_empty() => Some(self.dir.clone()),
            [row] if row.is_dir => Some(path::join(&self.dir, &row.name)),
            _ => None,
        };
        match editor_dir {
            Some(dir)
                if !crate::sftp::open_in_editor::editors_for(&self.target, &self.store, cx)
                    .is_empty() =>
            {
                crate::sftp::open_in_editor::menu_items(
                    menu.separator(),
                    &self.target,
                    dir,
                    &self.store,
                    cx,
                )
            }
            _ => menu,
        }
    }

    /// 显示本机栏且它的目录已知时，下载到那里。
    fn download_label(&self, window: &Window, cx: &App) -> SharedString {
        if self.shows_local(window, cx) && self.local.read(cx).dir().is_some() {
            t!("sftp.download_here")
        } else {
            t!("sftp.download")
        }
    }

    pub(super) fn render_toolbar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ready = self.ready();
        let selected = self.selected_rows(cx);
        let compact = self.remote_width.get() > gpui_kit::px(0.)
            && self.remote_width.get() < rems(COMPACT_TOOLBAR).to_pixels(window.rem_size());
        let show_local = self.shows_local(window, cx);
        let at_root = self.dir == "/" || self.dir.is_empty();
        let single_file = matches!(selected.as_slice(), [row] if !row.is_dir);
        let download_label = self.download_label(window, cx);
        let follow = self.link.as_ref().map(|l| (l.follow, l.cwd.is_some()));
        let mono = cx.theme().mono_font_family.clone();
        let icon = |id: &'static str, icon: IconName, tip: SharedString| {
            Button::new(id).ghost().small().icon(icon).tooltip(tip)
        };
        h_flex()
            .flex_none()
            .gap_0p5()
            .px_1()
            .py_1()
            .overflow_hidden()
            .child(
                icon(
                    "sftp-toggle-local",
                    IconName::PanelLeft,
                    if show_local {
                        t!("sftp.hide_local")
                    } else {
                        t!("sftp.show_local")
                    },
                )
                .selected(show_local)
                .on_click(cx.listener(|this, _, window, cx| this.toggle_local(window, cx))),
            )
            .child(
                icon("sftp-up", IconName::FolderUp, t!("sftp.up"))
                    .disabled(!ready || at_root)
                    .on_click(cx.listener(|this, _, window, cx| this.go_up(window, cx))),
            )
            .child(
                icon("sftp-home", IconName::House, t!("sftp.home"))
                    .disabled(!ready)
                    .on_click(cx.listener(|this, _, window, cx| this.go_home(window, cx))),
            )
            .child(
                icon("sftp-refresh", IconName::RefreshCw, t!("sftp.refresh"))
                    .loading(self.loading || self.reading)
                    .disabled(!ready)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let dir = this.dir.clone();
                        this.load(dir, true, window, cx)
                    })),
            )
            .child(file_list::hidden_files_button(
                "sftp-hidden",
                DragSource::Remote(cx.entity_id()),
                cx,
            ))
            .when_some(follow, |this, (on, known)| {
                let tip = match (on, known) {
                    (false, _) => t!("sftp.follow"),
                    (true, false) => t!("sftp.follow_unknown"),
                    (true, true) => t!("sftp.following"),
                };
                this.child(
                    icon(
                        "sftp-follow",
                        if on {
                            IconName::LocateFixed
                        } else {
                            IconName::LocateOff
                        },
                        tip,
                    )
                    .selected(on)
                    .disabled(!ready)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_follow(window, cx))),
                )
            })
            .when(self.target.is_root(), |this| {
                this.child(super::root_badge("sftp-root-badge", false))
            })
            .when(ready && self.is_scp(), |this| {
                this.child(super::scp_badge("sftp-scp-badge"))
            })
            .child(
                div()
                    .flex_1()
                    .min_w(rems(4.))
                    .key_context(file_list::PATH_CONTEXT)
                    .on_action(cx.listener(|this, _: &file_list::FocusList, window, cx| {
                        this.path_to_list(window, cx)
                    }))
                    .child(
                        Input::new(&self.path)
                            .small()
                            .id("sftp-path")
                            .disabled(!ready)
                            .font_family(mono),
                    ),
            )
            .child(
                icon(
                    "sftp-new-folder-button",
                    IconName::FolderPlus,
                    t!("sftp.new_folder"),
                )
                .disabled(!ready)
                .on_click(cx.listener(|this, _, window, cx| this.new_folder(window, cx))),
            )
            .child(
                icon(
                    "sftp-upload-files",
                    IconName::Upload,
                    t!("sftp.upload_files"),
                )
                .disabled(!ready)
                .on_click(cx.listener(|this, _, window, cx| this.pick_upload(false, window, cx))),
            )
            .when(!compact, |this| {
                this.child(
                    icon(
                        "sftp-upload-folder",
                        IconName::FolderInput,
                        t!("sftp.upload_folder"),
                    )
                    .disabled(!ready)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.pick_upload(true, window, cx)),
                    ),
                )
                .child(
                    icon("sftp-edit", IconName::FilePen, t!("editor.edit_hint"))
                        .disabled(!ready || !single_file)
                        .on_click(cx.listener(|this, _, window, cx| {
                            if let [row] = this.selected_rows(cx).as_slice() {
                                let row = row.clone();
                                this.edit(row, window, cx);
                            }
                        })),
                )
            })
            .child(
                icon("sftp-download", IconName::Download, download_label)
                    .disabled(!ready || selected.is_empty())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.download_selected(window, cx)),
                    ),
            )
            .child(
                icon("sftp-rename", IconName::Pencil, t!("sftp.rename"))
                    .disabled(!ready || selected.len() != 1)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(row) = this.selected_rows(cx).into_iter().next() {
                            this.rename(row, window, cx);
                        }
                    })),
            )
            .child(
                icon("sftp-delete", IconName::Trash, t!("sftp.delete"))
                    .disabled(!ready || selected.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_delete(window, cx))),
            )
            .into_any_element()
    }

    /// 工具栏下方的提示条：正在连接、连接失败、正在重连、已断开、操作出错。
    pub(super) fn render_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let bar = |bg: gpui_kit::Hsla, fg: gpui_kit::Hsla| {
            h_flex()
                .flex_none()
                .px_3()
                .gap_2()
                .min_h(rems(1.5))
                .text_xs()
                .bg(bg)
                .text_color(fg)
        };
        let reconnect = || {
            Button::new("sftp-reconnect")
                .link()
                .xsmall()
                .label(t!("sftp.reconnect"))
                .on_click(cx.listener(|this, _, window, cx| this.reconnect(window, cx)))
        };
        let (danger, warning, muted) = (theme.danger, theme.warning, theme.muted_foreground);
        let element = match &self.status {
            Status::Connecting => bar(theme.muted, muted)
                .child(prose(t!("terminal.connecting", name = self.name.clone())))
                .into_any_element(),
            Status::Failed(message) => bar(danger.opacity(0.15), danger)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(prose(t!("sftp.open_failed", error = message.clone()))),
                )
                .child(reconnect())
                .into_any_element(),
            Status::Reconnecting => bar(warning.opacity(0.15), warning)
                .child(prose(t!("sftp.reconnecting")))
                .into_any_element(),
            Status::Lost => bar(danger.opacity(0.15), danger)
                .child(div().flex_1().child(t!("sftp.lost")))
                .child(reconnect())
                .into_any_element(),
            Status::Ready if self.error.is_none() => {
                // SCP 模式：说明受限之处，可以关掉
                if !self.is_scp() || self.scp_notice_closed {
                    return None;
                }
                bar(theme.muted, muted)
                    .id("sftp-scp-notice")
                    .test_support()
                    .child(
                        gpui_kit::component::Icon::new(IconName::Info)
                            .xsmall()
                            .text_color(muted),
                    )
                    .child(div().flex_1().min_w_0().child(prose(t!("sftp.scp_notice"))))
                    .child(
                        Button::new("sftp-scp-notice-close")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip(t!("common.close"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.scp_notice_closed = true;
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }
            Status::Ready => {
                let error = self.error.clone()?;
                bar(danger.opacity(0.15), danger)
                    .child(div().flex_1().min_w_0().truncate().child(prose(error)))
                    .child(
                        Button::new("sftp-error-close")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip(t!("common.close"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.error = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }
        };
        Some(element)
    }
}
