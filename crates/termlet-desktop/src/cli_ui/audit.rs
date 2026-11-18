//! 审计日志对话框：时间、来源、主机、动作、命令或文件、结果、退出码的列表，按主机、内容、调用方搜索，
//! 按结果与来源筛选，按 ID 向更早翻页；选中一条在底部显示完整内容与耗时、调用方等；导出 CSV / JSON（另存为）、
//! 清空（先确认）。
//!
//! 读写都在后台执行器上（SQLite，毫秒级）。条件改了之后重新读取，旧查询较晚返回的结果按版本丢弃。
//! 打开着时写入了新记录：主会话调用 [`refresh_open_audit`]，这里只把比列表顶端更新的记录补在前面，已加载的页保留。

use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, VirtualListScrollHandle,
    WindowExt as _, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable as _,
    Global, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render,
    ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    TestSupportExt as _, WeakEntity, Window, div, prelude::FluentBuilder as _, px, rems, size,
};
use termlet_core::audit::{
    AuditAction, AuditEntry, AuditLog, AuditQuery, AuditSource, AuditStatus, ExportFormat,
};

use crate::choice::{Choice, ChoiceState, choice_state};
use crate::notify::{self, Notice, NoticeLine};
use crate::prose::prose;
use crate::sftp::list_model::format_size;
use crate::t;

/// 每页的条数。
pub(super) const PAGE: usize = 200;
/// 搜索框停止输入这么久后再查询。
pub(super) const SEARCH_DELAY: Duration = Duration::from_millis(250);
/// 列表中内容列最多显示的字符数（整条在详情中看）。
const DETAIL_COLUMN: usize = 300;

/// 筛选菜单中的结果与来源，按菜单中的顺序。
const STATUSES: [AuditStatus; 5] = [
    AuditStatus::Ok,
    AuditStatus::Approved,
    AuditStatus::Failed,
    AuditStatus::Blocked,
    AuditStatus::Denied,
];
const SOURCES: [AuditSource; 2] = [AuditSource::Cli, AuditSource::Panel];

/// 各列的宽度（rem）；内容列占余下的宽度。
const TIME_WIDTH: f32 = 9.;
const SOURCE_WIDTH: f32 = 4.5;
const HOST_WIDTH: f32 = 7.;
const ACTION_WIDTH: f32 = 6.;
const RESULT_WIDTH: f32 = 4.;
const EXIT_WIDTH: f32 = 3.;

// ------------------------------------------------------------ 对外

/// 打开着的审计对话框（关闭后弱引用失效）。
#[derive(Default)]
struct OpenAudits(Vec<WeakEntity<AuditView>>);

impl Global for OpenAudits {}

/// 打开审计日志对话框（设置中的“查看审计日志…”、命令面板）。审计日志没能打开时只说明原因（`notify::show`：
/// 从设置中打开时写在设置底部）。
pub(crate) fn open_audit(window: &mut Window, cx: &mut App) {
    match crate::cli_bridge::audit(cx) {
        Some(log) => open_audit_with(log, window, cx),
        None => notify::show(Notice::error(t!("cli_ui.audit.unavailable")), window, cx),
    }
}

/// 用指定的审计日志打开对话框（测试中用内存数据库上的日志）。
pub(super) fn open_audit_with(log: Arc<AuditLog>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| AuditView::new(log, window, cx));
    let open = &mut cx.default_global::<OpenAudits>().0;
    open.retain(|w| w.upgrade().is_some());
    open.push(view.downgrade());
    let notice = view.read(cx).notice.clone();
    let width = rems(64.).to_pixels(window.rem_size());
    let first = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            view.read(cx).search.focus_handle(cx).focus(window, cx);
        }
        dialog
            .title(t!("cli_ui.audit.title"))
            .w(width)
            // 搜索框中按 Enter 只是立即搜索，不关闭对话框
            .on_ok(|_, _, _| false)
            .child(view.clone())
    });
    notify::attach(&notice, window, cx);
}

/// 写入了新的审计记录（命令行服务的 `CliEvent::Audited`、主机工具面板的写操作）：打开着的审计对话框补上新记录。
pub(crate) fn refresh_open_audit(cx: &mut App) {
    let Some(open) = cx.try_global::<OpenAudits>() else {
        return;
    };
    let views: Vec<Entity<AuditView>> = open.0.iter().filter_map(WeakEntity::upgrade).collect();
    cx.global_mut::<OpenAudits>()
        .0
        .retain(|w| w.upgrade().is_some());
    for view in views {
        view.update(cx, |view, cx| view.fetch_newer(cx));
    }
}

