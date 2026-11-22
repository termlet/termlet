//! 远程文本文件的编辑器 Tab：工具栏（保存、重新加载、查找与替换、远程路径）、提示条、编辑区、状态栏。
//!
//! - 内容由 SFTP 面板读好后交来（[`TextFile`]），编辑器与 SFTP 面板共用会话（[`SessionLease`]）：SFTP 的 Tab
//!   先关掉时会话仍为编辑器保留。
//! - 保存时提交打开（或上次保存）时的修改时间与大小；远程文件在此期间被别人改过（或删除）时提示冲突，可以仍然覆盖、
//!   放弃修改重新加载或暂不保存。保存成功后把**保存时刻**的内容记为已保存，保存期间继续输入的仍算未保存。
//! - 换行符见 [`super::text`]：CRLF 文件按 LF 编辑、保存时还原；混用的只把 `\n` 当换行。文件开头的 BOM 不放进编辑区，
//!   保存时加回；状态栏的编码后面写明 BOM。
//! - 会话断开时修改保留：核心自动重连期间不能保存；重连放弃后经 SFTP 面板重新连接，换上它的新会话继续编辑、保存。
//!   SFTP 面板也关了时提示重新打开它（工作区经 [`EditorPanel::set_sftp_panel`] 交给编辑器后自动连上）。
//! - 不暂存未保存修改的草稿：界面不会像网页那样重新载入，编辑中的内容不会因此丢失。

mod chrome;

use std::rc::Rc;

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{self, Editor, EditorState, InputEvent, Rope};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Pixels, Render,
    SharedString, Styled as _, Subscription, Task, WeakEntity, Window, actions, div, px,
};
use termlet_core::sftp::{
    SessionState, SftpError, TextEncoding, TextFile, WriteResult, WriteTextRequest,
};
use termlet_core::store::StoreEvent;

use super::language::{self, Language};
use super::text::{self, Eol};
use crate::app_state::AppState;
use crate::prose::prose;
use crate::sftp::errors;
use crate::sftp::list_model::visible_name;
use crate::sftp::panel::{SessionLease, SftpPanel};
use crate::t;

/// 面板在持久化布局中的名称，发布后不能改。
const PANEL_NAME: &str = "EditorPanel";
const KEY_CONTEXT: &str = "EditorPanel";

actions!(
    editor_panel,
    [
        /// 保存（有未保存的修改时）
        Save,
        /// 打开查找与替换（焦点不在编辑区时；编辑区内由编辑器自己处理 Ctrl+F）
        Find,
        /// 关闭这个 Tab（有未保存的修改时先确认）
        CloseTab,
    ]
);

/// 注册编辑器固定的快捷键：在整个编辑器 Tab 内有效（工具栏按钮、查找栏有焦点时也能保存）。按当前系统的习惯。
pub fn init(cx: &mut App) {
    cx.bind_keys(key_bindings(cx.platform_info().os));
}

/// 编辑器固定的快捷键：`os` 为按哪个系统的习惯（`KeyBinding::new` 不认 `secondary-`，会 panic，分开写）。
pub(crate) fn key_bindings(os: gpui_kit::OperatingSystem) -> Vec<KeyBinding> {
    let context = Some(KEY_CONTEXT);
    if os == gpui_kit::OperatingSystem::Mac {
        vec![
            KeyBinding::new("cmd-s", Save, context),
            KeyBinding::new("cmd-f", Find, context),
            KeyBinding::new("cmd-w", CloseTab, context),
        ]
    } else {
        vec![
            KeyBinding::new("ctrl-s", Save, context),
            KeyBinding::new("ctrl-f", Find, context),
            KeyBinding::new("ctrl-shift-w", CloseTab, context),
        ]
    }
}

/// 编辑器发给工作区的请求。
pub enum EditorPanelEvent {
    /// 关闭这个 Tab（有未保存的修改时已确认过）
    Close,
}