/// 打开着的审计对话框（测试用）。
#[cfg(test)]
pub(super) fn open_views(cx: &App) -> Vec<Entity<AuditView>> {
    cx.try_global::<OpenAudits>()
        .map(|open| open.0.iter().filter_map(WeakEntity::upgrade).collect())
        .unwrap_or_default()
}

// ------------------------------------------------------------ 查询与文案

/// 筛选条件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Filter {
    /// 搜索框中的文字（查询时去掉首尾空白）
    pub(super) q: String,
    pub(super) status: Option<AuditStatus>,
    pub(super) source: Option<AuditSource>,
}

impl Filter {
    /// 一页的查询：`before` 为 0 时从最新的开始，否则只要 ID 小于它的。
    pub(super) fn query(&self, before: i64) -> AuditQuery {
        AuditQuery {
            limit: PAGE,
            before,
            q: self.q.trim().to_string(),
            status: self.status,
            source: self.source,
        }
    }

    /// 是否有筛选条件（没有记录时据此说“没有符合条件的”还是“还没有”）。
    pub(super) fn is_active(&self) -> bool {
        !self.q.trim().is_empty() || self.status.is_some() || self.source.is_some()
    }
}

/// 新读到的第一页怎样并入列表。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Merge {
    /// 比列表顶端更新的记录，加在前面
    Prepend(Vec<AuditEntry>),
    /// 整个换掉：列表是空的，或者新记录有一整页（与已有的记录之间可能有缺口）
    Replace(Vec<AuditEntry>),
}

/// 把重新读到的第一页（按 ID 倒序）并入顶端 ID 为 `top` 的列表。
pub(super) fn merge_newer(top: Option<i64>, mut fresh: Vec<AuditEntry>) -> Merge {
    let Some(top) = top else {
        return Merge::Replace(fresh);
    };
    let newer = fresh.iter().take_while(|e| e.id > top).count();
    if newer == fresh.len() && fresh.len() >= PAGE {
        return Merge::Replace(fresh);
    }
    fresh.truncate(newer);
    Merge::Prepend(fresh)
}

pub(super) fn status_label(status: AuditStatus) -> SharedString {
    match status {
        AuditStatus::Ok => t!("cli_ui.audit.status_ok"),
        AuditStatus::Failed => t!("cli_ui.audit.status_failed"),
        AuditStatus::Blocked => t!("cli_ui.audit.status_blocked"),
        AuditStatus::Approved => t!("cli_ui.audit.status_approved"),
        AuditStatus::Denied => t!("cli_ui.audit.status_denied"),
        AuditStatus::Other => t!("cli_ui.audit.status_other"),
    }
}

pub(super) fn source_label(source: AuditSource) -> SharedString {
    match source {
        AuditSource::Cli => t!("cli_ui.audit.source_cli"),
        AuditSource::Panel => t!("cli_ui.audit.source_panel"),
    }
}

pub(super) fn action_label(action: AuditAction) -> SharedString {
    match action {
        AuditAction::Exec => t!("cli_ui.audit.action_exec"),
        AuditAction::Terminal => t!("cli_ui.audit.action_terminal"),
        AuditAction::Upload => t!("cli_ui.audit.action_upload"),
        AuditAction::Download => t!("cli_ui.audit.action_download"),
        AuditAction::Panel => t!("cli_ui.audit.action_panel"),
        AuditAction::PanelTerminal => t!("cli_ui.audit.action_panel_terminal"),
        AuditAction::Other => t!("cli_ui.audit.action_other"),
    }
}

/// 本地时间 `YYYY-MM-DD HH:MM:SS`。秒数与时区无关（现行的时区偏移都是整分钟），直接由毫秒数得出。
pub(super) fn format_time(ms: i64) -> String {
    let minute = termlet_core::time::format_local(ms);
    if minute.is_empty() {
        return minute;
    }
    format!("{minute}:{:02}", ms.div_euclid(1000).rem_euclid(60))
}

/// 导出文件的默认名称：`termlet-audit-2026-10-20.csv`。
pub(super) fn export_file_name(format: ExportFormat, now_ms: i64) -> String {
    let local = termlet_core::time::format_local(now_ms);
    let date = local.get(..10).unwrap_or("export");
    let extension = match format {
        ExportFormat::Csv => "csv",
        ExportFormat::Json => "json",
    };
    format!("termlet-audit-{date}.{extension}")
}