/// 远程文件的元信息：保存时提交修改时间与大小，用于发现冲突。
#[derive(Clone, Debug)]
struct Meta {
    size: u64,
    /// Unix 秒
    modified: i64,
    /// `ls -l` 的写法
    mode: SharedString,
}

impl Meta {
    fn of_file(file: &TextFile) -> Self {
        Self {
            size: file.size,
            modified: file.modified,
            mode: file.mode.clone().into(),
        }
    }

    fn of_write(result: &WriteResult) -> Self {
        Self {
            size: result.size,
            modified: result.modified,
            mode: result.mode.clone().into(),
        }
    }
}

/// 会话的连接状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Ready,
    /// 连接断开，核心正在自动重连（会话不变）
    Reconnecting,
    /// 已断开（重连放弃、会话已关闭）：可以经 SFTP 面板重新连接
    Lost,
    /// 已请 SFTP 面板重新连接，等它的新会话
    Waiting,
}

/// 工具栏下方的提示条（连接状态之外的）。
#[derive(Clone, Debug)]
enum Banner {
    /// 保存时发现远程文件在打开后被别人修改（`deleted` 为已被删除）
    Conflict { deleted: bool },
    /// 重新读取（重新加载、换编码重新打开）失败，可以重试
    LoadFailed {
        message: SharedString,
        encoding: TextEncoding,
    },
    /// 其他错误
    Error(SharedString),
}

/// 编辑区的字体：与终端相同的字体与字号（行高由编辑器按字号的 1.5 倍排）。
#[derive(Clone, Debug, PartialEq)]
struct EditorFont {
    family: SharedString,
    /// 设置中的字号是像素值（与终端一致），不随界面缩放
    size: Pixels,
}

impl EditorFont {
    fn from_settings(cx: &App) -> Self {
        let terminal = AppState::store(cx).settings().terminal;
        let family = terminal.font_family.trim();
        Self {
            family: if family.is_empty() {
                termlet_terminal::fonts::DEFAULT_FAMILY.into()
            } else {
                family.to_string().into()
            },
            size: px(f32::from(terminal.font_size.clamp(8, 40))),
        }
    }
}

pub struct EditorPanel {
    /// SFTP 面板上的主机名（提示重新打开 SFTP 时用）
    host_name: SharedString,
    /// 远程文件的绝对路径
    path: String,
    /// Tab 上的名称（看不见的字符写成 `<U+XXXX>`）
    file_name: SharedString,
    lease: Rc<SessionLease>,
    sftp: WeakEntity<SftpPanel>,
    editor: Entity<EditorState>,
    language: Option<Language>,
    eol: Eol,
    encoding: TextEncoding,
    /// 文件以 BOM 开头：编辑区中不显示，保存时加回
    bom: bool,
    meta: Meta,
    /// 上次保存（或打开、重新加载）时编辑器中的内容；与当前内容不同即为有未保存的修改
    saved: Rope,
    dirty: bool,
    saving: bool,
    /// 正在重新读取远程文件
    loading: bool,
    /// 保存后还没有再修改（状态栏显示“已保存”）
    saved_once: bool,
    link: Link,
    banner: Option<Banner>,
    font: EditorFont,
    _save: Option<Task<()>>,
    _load: Option<Task<()>>,
    _session_state: Task<()>,
    _sftp_watch: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
    /// Tab 移到别的窗口后：编辑区获得焦点时开始闪烁光标（编辑区自己的处理登记在打开它的窗口上）
    _window_focus: Option<Subscription>,
}

impl EventEmitter<PanelEvent> for EditorPanel {}
impl EventEmitter<EditorPanelEvent> for EditorPanel {}

impl Focusable for EditorPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

/// 编辑器的语法名：识别不了、没有语法的按纯文本。
fn grammar_of(language: Option<Language>) -> &'static str {
    language.and_then(Language::grammar).unwrap_or("text")
}

/// 会话不能用了（断开或已关闭）。
fn is_lost(error: &SftpError) -> bool {
    matches!(error, SftpError::Disconnected | SftpError::SessionClosed)
}

fn task_failed(error: impl std::fmt::Display) -> SftpError {
    SftpError::Unavailable(format!("任务异常结束：{error}"))
}

impl EditorPanel {
    /// 打开编辑器。`file` 是 SFTP 面板读好的内容，`sftp` 是那个面板（会话断开后经它重新连接）。
    pub fn open(
        lease: Rc<SessionLease>,
        sftp: WeakEntity<SftpPanel>,
        host_name: SharedString,
        file: TextFile,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx: &mut Context<Self>| {
            let (bom, body) = text::split_bom(&file.content);
            let eol = text::detect_eol(body);
            let language = language::language_for(&file.path, language::first_line_of(body));
            let content = text::to_editor(body, eol).into_owned();
            let editor = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language(grammar_of(language))
                    .soft_wrap(false)
                    .default_value(content)
            });
            let saved = editor.read(cx).text().clone();
            let store = AppState::global(cx).store.clone();
            let subscriptions = vec![
                cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.update_dirty(cx);
                    }
                }),
                // 终端的字体、字号改了：编辑区跟着变
                cx.subscribe(&store, |this, _, event: &StoreEvent, cx| {
                    if matches!(event, StoreEvent::SettingsChanged) {
                        let font = EditorFont::from_settings(cx);
                        if font != this.font {
                            this.font = font;
                            cx.notify();
                        }
                    }
                }),
            ];
            let mut this = Self {
                host_name,
                file_name: visible_name(&termlet_core::sftp::path::base_name(&file.path)).into(),
                path: file.path.clone(),
                lease,
                sftp,
                editor,
                language,
                eol,
                encoding: file.encoding,
                bom,
                meta: Meta::of_file(&file),
                saved,
                dirty: false,
                saving: false,
                loading: false,
                saved_once: false,
                link: Link::Ready,
                banner: None,
                font: EditorFont::from_settings(cx),
                _save: None,
                _load: None,
                _session_state: Task::ready(()),
                _sftp_watch: Vec::new(),
                _subscriptions: subscriptions,
                _window_focus: None,
            };
            this.watch_session(cx);
            this.watch_sftp(cx);
            this
        })
    }

    /// 远程文件的绝对路径：同一主机同一路径已打开时，工作区切过去而不是新开。
    pub fn remote_path(&self) -> &str {
        &self.path
    }

    /// 主机的标识（与 SFTP 面板的 `SftpTarget::key` 相同）。
    pub fn host_key(&self) -> String {
        self.lease.session().host().to_string()
    }

    /// 用着的 SFTP 会话（Tab 移到别的窗口后会话跟着走，见 `workspace/move_tab.rs`）。
    pub(crate) fn session_id(&self) -> termlet_core::sftp::SessionId {
        self.lease.session().id().clone()
    }

    /// 有未保存的修改（关闭窗口前统计）。
    pub fn is_dirty(&self, _: &App) -> bool {
        self.dirty
    }

    /// 打开它的 SFTP 面板还在。
    pub fn has_sftp_panel(&self) -> bool {
        self.sftp.upgrade().is_some()
    }

    /// 换一个 SFTP 面板（原来的已关闭、工作区为这台主机重新打开了 SFTP 时）：会话已断开的话等它连上后换上它的会话。
    /// 不是同一台主机的面板忽略。
    pub fn set_sftp_panel(&mut self, sftp: &Entity<SftpPanel>, cx: &mut Context<Self>) {
        if sftp.read(cx).target().key() != self.host_key() {
            return;
        }
        self.sftp = sftp.downgrade();
        self.watch_sftp(cx);
        if matches!(self.link, Link::Lost | Link::Waiting) {
            self.adopt_current(cx);
        }
        cx.notify();
    }

    /// 不再确认、直接关闭（一次关闭几个 Tab 时已经汇总确认过，见 [`crate::workspace::Workspace::close_tabs`]）。
    pub(crate) fn close_tab(&mut self, cx: &mut Context<Self>) {
        cx.emit(EditorPanelEvent::Close);
    }

    /// 关闭这个 Tab：有未保存的修改时先确认（不受“关闭前确认”设置影响）。
    pub fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dirty {
            cx.emit(EditorPanelEvent::Close);
            return;
        }
        let this = cx.entity().downgrade();
        let title = t!("editor.confirm_close", name = self.file_name.clone());
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let this = this.clone();
            alert
                .confirm()
                .title(title.clone())
                .description(prose(t!("editor.confirm_close_detail")))
                .ok_text(t!("editor.discard_and_close"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, _, cx| {
                    let _ = this.update(cx, |_, cx| cx.emit(EditorPanelEvent::Close));
                    true
                })
        });
    }

    // ------------------------------------------------------------ 移到别的窗口

    /// Tab 移到别的窗口（见 `workspace/move_tab.rs`）：编辑区连同未保存的修改、撤销记录照用，会话照用（它的所属窗口
    /// 由工作区跟着改）；编辑区自己在打开它的窗口中登记的获得焦点的处理（开始闪烁光标）在新窗口中补上。
    pub(crate) fn rebind_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.clone();
        let focus = editor.focus_handle(cx);
        self._window_focus = Some(cx.on_focus(&focus, window, move |_, window, cx| {
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        }));
        cx.notify();
    }

    /// 编辑区中现在的内容（测试用）。
    #[cfg(test)]
    pub(crate) fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    // ------------------------------------------------------------ 状态

    /// 可以读写远程文件：会话连接着、不在重新读取中。
    fn is_ready(&self) -> bool {
        self.link == Link::Ready && !self.loading
    }

    fn can_save(&self) -> bool {
        self.is_ready() && self.dirty && !self.saving
    }

    fn update_dirty(&mut self, cx: &mut Context<Self>) {
        let dirty = *self.editor.read(cx).text() != self.saved;
        let saved_once = self.saved_once && !dirty;
        if dirty != self.dirty || saved_once != self.saved_once {
            self.dirty = dirty;
            self.saved_once = saved_once;
            cx.notify();
        }
    }

    // ------------------------------------------------------------ 会话

    /// 跟着会话的连接状态（换了会话后重新开始）。
    fn watch_session(&mut self, cx: &mut Context<Self>) {
        let session = self.lease.session().clone();
        self.link = match session.state() {
            SessionState::Connected => Link::Ready,
            SessionState::Reconnecting { .. } => Link::Reconnecting,
            SessionState::Closed { .. } => Link::Lost,
        };
        let mut state = session.watch_state();
        self._session_state = cx.spawn(async move |this, cx| {
            while state.changed().await.is_ok() {
                let state = state.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| this.on_session_state(state, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    fn on_session_state(&mut self, state: SessionState, cx: &mut Context<Self>) {
        self.link = match (state, self.link) {
            (SessionState::Connected, _) => Link::Ready,
            // 已在等 SFTP 面板的新会话：旧会话的变化不再要紧
            (_, Link::Waiting) => Link::Waiting,
            (SessionState::Reconnecting { .. }, _) => Link::Reconnecting,
            (SessionState::Closed { .. }, _) => Link::Lost,
        };
        if self.link == Link::Lost {
            // SFTP 面板可能已经自己连上了新的会话
            self.adopt_current(cx);
        }
        cx.notify();
    }

    /// 观察 SFTP 面板：会话断开后它连上了（新的会话），就换上它的。
    fn watch_sftp(&mut self, cx: &mut Context<Self>) {
        self._sftp_watch.clear();
        let Some(sftp) = self.sftp.upgrade() else {
            return;
        };
        self._sftp_watch = vec![
            cx.observe(&sftp, |this, sftp, cx| {
                if matches!(this.link, Link::Lost | Link::Waiting) && !this.adopt_current(cx) {
                    // 面板的重新连接也失败了：回到“已断开”，可以再试
                    if matches!(this.link, Link::Waiting) && !sftp.read(cx).is_connecting() {
                        this.link = Link::Lost;
                        cx.notify();
                    }
                }
            }),
            // 面板关了：提示条改为提示重新打开 SFTP
            cx.observe_release(&sftp, |_, _, cx| cx.notify()),
        ];
    }

    /// 换上 SFTP 面板现在的会话（同一台主机、还能用时）。
    fn adopt_current(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(sftp) = self.sftp.upgrade() else {
            return false;
        };
        let Some(lease) = sftp.read(cx).current_lease() else {
            return false;
        };
        let session = lease.session();
        if session.host() != self.lease.session().host()
            || matches!(session.state(), SessionState::Closed { .. })
        {
            return false;
        }
        if !Rc::ptr_eq(&lease, &self.lease) {
            self.lease = lease;
            self.watch_session(cx);
        }
        self.link = Link::Ready;
        cx.notify();
        true
    }

    /// 读写时发现会话不能用了：核心在自动重连时等它，否则显示“已断开”。
    fn on_lost(&mut self, cx: &mut Context<Self>) {
        tracing::info!(path = %self.path, "编辑器的 SFTP 会话断开");
        self.link = match self.lease.session().state() {
            SessionState::Reconnecting { .. } => Link::Reconnecting,
            _ => Link::Lost,
        };
        cx.notify();
    }

    /// 重新连接：SFTP 面板已有能用的会话时直接换上，否则请它重新连接，连上后换上它的新会话。
    fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.adopt_current(cx) {
            return;
        }
        let Some(sftp) = self.sftp.upgrade() else {
            cx.notify();
            return;
        };
        self.link = Link::Waiting;
        sftp.update(cx, |panel, cx| panel.ensure_connected(window, cx));
        cx.notify();
    }

    // ------------------------------------------------------------ 保存

    /// 保存。`force` 为不检查远程文件有没有被别人改过（冲突时“仍然覆盖”）。
    fn save(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_ready() || self.saving || (!self.dirty && !force) {
            return;
        }
        let snapshot = self.editor.read(cx).text().clone();
        let mut content = text::to_disk(&snapshot.to_string(), self.eol).into_owned();
        if self.bom {
            content.insert(0, '\u{feff}');
        }
        let request = WriteTextRequest {
            path: self.path.clone(),
            content,
            encoding: Some(self.encoding),
            expected_modified: Some(self.meta.modified),
            expected_size: Some(self.meta.size),
            force,
        };
        let session = self.lease.session().clone();
        self.saving = true;
        self.banner = None;
        tracing::debug!(path = %self.path, force, bytes = request.content.len(), "保存远程文件");
        let job = termlet_core::runtime::spawn(async move { session.write_text(&request).await });
        self._save = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(task_failed(e)));
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(written) => {
                        tracing::info!(path = %this.path, "已保存远程文件");
                        this.meta = Meta::of_write(&written);
                        this.saved = snapshot;
                        this.saved_once = true;
                        this.update_dirty(cx);
                    }
                    Err(SftpError::Changed { deleted, .. }) => {
                        tracing::info!(path = %this.path, deleted, "保存时发现远程文件被别处修改过");
                        this.banner = Some(Banner::Conflict { deleted })
                    }
                    Err(error) if is_lost(&error) => this.on_lost(cx),
                    Err(error) => {
                        tracing::warn!(path = %this.path, "保存远程文件失败：{error}");
                        this.banner = Some(Banner::Error(errors::describe(&error)));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------ 重新读取

    /// 重新加载（有未保存的修改时先确认）。
    fn confirm_reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_ready() || self.saving {
            return;
        }
        let encoding = self.encoding;
        if !self.dirty {
            self.reload(encoding, window, cx);
            return;
        }
        self.confirm_discard(
            t!("editor.confirm_reload"),
            t!("editor.discard_and_reload"),
            encoding,
            window,
            cx,
        );
    }

    /// 换一种编码重新打开（有未保存的修改时先确认）。
    fn reopen_with(&mut self, encoding: TextEncoding, window: &mut Window, cx: &mut Context<Self>) {
        if encoding == self.encoding || !self.is_ready() || self.saving {
            return;
        }
        if !self.dirty {
            self.reload(encoding, window, cx);
            return;
        }
        self.confirm_discard(
            t!("editor.confirm_reopen", name = encoding.label()),
            t!("editor.discard_and_reopen"),
            encoding,
            window,
            cx,
        );
    }

    /// 确认放弃未保存的修改后按 `encoding` 重新读取。
    fn confirm_discard(
        &mut self,
        title: SharedString,
        ok: SharedString,
        encoding: TextEncoding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let this = this.clone();
            alert
                .confirm()
                .title(title.clone())
                .ok_text(ok.clone())
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.reload(encoding, window, cx));
                    true
                })
        });
    }

    /// 重新读取远程文件，替换编辑器中的内容（放弃未保存的修改、清空撤销历史）。读取期间编辑区只读。
    fn reload(&mut self, encoding: TextEncoding, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.lease.session().clone();
        let path = self.path.clone();
        self.loading = true;
        self.banner = None;
        tracing::debug!(path = %path, ?encoding, "重新读取远程文件");
        let job =
            termlet_core::runtime::spawn(
                async move { session.read_text(&path, Some(encoding)).await },
            );
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|e| Err(task_failed(e)));
            let _ = this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Ok(file) => this.apply_file(file, window, cx),
                    Err(error) if is_lost(&error) => this.on_lost(cx),
                    Err(error) => {
                        tracing::warn!(path = %this.path, "重新读取远程文件失败：{error}");
                        this.banner = Some(Banner::LoadFailed {
                            message: errors::describe(&error),
                            encoding,
                        })
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn apply_file(&mut self, file: TextFile, window: &mut Window, cx: &mut Context<Self>) {
        let (bom, body) = text::split_bom(&file.content);
        let eol = text::detect_eol(body);
        let language = language::language_for(&self.path, language::first_line_of(body));
        let content = text::to_editor(body, eol).into_owned();
        let relanguage = language != self.language;
        self.editor.update(cx, |editor, cx| {
            if relanguage {
                editor.set_highlighter(grammar_of(language), cx);
            }
            editor.set_value(content, window, cx);
        });
        self.saved = self.editor.read(cx).text().clone();
        self.language = language;
        self.eol = eol;
        self.encoding = file.encoding;
        self.bom = bom;
        self.meta = Meta::of_file(&file);
        self.dirty = false;
        self.saved_once = false;
        self.banner = None;
    }

    // ------------------------------------------------------------ 动作

    /// 打开编辑器自带的查找栏（在编辑区顶部；栏中可以展开替换）。
    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.focus(window, cx);
            editor.open_search(false, cx);
        });
    }

    fn on_save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        self.save(false, window, cx);
    }

    fn on_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.open_find(window, cx);
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.request_close(window, cx);
    }
}