/// 把符合条件的全部记录导出到 `path`，返回条数；失败时删掉写了一半的文件。
pub(super) fn export_to(
    log: &AuditLog,
    path: &Path,
    format: ExportFormat,
    query: &AuditQuery,
) -> Result<usize, String> {
    let written = std::fs::File::create(path)
        .map_err(|e| e.to_string())
        .and_then(|file| {
            let mut out = BufWriter::new(file);
            log.export(&mut out, format, query)
                .map_err(|e| e.to_string())
        });
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

// ------------------------------------------------------------ 视图

pub(super) struct AuditView {
    log: Arc<AuditLog>,
    search: Entity<InputState>,
    status: Entity<ChoiceState<Option<AuditStatus>>>,
    source: Entity<ChoiceState<Option<AuditSource>>>,
    pub(super) filter: Filter,
    /// 已加载的记录（按 ID 倒序，内容只有开头部分）
    pub(super) entries: Vec<AuditEntry>,
    /// 上一页取满了：可能还有更早的记录
    pub(super) more: bool,
    /// 第一页已经读到（区分“正在读取”与“没有记录”）
    pub(super) loaded: bool,
    loading: bool,
    loading_more: bool,
    /// 正在补新记录；期间又有新记录时 `newer_again`，补完再补一次
    newer_running: bool,
    newer_again: bool,
    /// 导出、清空进行中
    busy: bool,
    error: Option<SharedString>,
    /// 导出、清空、复制的结果（写在底部，见 [`notify::show`]）
    notice: Entity<NoticeLine>,
    /// 选中的记录；列表中的只有开头部分（`partial`），选中后再取完整内容
    pub(super) selected: Option<AuditEntry>,
    detail: Entity<TextareaState>,
    /// 查询的版本：条件改了、重新读取时加 1，旧查询的结果丢弃
    revision: u64,
    scroll: VirtualListScrollHandle,
    focus: FocusHandle,
    _search_delay: Option<Task<()>>,
    _load: Option<Task<()>>,
    _load_more: Option<Task<()>>,
    _newer: Option<Task<()>>,
    _detail: Option<Task<()>>,
    _job: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

type StatusEvent = SelectEvent<SearchableVec<Choice<Option<AuditStatus>>>>;
type SourceEvent = SelectEvent<SearchableVec<Choice<Option<AuditSource>>>>;

impl AuditView {
    fn new(log: Arc<AuditLog>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("cli_ui.audit.search"))
                .clean_on_escape()
        });
        let status = choice_state(
            std::iter::once(Choice::new(t!("cli_ui.audit.all_statuses"), None))
                .chain(
                    STATUSES
                        .iter()
                        .map(|s| Choice::new(status_label(*s), Some(*s))),
                )
                .collect(),
            &None,
            false,
            window,
            cx,
        );
        let source = choice_state(
            std::iter::once(Choice::new(t!("cli_ui.audit.all_sources"), None))
                .chain(
                    SOURCES
                        .iter()
                        .map(|s| Choice::new(source_label(*s), Some(*s))),
                )
                .collect(),
            &None,
            false,
            window,
            cx,
        );
        let detail = cx.new(|cx| TextareaState::new(window, cx).rows(6));
        let subscriptions = vec![
            cx.subscribe(&search, |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change => this.schedule_search(cx),
                InputEvent::PressEnter { .. } => this.apply_search(cx),
                _ => {}
            }),
            cx.subscribe(&status, |this, _, event: &StatusEvent, cx| {
                let SelectEvent::Confirm(value) = event;
                this.set_status(value.flatten(), cx);
            }),
            cx.subscribe(&source, |this, _, event: &SourceEvent, cx| {
                let SelectEvent::Confirm(value) = event;
                this.set_source(value.flatten(), cx);
            }),
        ];
        let mut this = Self {
            log,
            search,
            status,
            source,
            filter: Filter::default(),
            entries: Vec::new(),
            more: false,
            loaded: false,
            loading: false,
            loading_more: false,
            newer_running: false,
            newer_again: false,
            busy: false,
            error: None,
            notice: NoticeLine::new(cx),
            selected: None,
            detail,
            revision: 0,
            scroll: VirtualListScrollHandle::new(),
            focus: cx.focus_handle(),
            _search_delay: None,
            _load: None,
            _load_more: None,
            _newer: None,
            _detail: None,
            _job: None,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    // ------------------------------------------------------------ 查询

    /// 搜索框停止输入一会儿后再查询。
    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self._search_delay = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            let _ = this.update(cx, |this, cx| this.apply_search(cx));
        }));
    }

    fn apply_search(&mut self, cx: &mut Context<Self>) {
        self._search_delay = None;
        let q = self.search.read(cx).value().to_string();
        if q != self.filter.q {
            self.filter.q = q;
            self.reload(cx);
        }
    }

    pub(super) fn set_status(&mut self, status: Option<AuditStatus>, cx: &mut Context<Self>) {
        if self.filter.status != status {
            self.filter.status = status;
            self.reload(cx);
        }
    }

    pub(super) fn set_source(&mut self, source: Option<AuditSource>, cx: &mut Context<Self>) {
        if self.filter.source != source {
            self.filter.source = source;
            self.reload(cx);
        }
    }

    /// 按当前条件从最新的记录重新读取第一页。
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        self.revision += 1;
        let revision = self.revision;
        self.loading = true;
        self.loading_more = false;
        self._load_more = None;
        let log = self.log.clone();
        let query = self.filter.query(0);
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { log.list(&query) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.revision != revision {
                    return;
                }
                this.loading = false;
                this.loaded = true;
                match result {
                    Ok(list) => {
                        this.more = list.len() >= PAGE;
                        this.entries = list;
                        this.error = None;
                        let kept = this
                            .selected
                            .as_ref()
                            .is_some_and(|s| this.entries.iter().any(|e| e.id == s.id));
                        if !kept {
                            this.selected = None;
                        }
                        this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                    }
                    Err(e) => {
                        tracing::warn!("读取审计日志失败：{e}");
                        this.error = Some(t!("cli_ui.audit.load_failed", error = e.to_string()));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 读取更早的一页，接在列表后面。
    pub(super) fn load_more(&mut self, cx: &mut Context<Self>) {
        if !self.more || self.loading || self.loading_more {
            return;
        }
        let Some(last) = self.entries.last() else {
            return;
        };
        let revision = self.revision;
        let query = self.filter.query(last.id);
        let log = self.log.clone();
        self.loading_more = true;
        self._load_more = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { log.list(&query) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.revision != revision {
                    return;
                }
                this.loading_more = false;
                match result {
                    Ok(list) => {
                        this.more = list.len() >= PAGE;
                        this.entries.extend(list);
                    }
                    Err(e) => {
                        tracing::warn!("读取审计日志失败：{e}");
                        this.error = Some(t!("cli_ui.audit.load_failed", error = e.to_string()));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 补上比列表顶端更新的记录（写入了新记录时）。
    pub(super) fn fetch_newer(&mut self, cx: &mut Context<Self>) {
        // 第一页还没读到：读到的就含新记录
        if !self.loaded || self.loading {
            return;
        }
        if self.newer_running {
            self.newer_again = true;
            return;
        }
        self.newer_running = true;
        let revision = self.revision;
        let query = self.filter.query(0);
        let log = self.log.clone();
        self._newer = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { log.list(&query) }).await;
            let _ = this.update(cx, |this, cx| {
                this.newer_running = false;
                if this.revision == revision
                    && let Ok(fresh) = result
                {
                    match merge_newer(this.entries.first().map(|e| e.id), fresh) {
                        Merge::Prepend(newer) => {
                            this.entries.splice(0..0, newer);
                        }
                        Merge::Replace(page) => {
                            this.more = page.len() >= PAGE;
                            this.entries = page;
                        }
                    }
                    cx.notify();
                }
                if std::mem::take(&mut this.newer_again) {
                    this.fetch_newer(cx);
                }
            });
        }));
    }

    // ------------------------------------------------------------ 选中与详情

    pub(super) fn select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.iter().find(|e| e.id == id).cloned() else {
            return;
        };
        let text = if entry.partial {
            format!("{}…", entry.detail)
        } else {
            entry.detail.clone()
        };
        self.detail
            .update(cx, |state, cx| state.set_value(text, window, cx));
        let partial = entry.partial;
        self.selected = Some(entry);
        self._detail = None;
        if partial {
            let log = self.log.clone();
            self._detail = Some(cx.spawn_in(window, async move |this, cx| {
                let result = cx.background_spawn(async move { log.get(id) }).await;
                let _ = this.update_in(cx, |this, window, cx| {
                    // 期间又选了别的记录：不覆盖
                    if this.selected.as_ref().map(|e| e.id) != Some(id) {
                        return;
                    }
                    match result {
                        Ok(full) => {
                            let text = full.detail.clone();
                            this.detail
                                .update(cx, |state, cx| state.set_value(text, window, cx));
                            this.selected = Some(full);
                        }
                        Err(e) => {
                            this.error =
                                Some(t!("cli_ui.audit.load_failed", error = e.to_string()));
                        }
                    }
                    cx.notify();
                });
            }));
        }
        cx.notify();
    }

    /// 列表中按上下键换选中的记录。
    fn on_list_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let step: isize = match event.keystroke.key.as_str() {
            "up" => -1,
            "down" => 1,
            _ => return,
        };
        if self.entries.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|s| self.entries.iter().position(|e| e.id == s.id));
        let next = match current {
            Some(ix) => ix.saturating_add_signed(step).min(self.entries.len() - 1),
            None => 0,
        };
        let id = self.entries[next].id;
        self.select(id, window, cx);
        self.scroll.scroll_to_item(next, ScrollStrategy::Nearest);
        cx.stop_propagation();
    }

    // ------------------------------------------------------------ 导出与清空

    fn export(&mut self, format: ExportFormat, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let dir = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let name = export_file_name(format, termlet_core::time::now_ms());
        let picked = cx.prompt_for_new_path(&dir, Some(&name));
        let log = self.log.clone();
        let query = self.filter.query(0);
        self.busy = true;
        self._job = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    cx.notify();
                });
                return;
            };
            let target = path.clone();
            let result = cx
                .background_spawn(async move { export_to(&log, &target, format, &query) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                let notice = match result {
                    Ok(count) => Notice::success(t!(
                        "cli_ui.audit.exported",
                        count = count,
                        path = path.display().to_string()
                    )),
                    Err(error) => {
                        tracing::warn!("导出审计日志失败：{error}");
                        Notice::error(t!("cli_ui.audit.export_failed", error = error))
                    }
                };
                notify::show(notice, window, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn confirm_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let view = view.clone();
            alert
                .confirm()
                .title(t!("cli_ui.audit.clear_title"))
                .description(prose(t!("cli_ui.audit.clear_body")))
                .ok_text(t!("cli_ui.audit.clear_ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = view.update(cx, |view, cx| view.clear(window, cx));
                    true
                })
        });
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let log = self.log.clone();
        self._job = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { log.clear() }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        this.selected = None;
                        this.reload(cx);
                    }
                    Err(e) => {
                        tracing::warn!("清空审计日志失败：{e}");
                        notify::show(
                            Notice::error(t!("cli_ui.audit.clear_failed", error = e.to_string())),
                            window,
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------ 渲染

    fn status_color(entry: &AuditEntry, cx: &App) -> Hsla {
        let theme = cx.theme();
        match entry.status {
            AuditStatus::Blocked | AuditStatus::Denied => theme.danger,
            AuditStatus::Failed => theme.warning,
            AuditStatus::Ok | AuditStatus::Approved => match entry.exit_code {
                None | Some(0) => theme.success,
                Some(_) => theme.foreground,
            },
            AuditStatus::Other => theme.muted_foreground,
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let empty = self.entries.is_empty();
        h_flex()
            .gap_2()
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.search)
                        .id("audit-search")
                        .small()
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).xsmall().text_color(muted)),
                ),
            )
            .child(
                div()
                    .w(rems(8.))
                    .flex_none()
                    .child(Select::new(&self.source).small().id("audit-source")),
            )
            .child(
                div()
                    .w(rems(8.))
                    .flex_none()
                    .child(Select::new(&self.status).small().id("audit-status")),
            )
            .child(
                Button::new("audit-refresh")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip(t!("cli_ui.audit.refresh"))
                    .loading(self.loading && self.loaded)
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
            .child(
                Button::new("audit-export-csv")
                    .ghost()
                    .small()
                    .icon(IconName::FileDown)
                    .label(t!("cli_ui.audit.export_csv"))
                    .tooltip(t!("cli_ui.audit.export_hint"))
                    .disabled(self.busy || empty)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.export(ExportFormat::Csv, window, cx)
                    })),
            )
            .child(
                Button::new("audit-export-json")
                    .ghost()
                    .small()
                    .icon(IconName::FileDown)
                    .label(t!("cli_ui.audit.export_json"))
                    .tooltip(t!("cli_ui.audit.export_hint"))
                    .disabled(self.busy || empty)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.export(ExportFormat::Json, window, cx)
                    })),
            )
            .child(
                Button::new("audit-clear")
                    .ghost()
                    .small()
                    .icon(IconName::Trash)
                    .label(t!("cli_ui.audit.clear"))
                    .disabled(self.busy || (empty && !self.filter.is_active()))
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_clear(window, cx))),
            )
    }

    fn render_header(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let cell = |width: f32, text: SharedString| {
            div()
                .flex_none()
                .w(rems(width))
                .truncate()
                .child(prose(text))
        };
        h_flex()
            .flex_none()
            .h(rems(1.75))
            .px_2()
            .gap_2()
            .text_xs()
            .text_color(theme.muted_foreground)
            .bg(theme.sidebar)
            .border_b_1()
            .border_color(theme.border)
            .child(cell(TIME_WIDTH, t!("cli_ui.audit.col_time")))
            .child(cell(SOURCE_WIDTH, t!("cli_ui.audit.col_source")))
            .child(cell(HOST_WIDTH, t!("cli_ui.audit.col_host")))
            .child(cell(ACTION_WIDTH, t!("cli_ui.audit.col_action")))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(t!("cli_ui.audit.col_detail")),
            )
            .child(cell(RESULT_WIDTH, t!("cli_ui.audit.col_result")))
            .child(
                div()
                    .flex_none()
                    .w(rems(EXIT_WIDTH))
                    .text_right()
                    .truncate()
                    .child(t!("cli_ui.audit.col_exit")),
            )
    }

    fn render_row(&self, entry: &AuditEntry, cx: &mut Context<Self>) -> AnyElement {
        let id = entry.id;
        let selected = self.selected.as_ref().is_some_and(|s| s.id == id);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (active, hover) = (theme.list_active, theme.list_hover);
        let mono = theme.mono_font_family.clone();
        let status_color = Self::status_color(entry, cx);
        let exit_color = match entry.exit_code {
            Some(0) | None => muted,
            Some(_) => cx.theme().foreground,
        };
        let cell = |width: f32| div().flex_none().w(rems(width)).truncate();
        h_flex()
            .id(("audit-row", id as u64))
            .test_support()
            .size_full()
            .px_2()
            .gap_2()
            .text_xs()
            .cursor_default()
            .when(selected, |this| this.bg(active))
            .when(!selected, |this| this.hover(|s| s.bg(hover)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                this.select(id, window, cx);
            }))
            .child(
                cell(TIME_WIDTH)
                    .text_color(muted)
                    .child(format_time(entry.at)),
            )
            .child(cell(SOURCE_WIDTH).child(source_label(entry.source)))
            .child(cell(HOST_WIDTH).child(entry.host_name.clone()))
            .child(cell(ACTION_WIDTH).child(action_label(entry.action)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(mono)
                    .child(termlet_core::guard::one_line(&entry.detail, DETAIL_COLUMN)),
            )
            .child(
                cell(RESULT_WIDTH)
                    .text_color(status_color)
                    .child(status_label(entry.status)),
            )
            .child(
                div()
                    .flex_none()
                    .w(rems(EXIT_WIDTH))
                    .text_right()
                    .text_color(exit_color)
                    .children(entry.exit_code.map(|c| c.to_string())),
            )
            .into_any_element()
    }

    /// 没有记录时的说明：正在读取、没有符合条件的、还没有记录。
    fn render_empty(&self, cx: &App) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let (title, hint) = if !self.loaded {
            (t!("cli_ui.audit.loading"), None)
        } else if self.filter.is_active() {
            (t!("cli_ui.audit.no_match"), None)
        } else {
            (
                t!("cli_ui.audit.empty"),
                Some(t!("cli_ui.audit.empty_hint")),
            )
        };
        v_flex()
            .id("audit-empty")
            .test_support()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .text_sm()
            .text_color(muted)
            .child(title)
            .when_some(hint, |this, hint| {
                this.child(div().text_xs().child(prose(hint)))
            })
            .into_any_element()
    }

    fn render_table(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, radius) = (theme.border, theme.radius);
        let body = if self.entries.is_empty() {
            self.render_empty(cx)
        } else {
            let row_height = rems(1.75).to_pixels(window.rem_size());
            let sizes = Rc::new(vec![size(px(0.), row_height); self.entries.len()]);
            v_virtual_list(cx.entity(), "audit-rows", sizes, |this, range, _, cx| {
                range
                    .filter_map(|ix| this.entries.get(ix).map(|e| this.render_row(e, cx)))
                    .collect::<Vec<_>>()
            })
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };
        v_flex()
            .id("audit-list")
            .test_support()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_list_key))
            .flex_1()
            .min_h_0()
            .border_1()
            .border_color(border)
            .rounded(radius)
            .overflow_hidden()
            .child(self.render_header(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .vertical_scrollbar(&self.scroll),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        h_flex()
            .flex_none()
            .gap_2()
            .text_xs()
            .text_color(muted)
            .child(
                div()
                    .id("audit-count")
                    .test_support()
                    .flex_none()
                    .child(t!("cli_ui.audit.count", count = self.entries.len())),
            )
            .child(div().flex_1().min_w_0().child(self.notice.clone()))
            .when(self.more, |this| {
                this.child(
                    Button::new("audit-load-more")
                        .outline()
                        .small()
                        .label(t!("cli_ui.audit.load_more"))
                        .loading(self.loading_more)
                        .disabled(self.loading_more || self.loading)
                        .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                )
            })
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let entry = self.selected.as_ref()?;
        let theme = cx.theme();
        let (muted, warning, border) = (theme.muted_foreground, theme.warning, theme.border);
        let mono = theme.mono_font_family.clone();
        let status_color = Self::status_color(entry, cx);
        let mut meta: Vec<AnyElement> = vec![
            div().child(format_time(entry.at)).into_any_element(),
            div()
                .child(format!(
                    "{} · {}",
                    source_label(entry.source),
                    action_label(entry.action)
                ))
                .into_any_element(),
            div().child(entry.host_name.clone()).into_any_element(),
            div()
                .text_color(status_color)
                .child(status_label(entry.status))
                .into_any_element(),
        ];
        if let Some(code) = entry.exit_code {
            meta.push(
                div()
                    .child(t!("cli_ui.audit.exit_code", code = code))
                    .into_any_element(),
            );
        }
        meta.push(
            div()
                .child(t!("cli_ui.audit.duration", ms = entry.duration_ms))
                .into_any_element(),
        );
        if let Some(bytes) = entry.bytes {
            meta.push(
                div()
                    .child(t!(
                        "cli_ui.audit.bytes",
                        size = format_size(bytes.max(0) as u64)
                    ))
                    .into_any_element(),
            );
        }
        if !entry.error_code.is_empty() {
            meta.push(
                div()
                    .text_color(theme.danger)
                    .child(t!(
                        "cli_ui.audit.error_code",
                        code = entry.error_code.clone()
                    ))
                    .into_any_element(),
            );
        }
        let copied = entry.detail.clone();
        let truncated = entry.truncated.then(|| {
            t!(
                "cli_ui.audit.truncated",
                size = format_size(entry.detail_size.max(0) as u64)
            )
        });
        let caller = (!entry.caller.is_empty())
            .then(|| t!("cli_ui.audit.caller", caller = entry.caller.clone()));
        let panel_terminal = entry.action == AuditAction::PanelTerminal;
        Some(
            v_flex()
                .id("audit-detail")
                .test_support()
                .flex_none()
                .gap_1p5()
                .pt_2()
                .border_t_1()
                .border_color(border)
                .text_xs()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .flex_wrap()
                                .gap_x_3()
                                .text_color(muted)
                                .children(meta),
                        )
                        .child(
                            Button::new("audit-copy-detail")
                                .ghost()
                                .small()
                                .icon(IconName::Copy)
                                .tooltip(t!("cli_ui.audit.copy_detail"))
                                .on_click(move |_, window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copied.clone(),
                                    ));
                                    notify::show(
                                        Notice::success(t!("cli_ui.audit.detail_copied")),
                                        window,
                                        cx,
                                    );
                                }),
                        )
                        .child(
                            Button::new("audit-close-detail")
                                .ghost()
                                .small()
                                .icon(IconName::Close)
                                .tooltip(t!("cli_ui.audit.close_detail"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.selected = None;
                                    this._detail = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(Textarea::new(&self.detail).readonly(true).font_family(mono))
                .when(entry.partial, |this| {
                    this.child(
                        div()
                            .text_color(muted)
                            .child(prose(t!("cli_ui.audit.detail_loading"))),
                    )
                })
                .when_some(truncated, |this, text| {
                    this.child(div().text_color(warning).child(prose(text)))
                })
                .when_some(caller, |this, text| {
                    this.child(div().text_color(muted).child(prose(text)))
                })
                .when(panel_terminal, |this| {
                    this.child(
                        div()
                            .text_color(muted)
                            .child(prose(t!("cli_ui.audit.panel_terminal_note"))),
                    )
                })
                .into_any_element(),
        )
    }
}