/// 编辑区：终端的字体与字号，铺满所在区域。LF、CRLF 文件粘贴进来的 `\r\n`、`\r` 换成 `\n`（编辑器只按 `\n` 分行，
/// 否则 `\r` 会作为字符留在行尾）。右键菜单只放编辑命令（代码编辑器默认的“转到定义”等要有语言服务器，这里用不上）。
fn editor_element(
    state: &Entity<EditorState>,
    eol: Eol,
    font: &EditorFont,
    readonly: bool,
) -> Editor {
    let target = state.clone();
    let selection = state.clone();
    Editor::new(state)
        .bordered(false)
        .rounded_none()
        .size_full()
        .readonly(readonly)
        .font_family(font.family.clone())
        .text_size(font.size)
        .context_menu(move |menu, _, cx| {
            let selected = !selection.read(cx).selected_range().is_empty();
            let editable = !readonly;
            menu.menu_with_disabled(t!("editor.menu.undo"), !editable, Box::new(input::Undo))
                .menu_with_disabled(t!("editor.menu.redo"), !editable, Box::new(input::Redo))
                .separator()
                .menu_with_disabled(
                    t!("editor.menu.cut"),
                    !(editable && selected),
                    Box::new(input::Cut),
                )
                .menu_with_disabled(t!("editor.menu.copy"), !selected, Box::new(input::Copy))
                .menu_with_disabled(t!("editor.menu.paste"), !editable, Box::new(input::Paste))
                .separator()
                .menu(t!("editor.menu.select_all"), Box::new(input::SelectAll))
                .separator()
                .menu(t!("editor.find"), Box::new(input::Search))
        })
        .on_paste(move |item, window, cx| {
            let Some(pasted) = item.text() else {
                return false;
            };
            match text::normalize_paste(&pasted, eol) {
                std::borrow::Cow::Borrowed(_) => false,
                std::borrow::Cow::Owned(unified) => {
                    target.update(cx, |editor, cx| editor.replace(unified, window, cx));
                    true
                }
            }
        })
}

impl BasePanel for EditorPanel {
    fn panel_name(&self) -> &'static str {
        PANEL_NAME
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.editor
                .update(cx, |editor, cx| editor.focus(window, cx));
        }
        cx.notify();
    }
}

impl Panel for EditorPanel {
    fn title(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        h_flex()
            .id(("editor-tab-title", cx.entity_id().as_u64() as usize))
            // 右键 Tab 打开这个 Tab 的菜单（Tab 栏上没有“…”，见 dock_skin）
            .context_menu({
                let this = cx.entity().downgrade();
                move |menu, window, cx| match this.upgrade() {
                    Some(panel) => {
                        panel.update(cx, |panel, cx| panel.dropdown_menu(menu, window, cx))
                    }
                    None => menu,
                }
            })
            .child(self.render_title(menu_open, cx))
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    /// Tab 菜单（右键 Tab 打开）中的项：保存、查找与替换、关闭派发与快捷键相同的动作。
    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        use gpui_kit::component::menu::PopupMenuItem;
        let this = cx.entity().downgrade();
        let reload_enabled = self.is_ready() && !self.saving;
        let menu = menu
            .action_context(self.focus_handle(cx))
            .menu_with_disabled(t!("common.save"), Box::new(Save), !self.can_save())
            .menu(t!("editor.find"), Box::new(Find))
            .item(
                PopupMenuItem::new(t!("editor.reload"))
                    .disabled(!reload_enabled)
                    .on_click(move |_: &ClickEvent, window, cx| {
                        let _ = this.update(cx, |this, cx| this.confirm_reload(window, cx));
                    }),
            )
            .separator()
            .menu(t!("panel.close"), Box::new(CloseTab));
        crate::workspace::close_tab_items(menu, cx.entity_id(), window, cx).separator()
    }
}