impl Render for AuditView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 高 40rem，窗口矮时缩到窗口内（对话框上下各留一些边距，从设置中打开时叠在设置上面）
        let rem = window.rem_size();
        let height = rems(40.)
            .to_pixels(rem)
            .min(window.viewport_size().height - rems(9.).to_pixels(rem));
        let danger = cx.theme().danger;
        v_flex()
            .id("audit-view")
            .test_support()
            .h(height)
            .gap_2()
            .child(self.render_toolbar(cx))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .id("audit-error")
                        .test_support()
                        .text_xs()
                        .text_color(danger)
                        .child(prose(error)),
                )
            })
            .child(self.render_table(window, cx))
            .child(self.render_footer(cx))
            .children(self.render_detail(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{self, Language};

    fn entry(id: i64) -> AuditEntry {
        AuditEntry {
            id,
            ..Default::default()
        }
    }

    #[test]
    fn filter_builds_queries() {
        let filter = Filter {
            q: "  web  ".into(),
            status: Some(AuditStatus::Blocked),
            source: Some(AuditSource::Panel),
        };
        assert_eq!(
            filter.query(0),
            AuditQuery {
                limit: PAGE,
                before: 0,
                q: "web".into(),
                status: Some(AuditStatus::Blocked),
                source: Some(AuditSource::Panel),
            }
        );
        assert_eq!(filter.query(42).before, 42, "翻页只要更早的");
        assert!(filter.is_active());
        assert!(!Filter::default().is_active());
        assert!(
            !Filter {
                q: "   ".into(),
                ..Default::default()
            }
            .is_active(),
            "只有空白的搜索不算条件"
        );
    }

    #[test]
    fn newer_entries_are_prepended() {
        // 列表顶端是 10：只加更新的
        let fresh: Vec<_> = (8..=12).rev().map(entry).collect();
        assert_eq!(
            merge_newer(Some(10), fresh),
            Merge::Prepend(vec![entry(12), entry(11)])
        );
        assert_eq!(
            merge_newer(Some(12), vec![entry(12), entry(11)]),
            Merge::Prepend(Vec::new())
        );
        // 列表是空的：整个换掉
        assert_eq!(
            merge_newer(None, vec![entry(3)]),
            Merge::Replace(vec![entry(3)])
        );
        // 新记录有一整页：与已有的之间可能有缺口，整页换掉
        let page: Vec<_> = (1..=PAGE as i64).rev().map(|i| entry(i + 100)).collect();
        assert!(
            matches!(merge_newer(Some(100), page), Merge::Replace(list) if list.len() == PAGE),
            "新记录有一整页时应整页换掉"
        );
        // 不到一页：都是新的也只是加在前面
        let few = vec![entry(105), entry(104)];
        assert_eq!(merge_newer(Some(100), few.clone()), Merge::Prepend(few));
    }

    #[test]
    fn time_has_seconds_and_file_names_have_the_date() {
        let ms = 1_760_000_000_123; // 秒数为 20
        let time = format_time(ms);
        assert_eq!(time.len(), "2025-10-09 08:53:20".len());
        assert!(time.ends_with(":20"), "{time}");
        assert!(time.starts_with(&termlet_core::time::format_local(ms)));
        let name = export_file_name(ExportFormat::Csv, ms);
        assert!(
            name.starts_with("termlet-audit-20") && name.ends_with(".csv"),
            "{name}"
        );
        assert_eq!(name.len(), "termlet-audit-2025-10-09.csv".len());
        assert!(export_file_name(ExportFormat::Json, ms).ends_with(".json"));
    }

    #[test]
    fn labels_follow_the_language() {
        let original = i18n::language();
        i18n::set_language(Language::ZhCn);
        assert_eq!(status_label(AuditStatus::Blocked), "已拦截");
        assert_eq!(source_label(AuditSource::Panel), "主机工具");
        assert_eq!(action_label(AuditAction::PanelTerminal), "交给终端执行");
        i18n::set_language(Language::En);
        assert_eq!(status_label(AuditStatus::Denied), "Denied");
        assert_eq!(action_label(AuditAction::Terminal), "Run in terminal");
        i18n::set_language(original);
    }
}