impl Render for EditorPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(cx);
        let link_banner = self.render_link_banner(cx);
        let banner = self.render_banner(cx);
        let status_bar = self.render_status_bar(cx);
        let editor = editor_element(&self.editor, self.eol, &self.font, self.loading);
        v_flex()
            .id("editor-panel")
            .key_context(KEY_CONTEXT)
            .size_full()
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_find))
            .on_action(cx.listener(Self::on_close_tab))
            // macOS 菜单的“查找”“关闭 Tab”用的是终端的动作（菜单项只能有一个动作），在编辑器中当作自己的
            .on_action(
                cx.listener(|this, _: &crate::terminal_search::OpenSearch, window, cx| {
                    this.open_find(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::terminal_panel::CloseTab, window, cx| {
                    this.request_close(window, cx)
                }),
            )
            .child(toolbar)
            .children(link_banner)
            .children(banner)
            .child(div().flex_1().min_h_0().child(editor))
            .child(status_bar)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::input::RopeExt as _;
    use gpui_kit::test::{TestSupportExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, Bounds, ClipboardItem, Context, Entity, IntoElement, Render,
        TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
    };

    use super::*;

    /// 只放编辑区的视图：与编辑器 Tab 用同一个 [`editor_element`]。
    struct Harness {
        state: Entity<EditorState>,
        eol: Eol,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let font = EditorFont {
                family: termlet_terminal::fonts::DEFAULT_FAMILY.into(),
                size: px(14.),
            };
            div().size_full().child(
                div()
                    .id("editor")
                    .size_full()
                    .child(editor_element(&self.state, self.eol, &font, false))
                    .test_support(),
            )
        }
    }

    /// 按打开文件的流程放进编辑器：检测换行符、转成编辑器中的文本。
    fn open_text(
        cx: &mut TestAppContext,
        file: &str,
    ) -> (AnyWindowHandle, Entity<EditorState>, Eol) {
        let eol = text::detect_eol(file);
        let content = text::to_editor(file, eol).into_owned();
        let (handle, harness) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(640.), px(480.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    let state = cx.new(|cx| {
                        EditorState::new(window, cx)
                            .language("text")
                            .default_value(content)
                    });
                    cx.new(|_| Harness { state, eol })
                },
            )
            .expect("打开测试窗口")
        });
        let state = cx.read(|cx| harness.read(cx).state.clone());
        (handle, state, eol)
    }

    /// 编辑器不改动交给它的文本（`\r` 原样保留、只按 `\n` 分行），不修改就保存时与原文件逐字节一致。
    #[gpui_kit::test]
    fn editor_keeps_text_and_saves_it_back_unchanged(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for file in [
            "",
            "a\nb\n",
            "a\r\nb\r\n",
            "a\r\nb\nc\rd\r\n",
            "\r",
            "中文\r\n",
            "😀 emoji 一行\r\n",
        ] {
            let (_, state, eol) = open_text(cx, file);
            let back =
                cx.read(|cx| text::to_disk(&state.read(cx).text().to_string(), eol).into_owned());
            assert_eq!(back, file, "{file:?}");
        }
        // 混用的文件：单独的 CR 是行中的字符
        let (_, state, _) = open_text(cx, "a\r\nb\nc\rd");
        cx.read(|cx| {
            let text = state.read(cx).text();
            assert_eq!(text.lines_len(), 3);
            assert_eq!(text.slice_line(0).to_string(), "a\r");
            assert_eq!(text.slice_line(2).to_string(), "c\rd");
        });
    }

    /// CRLF 文件：粘贴的 CRLF 文本按 LF 进入编辑器，保存时还原为 CRLF；混用的文件粘贴的内容原样保留。
    #[gpui_kit::test]
    fn pasted_line_breaks_follow_the_file(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, state, eol) = open_text(cx, "a\r\nb");
        cx.write_to_clipboard(ClipboardItem::new_string("x\r\ny\r\n".into()));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("editor", cx);
            window.press("ctrl-end", cx);
            window.press("ctrl-v", cx);
        })
        .unwrap();
        cx.read(|cx| {
            let text = state.read(cx).text().to_string();
            assert_eq!(text, "a\nbx\ny\n");
            assert_eq!(text::to_disk(&text, eol), "a\r\nbx\r\ny\r\n");
        });

        let (handle, state, eol) = open_text(cx, "a\r\nb\nc");
        assert_eq!(eol, Eol::Mixed);
        cx.write_to_clipboard(ClipboardItem::new_string("x\r\ny".into()));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("editor", cx);
            window.press("ctrl-end", cx);
            window.press("ctrl-v", cx);
        })
        .unwrap();
        cx.read(|cx| assert_eq!(state.read(cx).text().to_string(), "a\r\nb\ncx\r\ny"));
    }
}
