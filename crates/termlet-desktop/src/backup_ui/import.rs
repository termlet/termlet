//! 导入主机：选来源 → 选文件或文件夹（来源在本机有默认位置时自动填入）→ 预览 → 搜索、筛选、勾选 → 导入 → 结果。
//!
//! - 预览列出来源中的主机，标出本机已有的相同主机（默认不勾选）、需要提醒的事项、代理命令、登录后执行的命令与
//!   随主机导入的端口转发。勾选的逻辑在核心（`termlet_core::import::select`）：全选、全不选、只选新主机、反选与
//!   表头的勾选框只作用于筛选出的行；单击整行切换，按住 Shift 单击把上次单击的行到这一行设为相同的状态。
//! - Termlet 的主机文件加密时先要密码（导出时设置的）；WindTerm 设了主密码时先要它的主密码。
//! - 来源中有快捷命令（WindTerm）时，预览下方说明会一并导入多少条，结果中列出新增了多少条。
//! - 只有快捷命令的来源（NyaTerm 导出的快捷命令）：预览列出快捷命令（分组、名称、内容的第一行）与转换时丢掉或
//!   改动的内容，不用勾选，全部导入（与已有的名称、内容都相同的跳过）；对话框的标题随之写“导入快捷命令”。
//!   快捷命令页的“导入快捷命令…”打开时选好这个来源（[`open_with`]）。
//! - 密码是明文保存的来源（Electerm、Netcatty）带密码等秘密时，预览下方有“导入保存的密码”（默认不勾选）与说明；
//!   不勾选时按 `ImportPlan::without_secrets` 只导入主机，各行也不标“带密码 / 带私钥”。结果中列出给已有主机补上了
//!   多少个密码。
//! - 解析出的计划保存到导入为止；再次预览、切换来源后，之前还没返回的读取作废。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _,
    VirtualListScrollHandle, WindowExt as _, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window,
    div, prelude::FluentBuilder as _, px, rems, size,
};
use termlet_core::backup::BackupError;
use termlet_core::hostfile::{self, HostFile};
use termlet_core::import::select::{self, BulkAction, StatusFilter};
use termlet_core::import::{
    self, ImportPlan, ImportReport, OnDuplicate, PreviewForward, PreviewItem, SnippetNotes, Source,
};
use termlet_core::model::{ForwardKind, Protocol, Snippet, join_host_port};
use termlet_core::runtime;
use termlet_core::secret::SecretString;

use super::errors;
use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state};
use crate::prose::prose;
use crate::t;

/// 预览中一行的高度（rem）：上下留白、第一行（名称、地址等）、之后每一行说明。说明都只占一行（过长时截断，
/// 悬停显示全文），行高因此可以事先算出，列表按可见的范围渲染。
const ROW_PADDING: f32 = 0.5;
const MAIN_LINE: f32 = 1.25;
const DETAIL_LINE: f32 = 1.0;
/// 快捷命令列表中一行的高度（rem）。
const SNIPPET_ROW: f32 = 1.75;

/// 快捷命令列表中的一行。
struct SnippetRow {
    group: SharedString,
    name: SharedString,
    /// 内容中第一个不空的行
    first_line: SharedString,
    /// 内容的行数（不算末尾表示执行的换行）
    lines: usize,
}

impl SnippetRow {
    fn new(snippet: &Snippet) -> Self {
        let body = snippet.content.trim_end_matches(['\r', '\n']);
        Self {
            group: snippet.group.clone().into(),
            name: snippet.name.clone().into(),
            // 制表符显示为空白（字体中它没有宽度，看起来像被吞掉）
            first_line: body
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or_default()
                .replace('\t', "    ")
                .into(),
            lines: body.lines().count(),
        }
    }
}

/// 快捷命令转换时丢掉或改动的内容与跳过的条目，每项一句；最后一句说明相同的不重复导入。
fn snippet_notes(notes: &SnippetNotes, skipped: usize) -> Vec<SharedString> {
    let n = notes;
    let mut lines: Vec<SharedString> = [
        (
            n.pauses,
            t!("backup_ui.import.snippet_note.pauses", count = n.pauses),
        ),
        (
            n.options,
            t!("backup_ui.import.snippet_note.options", count = n.options),
        ),
        (
            n.unknown_params,
            t!(
                "backup_ui.import.snippet_note.unknown_params",
                count = n.unknown_params
            ),
        ),
        (
            n.literal_params,
            t!(
                "backup_ui.import.snippet_note.literal_params",
                count = n.literal_params
            ),
        ),
        (
            n.paced,
            t!("backup_ui.import.snippet_note.paced", count = n.paced),
        ),
        (
            n.nested_groups,
            t!(
                "backup_ui.import.snippet_note.nested_groups",
                count = n.nested_groups
            ),
        ),
        (
            n.descriptions,
            t!(
                "backup_ui.import.snippet_note.descriptions",
                count = n.descriptions
            ),
        ),
        (
            n.decorations,
            t!(
                "backup_ui.import.snippet_note.decorations",
                count = n.decorations
            ),
        ),
        (
            skipped,
            t!("backup_ui.import.snippet_note.skipped", count = skipped),
        ),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(_, text)| text)
    .collect();
    lines.push(t!("backup_ui.import.snippet_note.duplicates"));
    lines
}

pub(super) fn open(window: &mut Window, cx: &mut App) -> Entity<ImportDialog> {
    open_with(ImportFrom::Client(Source::SshConfig), window, cx)
}

/// 打开导入对话框，选好来源 `from`。
pub(super) fn open_with(
    from: ImportFrom,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ImportDialog> {
    let view = cx.new(|cx| ImportDialog::new(from, window, cx));
    let rem = window.rem_size();
    let width = rems(50.)
        .to_pixels(rem)
        .min(window.viewport_size().width - rems(4.).to_pixels(rem));
    let dialog_view = view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        // 导入进行中不能关闭（关掉后看不到导入的结果）
        let importing = dialog_view.read(cx).busy == Some(Busy::Importing);
        // 在输入框中按 Enter：还没预览（或换了路径、要输入密码）时预览，看结果时关闭，其余时候不做什么
        let on_ok = {
            let view = dialog_view.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                view.update(cx, |view, cx| view.on_enter(window, cx))
            }
        };
        // 只有快捷命令的来源（NyaTerm）写“导入快捷命令”
        let title = if dialog_view.read(cx).from.snippets_only() {
            t!("backup_ui.import.title_snippets")
        } else {
            t!("backup_ui.import.title")
        };
        dialog
            .title(title)
            .w(width)
            .overlay_closable(false)
            .close_button(!importing)
            .keyboard(!importing)
            .child(dialog_view.clone())
            .on_ok(on_ok)
    });
    view
}

/// 导入来源：其他客户端，或 Termlet 自己的主机文件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ImportFrom {
    HostFile,
    Client(Source),
}

impl ImportFrom {
    /// 列表中的顺序。
    fn all() -> Vec<ImportFrom> {
        std::iter::once(ImportFrom::HostFile)
            .chain(Source::ALL.into_iter().map(ImportFrom::Client))
            .collect()
    }

    fn label(self) -> SharedString {
        match self {
            ImportFrom::HostFile => t!("backup_ui.import.source.hostfile"),
            ImportFrom::Client(Source::SshConfig) => t!("backup_ui.import.source.ssh_config"),
            ImportFrom::Client(Source::Xshell) => t!("backup_ui.import.source.xshell"),
            ImportFrom::Client(Source::FinalShell) => t!("backup_ui.import.source.finalshell"),
            ImportFrom::Client(Source::MobaXterm) => t!("backup_ui.import.source.mobaxterm"),
            ImportFrom::Client(Source::Tabby) => t!("backup_ui.import.source.tabby"),
            ImportFrom::Client(Source::Termora) => t!("backup_ui.import.source.termora"),
            ImportFrom::Client(Source::WindTerm) => t!("backup_ui.import.source.windterm"),
            ImportFrom::Client(Source::Electerm) => t!("backup_ui.import.source.electerm"),
            ImportFrom::Client(Source::Netcatty) => t!("backup_ui.import.source.netcatty"),
            ImportFrom::Client(Source::NyaTerm) => t!("backup_ui.import.source.nyaterm"),
        }
    }

    /// 来源中只有快捷命令（NyaTerm）。
    pub(super) fn snippets_only(self) -> bool {
        matches!(self, ImportFrom::Client(source) if source.snippets_only())
    }

    /// 来源自己的说明（位置、限制）。
    fn hint(self) -> Option<SharedString> {
        match self {
            ImportFrom::Client(Source::Xshell) => Some(t!("backup_ui.import.source_hint.xshell")),
            ImportFrom::Client(Source::FinalShell) => {
                Some(t!("backup_ui.import.source_hint.finalshell"))
            }
            ImportFrom::Client(Source::MobaXterm) => {
                Some(t!("backup_ui.import.source_hint.mobaxterm"))
            }
            ImportFrom::Client(Source::Tabby) => Some(t!("backup_ui.import.source_hint.tabby")),
            ImportFrom::Client(Source::Termora) => Some(t!("backup_ui.import.source_hint.termora")),
            ImportFrom::Client(Source::WindTerm) => {
                Some(t!("backup_ui.import.source_hint.windterm"))
            }
            ImportFrom::Client(Source::Electerm) => {
                Some(t!("backup_ui.import.source_hint.electerm"))
            }
            ImportFrom::Client(Source::Netcatty) => {
                Some(t!("backup_ui.import.source_hint.netcatty"))
            }
            ImportFrom::Client(Source::NyaTerm) => Some(t!("backup_ui.import.source_hint.nyaterm")),
            _ => None,
        }
    }

    /// 选择文件夹（其余选择文件）。
    fn picks_directory(self) -> bool {
        matches!(
            self,
            ImportFrom::Client(
                Source::Xshell | Source::FinalShell | Source::Termora | Source::WindTerm
            )
        )
    }

    /// 需要密码时密码框的提示与说明：主机文件是导出时设置的密码，WindTerm 是它的主密码。
    fn password_texts(self) -> (SharedString, SharedString) {
        match self {
            ImportFrom::Client(Source::WindTerm) => (
                t!("backup_ui.import.windterm_password_placeholder"),
                t!("backup_ui.import.windterm_password_hint"),
            ),
            _ => (
                t!("backup_ui.import.password_placeholder"),
                t!("backup_ui.import.password_hint"),
            ),
        }
    }

    /// 提示中来源的名称。
    fn source_name(self) -> &'static str {
        match self {
            ImportFrom::HostFile => "Termlet",
            ImportFrom::Client(source) => errors::source_name(source),
        }
    }
}

/// 读出的、等待导入的内容（其中的凭据界面取不出）。
enum Loaded {
    Plan(ImportPlan),
    HostFile(HostFile),
}

struct Preview {
    from: ImportFrom,
    loaded: Loaded,
    items: Vec<PreviewItem>,
    /// 键 → `items` 中的位置
    index: HashMap<String, usize>,
    /// 来源中跳过的会话数
    skipped: usize,
    /// 来源要用户同意才导入其中的秘密（Electerm、Netcatty），且确实带有秘密
    asks_for_secrets: bool,
}

impl Preview {
    fn new(from: ImportFrom, loaded: Loaded, items: Vec<PreviewItem>, skipped: usize) -> Self {
        let index = items
            .iter()
            .enumerate()
            .map(|(i, item)| (item.key.clone(), i))
            .collect();
        let asks_for_secrets = match &loaded {
            Loaded::Plan(plan) => plan.source.secrets_need_consent() && plan.has_secrets(),
            Loaded::HostFile(_) => false,
        };
        Self {
            from,
            loaded,
            items,
            index,
            skipped,
            asks_for_secrets,
        }
    }
}

/// 预览中一行主机下面的说明，每项占一行。
#[derive(Clone, Debug, PartialEq)]
enum Detail {
    Jump(String),
    Hint(SharedString),
    ProxyCommand(String),
    Startup(String),
    ForwardsHeader { count: usize, merge: bool },
    Forward(PreviewForward),
}

/// 一台主机的说明：跳板机、提醒、代理命令、登录后执行的命令、随主机导入的端口转发（跳过已有的主机时不导入，不列出）。
fn details(item: &PreviewItem, source: &str, on_duplicate: OnDuplicate) -> Vec<Detail> {
    let mut lines = Vec::new();
    if let Some(jump) = &item.jump {
        lines.push(Detail::Jump(jump.clone()));
    }
    lines.extend(
        item.hints
            .iter()
            .map(|h| Detail::Hint(errors::describe_hint(h, source))),
    );
    if let Some(command) = &item.proxy_command {
        lines.push(Detail::ProxyCommand(command.clone()));
    }
    if let Some(command) = &item.startup {
        lines.push(Detail::Startup(command.clone()));
    }
    let merge = item.duplicate_of.is_some();
    if !item.forwards.is_empty() && (!merge || on_duplicate == OnDuplicate::Merge) {
        lines.push(Detail::ForwardsHeader {
            count: item.forwards.len(),
            merge,
        });
        lines.extend(item.forwards.iter().cloned().map(Detail::Forward));
    }
    lines
}

/// 一行的高度（rem）。
fn row_height(details: usize) -> f32 {
    ROW_PADDING + MAIN_LINE + details as f32 * DETAIL_LINE
}

/// 地址一栏：`用户名@地址:端口`（没有用户名时省略），Telnet、串口带协议。
fn address_text(item: &PreviewItem) -> String {
    let endpoint = join_host_port(&item.address, item.port);
    let target = if item.username.is_empty() {
        endpoint
    } else {
        format!("{}@{endpoint}", item.username)
    };
    match item.protocol {
        Protocol::Ssh => target,
        Protocol::Telnet => t!("hosts.summary.telnet", target = target).to_string(),
        Protocol::Serial => t!("hosts.summary.serial", port = item.address.clone()).to_string(),
    }
}

/// 端口转发的路线（与侧边栏的写法相同）。
fn forward_route(f: &PreviewForward) -> SharedString {
    let bind = join_host_port(&f.bind_host, f.bind_port);
    let target = join_host_port(&f.target_host, f.target_port);
    match f.kind {
        ForwardKind::Local => format!("{bind} → {target}").into(),
        ForwardKind::Remote => t!("forward.route_remote", bind = bind, target = target),
        ForwardKind::Dynamic => format!("SOCKS5 {bind}").into(),
    }
}

fn forward_kind(kind: ForwardKind) -> SharedString {
    match kind {
        ForwardKind::Local => t!("forward.kind_local"),
        ForwardKind::Remote => t!("forward.kind_remote"),
        ForwardKind::Dynamic => t!("forward.kind_dynamic"),
    }
}

pub(super) struct ImportDialog {
    from: ImportFrom,
    source_select: Entity<ChoiceState<ImportFrom>>,
    path: Entity<InputState>,
    /// 程序填入的路径（默认位置、浏览的结果）：输入框随后的变化是它时不算用户编辑
    filled: Option<SharedString>,
    /// 用户自己填写或选择了路径：默认位置晚到时不再覆盖
    path_edited: bool,
    password: Entity<InputState>,
    /// Termlet 的主机文件已加密：显示密码框
    needs_password: bool,
    preview: Option<Preview>,
    /// 预览时的路径：之后改了路径，按 Enter 重新预览
    previewed_path: Option<String>,
    query: Entity<InputState>,
    status: StatusFilter,
    selected: HashSet<String>,
    /// 上次单击的行：Shift 连续勾选的起点
    anchor: Option<String>,
    on_duplicate: OnDuplicate,
    /// 用户勾选了“导入保存的密码”（密码明文保存的来源；每次预览后重新默认不勾选）
    import_secrets: bool,
    /// 正在读取来源或导入
    busy: Option<Busy>,
    /// 读取的序号：切换来源、重新读取后，之前还没返回的结果丢弃
    seq: u64,
    error: Option<SharedString>,
    report: Option<ImportReport>,
    scroll: VirtualListScrollHandle,
    _task: Option<Task<()>>,
    _default_path: Option<Task<()>>,
    _browse: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ImportDialog {
    pub(super) fn new(from: ImportFrom, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let choices = ImportFrom::all()
            .into_iter()
            .map(|f| Choice::new(f.label(), f))
            .collect();
        let source_select = choice_state(choices, &from, false, window, cx);
        let path = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("backup_ui.import.path_placeholder"))
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("backup_ui.import.password_placeholder"))
        });
        let query =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("backup_ui.import.search")));
        let subscriptions = vec![
            cx.subscribe_in(
                &source_select,
                window,
                |this, _, event: &SelectEvent<_>, window, cx| {
                    let SelectEvent::Confirm(Some(from)) = event else {
                        return;
                    };
                    this.switch_source(*from, window, cx);
                },
            ),
            cx.subscribe(&path, |this, state, event: &InputEvent, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                let value = state.read(cx).value();
                if this.filled.as_ref() != Some(&value) {
                    this.path_edited = true;
                    this.filled = None;
                }
                // 换了路径：上一次的预览不再对应这个文件，不能再按它导入
                if this.busy != Some(Busy::Importing)
                    && this.previewed_path.as_deref() != Some(value.trim())
                {
                    this.clear_preview();
                }
                this.needs_password = false;
                this.error = None;
                cx.notify();
            }),
            cx.subscribe(&query, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let mut this = Self {
            from,
            source_select,
            path,
            filled: None,
            path_edited: false,
            password,
            needs_password: false,
            preview: None,
            previewed_path: None,
            query,
            status: StatusFilter::All,
            selected: HashSet::new(),
            anchor: None,
            on_duplicate: OnDuplicate::Skip,
            import_secrets: false,
            busy: None,
            seq: 0,
            error: None,
            report: None,
            scroll: VirtualListScrollHandle::new(),
            _task: None,
            _default_path: None,
            _browse: None,
            _subscriptions: subscriptions,
        };
        this.load_default_path(window, cx);
        this
    }

    /// 切换来源：清空路径与预览，再填入本机的默认位置。
    pub(super) fn switch_source(
        &mut self,
        from: ImportFrom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if from == self.from {
            return;
        }
        self.from = from;
        self.seq += 1;
        self.busy = None;
        self.preview = None;
        self.previewed_path = None;
        self.needs_password = false;
        self.error = None;
        let (placeholder, _) = from.password_texts();
        self.password.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.set_placeholder(placeholder, window, cx);
        });
        self.fill_path(String::new(), window, cx);
        self.load_default_path(window, cx);
        cx.notify();
    }

    /// 在后台查找来源的默认位置（查文件、注册表），找到且用户还没填写时填入。
    fn load_default_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path_edited = false;
        let ImportFrom::Client(source) = self.from else {
            return;
        };
        let seq = self.seq;
        self._default_path = Some(cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_spawn(async move { source.default_path() })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.seq != seq || this.path_edited {
                    return;
                }
                if let Some(path) = found {
                    this.fill_path(path.display().to_string(), window, cx);
                }
            });
        }));
    }

    fn fill_path(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.filled = Some(text.clone().into());
        self.path
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let directories = self.from.picks_directory();
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: !directories,
            directories,
            multiple: false,
            prompt: None,
        });
        self._browse = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.path_edited = true;
                this.fill_path(path.display().to_string(), window, cx);
                this.needs_password = false;
                this.start_preview(window, cx);
            });
        }));
    }

    /// 对话框收到 Enter；返回是否关闭对话框。
    fn on_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.report.is_some() {
            return true;
        }
        let path = self.path.read(cx).value().trim().to_string();
        if self.preview.is_none()
            || self.needs_password
            || self.previewed_path.as_deref() != Some(path.as_str())
        {
            self.start_preview(window, cx);
        }
        false
    }

    fn can_preview(&self, cx: &App) -> bool {
        self.busy.is_none()
            && !self.path.read(cx).value().trim().is_empty()
            && (!self.needs_password || !self.password.read(cx).value().is_empty())
    }

    /// 读取来源（在核心的运行时上），读完后列出预览。
    fn start_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_preview(cx) {
            return;
        }
        let text = self.path.read(cx).value().trim().to_string();
        let path = PathBuf::from(&text);
        self.seq += 1;
        let seq = self.seq;
        self.busy = Some(Busy::Reading);
        self.error = None;
        let from = self.from;
        tracing::info!(from = ?from, path = %path.display(), "导入主机：读取预览");
        let store = AppState::store(cx);
        let password = Some(self.password.read(cx).value().to_string())
            .filter(|p| self.needs_password && !p.is_empty())
            .map(SecretString::new);
        let job: Pin<Box<dyn Future<Output = Result<Preview, Failure>>>> = match from {
            ImportFrom::Client(source) => {
                // 读取与比对现有主机都在核心的运行时上做（来源中的主机多时比对也不慢）
                let job = runtime::spawn(async move {
                    let plan = import::read_with_password(source, path, password).await?;
                    let preview = import::preview(&store, &plan);
                    Ok::<_, import::ImportError>((plan, preview))
                });
                Box::pin(async move {
                    match job.await {
                        Ok(Ok((plan, preview))) => Ok(Preview::new(
                            from,
                            Loaded::Plan(plan),
                            preview.items,
                            preview.skipped,
                        )),
                        Ok(Err(import::ImportError::WindTermPasswordRequired)) => {
                            Err(Failure::PasswordRequired)
                        }
                        Ok(Err(e)) => Err(Failure::Message(errors::describe_import(&e))),
                        Err(e) => Err(Failure::Message(e.to_string().into())),
                    }
                })
            }
            ImportFrom::HostFile => {
                let job = runtime::spawn(async move {
                    let file = hostfile::read(path, password).await?;
                    let preview = hostfile::preview(&store, &file);
                    Ok::<_, BackupError>((file, preview))
                });
                Box::pin(async move {
                    match job.await {
                        Ok(Ok((file, preview))) => Ok(Preview::new(
                            from,
                            Loaded::HostFile(file),
                            preview.items,
                            preview.skipped,
                        )),
                        Ok(Err(BackupError::PasswordRequired)) => Err(Failure::PasswordRequired),
                        Ok(Err(e)) => Err(Failure::Message(errors::describe_backup(&e))),
                        Err(e) => Err(Failure::Message(e.to_string().into())),
                    }
                })
            }
        };
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.seq != seq {
                    return;
                }
                this.busy = None;
                match result {
                    Ok(preview) => this.show(preview, text, window, cx),
                    Err(Failure::PasswordRequired) => {
                        this.clear_preview();
                        this.needs_password = true;
                        this.error = None;
                    }
                    Err(Failure::Message(message)) => {
                        this.clear_preview();
                        this.error = Some(message);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 显示预览：默认勾选新主机，清空搜索与筛选。
    fn show(
        &mut self,
        preview: Preview,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = select::default_selection(&preview.items);
        self.anchor = None;
        self.status = StatusFilter::All;
        self.import_secrets = false;
        self.preview = Some(preview);
        self.previewed_path = Some(path);
        self.query
            .update(cx, |state, cx| state.set_value("", window, cx));
    }

    /// 丢掉预览（换了路径、读取失败）：“导入”按钮随之不可用。
    fn clear_preview(&mut self) {
        self.preview = None;
        self.previewed_path = None;
        self.selected.clear();
        self.anchor = None;
    }

    /// 当前筛选出的主机在 `items` 中的位置（按显示顺序）。
    fn visible(&self, cx: &App) -> Vec<usize> {
        let Some(preview) = &self.preview else {
            return Vec::new();
        };
        let query = self.query.read(cx).value();
        select::filter(&preview.items, &query, self.status)
            .into_iter()
            .filter_map(|item| preview.index.get(&item.key).copied())
            .collect()
    }

    fn bulk(&mut self, action: BulkAction, cx: &mut Context<Self>) {
        let Some(preview) = &self.preview else {
            return;
        };
        let query = self.query.read(cx).value();
        let visible = select::filter(&preview.items, &query, self.status);
        select::bulk_select(&mut self.selected, &visible, action);
        cx.notify();
    }

    /// 单击一行（`shift`：按住 Shift）。
    fn click_row(&mut self, key: &str, shift: bool, cx: &mut Context<Self>) {
        let Some(preview) = &self.preview else {
            return;
        };
        let query = self.query.read(cx).value();
        let visible = select::filter(&preview.items, &query, self.status);
        let keys: Vec<&str> = visible.iter().map(|item| item.key.as_str()).collect();
        select::click(
            &mut self.selected,
            &keys,
            self.anchor.as_deref(),
            key,
            shift,
        );
        self.anchor = Some(key.to_string());
        cx.notify();
    }

    /// 预览的来源要用户同意才导入其中的秘密（Electerm、Netcatty），且确实带有秘密。
    fn asks_for_secrets(&self) -> bool {
        self.preview.as_ref().is_some_and(|p| p.asks_for_secrets)
    }

    /// 来源中的秘密会一并导入：不需要同意的来源，或用户勾选了“导入保存的密码”。
    fn imports_secrets(&self) -> bool {
        !self.asks_for_secrets() || self.import_secrets
    }

    /// 勾选的主机（按来源中的顺序）。
    fn chosen(&self) -> Vec<String> {
        self.preview
            .as_ref()
            .map(|p| {
                p.items
                    .iter()
                    .filter(|item| self.selected.contains(&item.key))
                    .map(|item| item.key.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 只有快捷命令的预览（NyaTerm）中的快捷命令；不是这种预览时为 `None`。
    fn snippets_only(&self) -> Option<&ImportPlan> {
        match &self.preview.as_ref()?.loaded {
            Loaded::Plan(plan) if plan.source.snippets_only() => Some(plan),
            _ => None,
        }
    }

    /// 可以导入：勾选了主机，或只有快捷命令的预览中有快捷命令。
    fn can_apply(&self) -> bool {
        self.busy.is_none()
            && match self.snippets_only() {
                Some(plan) => !plan.snippets.is_empty(),
                None => !self.chosen().is_empty(),
            }
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_apply() {
            return;
        }
        let chosen = self.chosen();
        let Some(preview) = &self.preview else {
            return;
        };
        let backups = super::backups(cx);
        let on_duplicate = self.on_duplicate;
        tracing::info!(
            chosen = chosen.len(),
            on_duplicate = ?on_duplicate,
            from = ?preview.from,
            "导入主机"
        );
        let imports_secrets = self.imports_secrets();
        let job = match &preview.loaded {
            Loaded::Plan(plan) => {
                // 没有同意导入保存的密码：去掉来源中的秘密，只导入主机
                let plan = if imports_secrets {
                    plan.clone()
                } else {
                    plan.without_secrets()
                };
                runtime::spawn(import::apply(backups, plan, chosen, on_duplicate))
            }
            Loaded::HostFile(file) => runtime::spawn(hostfile::import(
                backups,
                file.clone(),
                chosen,
                on_duplicate,
            )),
        };
        self.busy = Some(Busy::Importing);
        self.error = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(Ok(report)) => this.report = Some(report),
                    Ok(Err(e)) => this.error = Some(errors::describe_backup(&e)),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------ 渲染

    fn render_source(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div().w(rems(19.)).flex_none().child(
                            Select::new(&self.source_select)
                                .id("import-source")
                                .disabled(self.busy.is_some()),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.path).id("import-path")),
                    )
                    .child(
                        Button::new("import-browse")
                            .outline()
                            .icon(IconName::FolderOpen)
                            .label(t!("backup_ui.import.browse"))
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx))),
                    )
                    .child(
                        Button::new("import-preview")
                            .label(t!("backup_ui.import.preview"))
                            .loading(self.busy == Some(Busy::Reading))
                            .disabled(!self.can_preview(cx))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.start_preview(window, cx)),
                            ),
                    ),
            )
            .when_some(self.from.hint(), |this, hint| {
                this.child(div().text_xs().text_color(muted).child(prose(hint)))
            })
            .when(self.needs_password, |this| {
                let (_, hint) = self.from.password_texts();
                this.child(
                    h_flex()
                        .gap_2()
                        .child(
                            div().w(rems(18.)).flex_none().child(
                                Input::new(&self.password)
                                    .id("import-password")
                                    .mask_toggle(),
                            ),
                        )
                        .child(div().text_xs().text_color(muted).child(prose(hint))),
                )
            })
    }

    fn render_toolbar(
        &self,
        preview: &Preview,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let count = |status| select::filter(&preview.items, "", status).len();
        let filters = [
            (
                StatusFilter::All,
                t!(
                    "backup_ui.import.filter.all",
                    count = count(StatusFilter::All)
                ),
            ),
            (
                StatusFilter::New,
                t!(
                    "backup_ui.import.filter.new",
                    count = count(StatusFilter::New)
                ),
            ),
            (
                StatusFilter::Duplicate,
                t!(
                    "backup_ui.import.filter.duplicate",
                    count = count(StatusFilter::Duplicate)
                ),
            ),
        ];
        let status_ix = filters
            .iter()
            .position(|(s, _)| *s == self.status)
            .unwrap_or(0);
        let statuses: Vec<StatusFilter> = filters.iter().map(|(s, _)| *s).collect();
        let empty = self.visible(cx).is_empty();
        let bulk =
            |id: &'static str, label: SharedString, action: BulkAction, cx: &Context<Self>| {
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .label(label)
                    .disabled(empty)
                    .on_click(cx.listener(move |this, _, _, cx| this.bulk(action, cx)))
            };
        h_flex()
            .gap_2()
            .child(
                div().w(rems(15.)).flex_none().child(
                    Input::new(&self.query)
                        .small()
                        .cleanable(true)
                        .prefix(
                            Icon::new(IconName::Search)
                                .xsmall()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .id("import-search"),
                ),
            )
            .child(
                TabBar::new("import-filter")
                    .segmented()
                    .small()
                    .selected_index(status_ix)
                    .on_click(cx.listener(move |this, ix: &usize, _, cx| {
                        if let Some(status) = statuses.get(*ix) {
                            this.status = *status;
                            cx.notify();
                        }
                    }))
                    .children(
                        filters
                            .into_iter()
                            .map(|(_, label)| Tab::new().label(label)),
                    ),
            )
            .child(div().flex_1())
            .child(bulk(
                "import-select-all",
                t!("backup_ui.import.select.all"),
                BulkAction::All,
                cx,
            ))
            .child(bulk(
                "import-select-none",
                t!("backup_ui.import.select.none"),
                BulkAction::None,
                cx,
            ))
            .child(bulk(
                "import-select-new",
                t!("backup_ui.import.select.new"),
                BulkAction::New,
                cx,
            ))
            .child(bulk(
                "import-select-invert",
                t!("backup_ui.import.select.invert"),
                BulkAction::Invert,
                cx,
            ))
    }

    /// 第一行的几栏：表头与各行共用，列宽相同。
    fn columns(
        name: AnyElement,
        address: AnyElement,
        group: AnyElement,
        status: AnyElement,
    ) -> impl IntoElement {
        h_flex()
            .w_full()
            .h(rems(MAIN_LINE))
            .gap_2()
            .text_xs()
            .child(div().flex_1().min_w_0().truncate().child(name))
            .child(div().flex_1().min_w_0().truncate().child(address))
            .child(div().w(rems(8.)).flex_none().truncate().child(group))
            .child(
                h_flex()
                    .w(rems(9.))
                    .flex_none()
                    .gap_1p5()
                    .overflow_hidden()
                    .child(status),
            )
    }

    fn render_header(&self, visible: &[usize], cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let all_on = !visible.is_empty()
            && self.preview.as_ref().is_some_and(|p| {
                visible
                    .iter()
                    .all(|&ix| self.selected.contains(&p.items[ix].key))
            });
        let label = |text: SharedString| div().child(prose(text)).into_any_element();
        div()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.list_head)
            .text_color(theme.muted_foreground)
            .font_medium()
            .child(
                Checkbox::new("import-check-visible")
                    .w_full()
                    .checked(all_on)
                    .disabled(visible.is_empty())
                    .accessibility_label(t!("backup_ui.import.select.all"))
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        let action = if *checked {
                            BulkAction::All
                        } else {
                            BulkAction::None
                        };
                        this.bulk(action, cx);
                    }))
                    .child(Self::columns(
                        label(t!("backup_ui.import.column.name")),
                        label(t!("backup_ui.import.column.address")),
                        label(t!("backup_ui.import.column.group")),
                        label(t!("backup_ui.import.column.status")),
                    )),
            )
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(preview) = &self.preview else {
            return div().into_any_element();
        };
        let item = &preview.items[ix];
        let theme = cx.theme();
        let (muted, warning, danger, success) = (
            theme.muted_foreground,
            theme.warning,
            theme.danger,
            theme.success,
        );
        let hover = theme.list_hover;
        let key = item.key.clone();
        let on = self.selected.contains(&key);
        let duplicate = item.duplicate_of.is_some();
        let lines = details(item, preview.from.source_name(), self.on_duplicate);
        // 不导入来源中的秘密时不标“带密码 / 带私钥”
        let secrets = self.imports_secrets();
        let group = if item.group.is_empty() {
            "—".to_string()
        } else {
            item.group.join(" / ")
        };
        let status = h_flex()
            .gap_1p5()
            .child(
                div()
                    .flex_none()
                    .text_color(if duplicate { warning } else { success })
                    .child(prose(if duplicate {
                        t!("backup_ui.import.status.duplicate")
                    } else {
                        t!("backup_ui.import.status.new")
                    })),
            )
            .when(secrets && item.has_password, |this| {
                this.child(
                    div()
                        .flex_none()
                        .text_color(muted)
                        .child(t!("backup_ui.import.has_password")),
                )
            })
            .when(secrets && item.has_key, |this| {
                this.child(
                    div()
                        .flex_none()
                        .text_color(muted)
                        .child(t!("backup_ui.import.has_key")),
                )
            });
        let detail_rows = lines.into_iter().enumerate().map(|(n, line)| {
            let line_id = super::element_id("import-detail", format!("{key}-{n}"));
            let base = div()
                .id(line_id)
                .h(rems(DETAIL_LINE))
                .w_full()
                .min_w_0()
                .text_xs();
            // 一行放不下时悬停显示全文
            let with_tip = |base: gpui_kit::Stateful<gpui_kit::Div>,
                            text: SharedString,
                            color: gpui_kit::Hsla| {
                let tip = text.clone();
                base.truncate()
                    .text_color(color)
                    .child(prose(text))
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .into_any_element()
            };
            match line {
                Detail::Jump(name) => base
                    .truncate()
                    .text_color(muted)
                    .child(t!("backup_ui.import.via", name = name))
                    .into_any_element(),
                Detail::Hint(text) => with_tip(base, text, warning),
                Detail::ProxyCommand(command) => with_tip(
                    base,
                    t!("backup_ui.import.proxy_command", command = command),
                    warning,
                ),
                Detail::Startup(command) => with_tip(
                    base,
                    t!("backup_ui.import.startup", command = command),
                    warning,
                ),
                Detail::ForwardsHeader { count, merge } => base
                    .truncate()
                    .text_color(muted)
                    .child(prose(if merge {
                        t!("backup_ui.import.forwards_merge", count = count)
                    } else {
                        t!("backup_ui.import.forwards", count = count)
                    }))
                    .into_any_element(),
                Detail::Forward(f) => {
                    let exposed = f.exposed.then(|| {
                        if f.kind == ForwardKind::Remote {
                            t!("backup_ui.import.forward_remote_exposed")
                        } else {
                            t!("backup_ui.import.forward_exposed")
                        }
                    });
                    let mut tip = t!(
                        "backup_ui.import.forward_line",
                        kind = forward_kind(f.kind),
                        route = forward_route(&f)
                    )
                    .to_string();
                    if !f.name.is_empty() {
                        tip.push_str(" · ");
                        tip.push_str(&f.name);
                    }
                    let line_text: SharedString = tip.clone().into();
                    if let Some(exposed) = &exposed {
                        tip.push('\n');
                        tip.push_str(exposed);
                    }
                    let tip: SharedString = tip.into();
                    base.child(
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(div().min_w_0().truncate().child(line_text))
                            .when_some(exposed, |this, exposed| {
                                this.child(
                                    div()
                                        .flex_shrink_0()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(danger)
                                        .child(exposed),
                                )
                            })
                            .when(f.auto_start, |this| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_color(warning)
                                        .child(t!("backup_ui.import.forward_auto_start")),
                                )
                            }),
                    )
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .into_any_element()
                }
            }
        });
        let click_key = key.clone();
        Checkbox::new(super::element_id("import-row", &key))
            .size_full()
            .px_2()
            .py_1()
            .checked(on)
            .accessibility_label(item.name.clone())
            .hover(move |style| style.bg(hover))
            .on_change(cx.listener(move |this, _: &bool, window, cx| {
                this.click_row(&click_key, window.modifiers().shift, cx)
            }))
            .child(
                v_flex()
                    .w_full()
                    .child(Self::columns(
                        div().child(item.name.clone()).into_any_element(),
                        div()
                            .text_color(muted)
                            .child(address_text(item))
                            .into_any_element(),
                        div().text_color(muted).child(group).into_any_element(),
                        status.into_any_element(),
                    ))
                    .children(detail_rows),
            )
            .into_any_element()
    }

    fn render_list(
        &self,
        preview: &Preview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (border, radius, muted) = (theme.border, theme.radius, theme.muted_foreground);
        let visible = self.visible(cx);
        let empty_text = if preview.items.is_empty() {
            Some(t!("backup_ui.import.none"))
        } else if visible.is_empty() {
            Some(t!("backup_ui.import.no_match"))
        } else {
            None
        };
        let header = self.render_header(&visible, cx);
        let body = match empty_text {
            Some(text) => div()
                .p_3()
                .text_xs()
                .text_color(muted)
                .child(prose(text))
                .into_any_element(),
            None => {
                let rem = window.rem_size();
                let sizes = Rc::new(
                    visible
                        .iter()
                        .map(|&ix| {
                            let lines = details(
                                &preview.items[ix],
                                preview.from.source_name(),
                                self.on_duplicate,
                            )
                            .len();
                            size(px(0.), rems(row_height(lines)).to_pixels(rem))
                        })
                        .collect::<Vec<_>>(),
                );
                let visible = Rc::new(visible);
                v_virtual_list(
                    cx.entity(),
                    "import-rows",
                    sizes,
                    move |this, range, _, cx| {
                        range
                            .filter_map(|i| visible.get(i).copied())
                            .map(|ix| this.render_row(ix, cx))
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&self.scroll)
                .size_full()
                .into_any_element()
            }
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .border_1()
            .border_color(border)
            .rounded(radius)
            .overflow_hidden()
            .child(header)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .vertical_scrollbar(&self.scroll),
            )
    }

    fn render_options(
        &self,
        preview: &Preview,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        let total = preview.items.len();
        let picked = self.chosen().len();
        let host_file = match &preview.loaded {
            Loaded::HostFile(file) => Some(t!(
                "backup_ui.import.hostfile_info",
                time = termlet_core::time::format_local(file.exported_at()),
                app = file.app().to_string(),
                count = file.host_count()
            )),
            Loaded::Plan(_) => None,
        };
        let snippets = match &preview.loaded {
            Loaded::Plan(plan) => plan.snippets.len(),
            Loaded::HostFile(_) => 0,
        };
        const MODES: [OnDuplicate; 2] = [OnDuplicate::Skip, OnDuplicate::Merge];
        v_flex()
            .gap_1p5()
            .text_xs()
            .text_color(muted)
            .child(
                h_flex()
                    .gap_3()
                    .child(div().id("import-selected-count").test_support().child(t!(
                        "backup_ui.import.selected_count",
                        selected = picked,
                        total = total
                    )))
                    .when(total > 1, |this| {
                        this.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(prose(t!("backup_ui.import.shift_hint"))),
                        )
                    }),
            )
            .when(preview.skipped > 0, |this| {
                this.child(prose(t!(
                    "backup_ui.import.skipped_other",
                    count = preview.skipped
                )))
            })
            .when(snippets > 0, |this| {
                this.child(
                    div()
                        .id("import-snippets")
                        .test_support()
                        .child(prose(t!("backup_ui.import.snippets", count = snippets))),
                )
            })
            .when_some(host_file, |this, info| this.child(info))
            .when_some(self.secrets_option(preview, cx), |this, option| {
                this.child(option)
            })
            .child(
                h_flex()
                    .gap_3()
                    .child(t!("backup_ui.import.on_duplicate"))
                    .child(
                        RadioGroup::horizontal("import-on-duplicate")
                            .children([t!("backup_ui.import.skip"), t!("backup_ui.import.merge")])
                            .selected_index(MODES.iter().position(|m| *m == self.on_duplicate))
                            .disabled(self.busy.is_some())
                            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                                if let Some(mode) = MODES.get(*ix) {
                                    this.on_duplicate = *mode;
                                    cx.notify();
                                }
                            })),
                    ),
            )
    }

    /// “导入保存的密码”与来源的说明（只在要用户同意、且来源带有秘密时）。
    fn secrets_option(&self, preview: &Preview, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.asks_for_secrets() {
            return None;
        }
        let hint = match preview.from {
            ImportFrom::Client(Source::Netcatty) => t!("backup_ui.import.secrets_hint.netcatty"),
            ImportFrom::Client(Source::FinalShell) => {
                t!("backup_ui.import.secrets_hint.finalshell")
            }
            _ => t!("backup_ui.import.secrets_hint.electerm"),
        };
        Some(
            v_flex()
                .gap_1()
                .child(
                    Checkbox::new("import-secrets")
                        .label(t!("backup_ui.import.import_secrets"))
                        .checked(self.import_secrets)
                        .disabled(self.busy.is_some())
                        .on_change(cx.listener(|this, checked: &bool, _, cx| {
                            this.import_secrets = *checked;
                            cx.notify();
                        })),
                )
                .child(div().pl_6().child(prose(hint)))
                .into_any_element(),
        )
    }

    /// 只有快捷命令的预览：列表（分组、名称、内容的第一行）与转换时丢掉或改动的内容。不用勾选，全部导入。
    fn render_snippets(
        &self,
        plan: &ImportPlan,
        skipped: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (border, radius, muted) = (theme.border, theme.radius, theme.muted_foreground);
        let mono = theme.mono_font_family.clone();
        let rows: Rc<Vec<SnippetRow>> =
            Rc::new(plan.snippets.iter().map(SnippetRow::new).collect());
        let body = if rows.is_empty() {
            div()
                .p_3()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("backup_ui.import.snippets_none")))
                .into_any_element()
        } else {
            let height = rems(SNIPPET_ROW).to_pixels(window.rem_size());
            let sizes = Rc::new(vec![size(px(0.), height); rows.len()]);
            let ungrouped = t!("snippets.ungrouped");
            v_virtual_list(
                cx.entity(),
                "import-snippet-rows",
                sizes,
                move |_, range, _, _| {
                    range
                        .filter_map(|i| rows.get(i))
                        .map(|row| {
                            Self::snippet_columns(
                                div()
                                    .text_color(muted)
                                    .child(if row.group.is_empty() {
                                        ungrouped.clone()
                                    } else {
                                        row.group.clone()
                                    })
                                    .into_any_element(),
                                div().child(row.name.clone()).into_any_element(),
                                h_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(mono.clone())
                                            .child(row.first_line.clone()),
                                    )
                                    .when(row.lines > 1, |this| {
                                        this.child(div().flex_none().text_color(muted).child(t!(
                                            "backup_ui.import.snippet_lines",
                                            count = row.lines
                                        )))
                                    })
                                    .into_any_element(),
                            )
                            .h(rems(SNIPPET_ROW))
                            .px_2()
                            .border_b_1()
                            .border_color(border)
                            .into_any_element()
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };
        let header = Self::snippet_columns(
            div()
                .child(t!("backup_ui.import.column.group"))
                .into_any_element(),
            div()
                .child(t!("backup_ui.import.column.name"))
                .into_any_element(),
            div()
                .child(t!("backup_ui.import.column.content"))
                .into_any_element(),
        )
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(border)
        .bg(theme.list_head)
        .text_color(muted)
        .font_medium();
        let notes = snippet_notes(&plan.snippet_notes, skipped);
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(
                div()
                    .id("import-snippet-count")
                    .test_support()
                    .text_xs()
                    .text_color(muted)
                    .child(t!(
                        "backup_ui.import.snippet_count",
                        count = plan.snippets.len()
                    )),
            )
            .child(
                v_flex()
                    .id("import-snippets-list")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .border_1()
                    .border_color(border)
                    .rounded(radius)
                    .overflow_hidden()
                    .child(header)
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(body)
                            .vertical_scrollbar(&self.scroll),
                    ),
            )
            .child(
                v_flex()
                    .id("import-snippet-notes")
                    .test_support()
                    .gap_0p5()
                    .text_xs()
                    .text_color(muted)
                    .children(notes.into_iter().map(|note| div().child(prose(note)))),
            )
    }

    /// 快捷命令列表的三栏：分组、名称、内容。表头与各行共用，列宽相同。
    fn snippet_columns(group: AnyElement, name: AnyElement, content: AnyElement) -> gpui_kit::Div {
        h_flex()
            .w_full()
            .gap_2()
            .text_xs()
            .child(div().w(rems(9.)).flex_none().truncate().child(group))
            .child(div().w(rems(11.)).flex_none().truncate().child(name))
            .child(div().flex_1().min_w_0().child(content))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        // 只有快捷命令的来源：还没预览时也按“条”写
        let label = if self.from.snippets_only() {
            t!(
                "backup_ui.import.apply_snippets",
                count = self.snippets_only().map_or(0, |plan| plan.snippets.len())
            )
        } else {
            t!("backup_ui.import.apply", count = self.chosen().len())
        };
        h_flex()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("import-error")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.danger)
                    .when_some(self.error.clone(), |this, error| this.child(prose(error))),
            )
            .child(
                Button::new("import-cancel")
                    .label(t!("common.cancel"))
                    .disabled(self.busy == Some(Busy::Importing))
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("import-apply")
                    .primary()
                    .label(label)
                    .loading(self.busy == Some(Busy::Importing))
                    .disabled(!self.can_apply())
                    .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
            )
    }

    fn render_report(
        &self,
        report: &ImportReport,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let failed = report.failed();
        let snippets_only = self.from.snippets_only();
        let failures: Vec<SharedString> = report
            .failures
            .iter()
            .map(|f| errors::describe_failure(&f.subject, &f.reason))
            .collect();
        let has_failures = !failures.is_empty();
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        Icon::new(IconName::CircleCheck)
                            .small()
                            .text_color(theme.success),
                    )
                    .child(
                        div()
                            .id("import-done")
                            .test_support()
                            .child(prose(if snippets_only {
                                t!(
                                    "backup_ui.import.snippets_only_done",
                                    count = report.snippets
                                )
                            } else {
                                t!(
                                    "backup_ui.import.done",
                                    created = report.created,
                                    merged = report.merged,
                                    skipped = report.skipped
                                )
                            })),
                    ),
            )
            .when(report.snippets > 0 && !snippets_only, |this| {
                this.child(div().text_sm().child(prose(t!(
                    "backup_ui.import.snippets_done",
                    count = report.snippets
                ))))
            })
            .when(report.snippets_skipped > 0, |this| {
                this.child(
                    div()
                        .id("import-snippets-skipped")
                        .test_support()
                        .text_sm()
                        .child(prose(t!(
                            "backup_ui.import.snippets_skipped",
                            count = report.snippets_skipped
                        ))),
                )
            })
            .when(report.passwords_filled > 0, |this| {
                this.child(
                    div()
                        .id("import-passwords-filled")
                        .test_support()
                        .text_sm()
                        .child(prose(t!(
                            "backup_ui.import.passwords_filled",
                            count = report.passwords_filled
                        ))),
                )
            })
            .when(failed > 0, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.warning)
                        .child(prose(t!("backup_ui.import.failed_hosts", count = failed))),
                )
            })
            .when(has_failures, |this| {
                this.child(div().text_sm().child(prose(t!(
                    "backup_ui.import.failures",
                    count = failures.len()
                ))))
                .child(
                    v_flex()
                        .id("import-failures")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .gap_1()
                        .p_2()
                        .border_1()
                        .border_color(theme.border)
                        .rounded(theme.radius)
                        .text_xs()
                        .text_color(theme.danger)
                        .children(failures.into_iter().map(|f| div().child(f))),
                )
            })
            .when(!has_failures, |this| this.child(div().flex_1()))
            .child(
                h_flex()
                    .justify_end()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("import-close")
                            .primary()
                            .label(t!("common.close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    }
}

#[cfg(test)]
impl ImportDialog {
    /// 勾选的主机（排好序）。
    pub(super) fn selected_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.selected.iter().cloned().collect();
        keys.sort();
        keys
    }

    /// 预览中的主机数；还没有预览时为 `None`。
    pub(super) fn item_count(&self) -> Option<usize> {
        self.preview.as_ref().map(|p| p.items.len())
    }

    /// 正在显示的错误提示。
    pub(super) fn error_text(&self) -> Option<SharedString> {
        self.error.clone()
    }

    /// 导入的结果：新建、合并、跳过、没能导入的主机数。
    pub(super) fn report_counts(&self) -> Option<(usize, usize, usize, usize)> {
        self.report
            .as_ref()
            .map(|r| (r.created, r.merged, r.skipped, r.failed()))
    }

    /// 当前的来源。
    pub(super) fn source(&self) -> ImportFrom {
        self.from
    }

    /// 只有快捷命令的预览：各条的名称、转换说明、跳过的条数；不是这种预览时为 `None`。
    pub(super) fn snippet_preview(&self) -> Option<(Vec<String>, SnippetNotes, usize)> {
        let plan = self.snippets_only()?;
        let names = plan.snippets.iter().map(|s| s.name.clone()).collect();
        Some((
            names,
            plan.snippet_notes.clone(),
            self.preview.as_ref()?.skipped,
        ))
    }

    /// 导入的结果中新增与相同而跳过的快捷命令数。
    pub(super) fn report_snippets(&self) -> Option<(usize, usize)> {
        self.report
            .as_ref()
            .map(|r| (r.snippets, r.snippets_skipped))
    }
}

/// 正在进行的操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    Reading,
    Importing,
}

/// 读取来源失败的原因。
enum Failure {
    /// 主机文件已加密，或 WindTerm 设了主密码：需要密码
    PasswordRequired,
    Message(SharedString),
}

impl Render for ImportDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rem = window.rem_size();
        // 高 40rem，窗口矮时缩到窗口内，预览列表在里面滚动
        let height = rems(40.)
            .to_pixels(rem)
            .min(window.viewport_size().height - rems(8.).to_pixels(rem));
        let root = v_flex()
            .id("import-dialog")
            .test_support()
            .h(height)
            .gap_3();
        if let Some(report) = &self.report {
            return root.child(self.render_report(report, cx));
        }
        let muted = cx.theme().muted_foreground;
        let hint = if self.from == ImportFrom::HostFile {
            t!("backup_ui.import.hint_hostfile")
        } else if self.from.snippets_only() {
            t!("backup_ui.import.hint_snippets")
        } else {
            t!("backup_ui.import.hint")
        };
        let root = root
            .child(div().text_xs().text_color(muted).child(prose(hint)))
            .child(self.render_source(cx));
        let root = match (&self.preview, self.snippets_only()) {
            (Some(preview), Some(plan)) => {
                root.child(self.render_snippets(plan, preview.skipped, window, cx))
            }
            (Some(preview), None) => root
                .child(self.render_toolbar(preview, cx))
                .child(self.render_list(preview, window, cx))
                .child(self.render_options(preview, cx)),
            (None, _) => root.child(div().flex_1()),
        };
        root.child(self.render_footer(cx))
    }
}

#[cfg(test)]
mod tests {
    use termlet_core::id::HostId;
    use termlet_core::import::Hint;

    use super::*;

    fn item(key: &str) -> PreviewItem {
        PreviewItem {
            key: key.into(),
            name: key.into(),
            protocol: Protocol::Ssh,
            address: "10.0.0.1".into(),
            port: 22,
            username: "root".into(),
            group: Vec::new(),
            tags: Vec::new(),
            jump: None,
            has_password: false,
            has_key: false,
            duplicate_of: None,
            hints: Vec::new(),
            proxy_command: None,
            startup: None,
            forwards: Vec::new(),
        }
    }

    fn forward() -> PreviewForward {
        PreviewForward {
            kind: ForwardKind::Local,
            name: String::new(),
            bind_host: "127.0.0.1".into(),
            bind_port: 8080,
            target_host: "db".into(),
            target_port: 5432,
            exposed: false,
            auto_start: false,
        }
    }

    #[test]
    fn details_list_everything_below_the_host() {
        let mut it = item("web");
        assert!(details(&it, "ssh_config", OnDuplicate::Skip).is_empty());
        assert_eq!(row_height(0), ROW_PADDING + MAIN_LINE);

        it.jump = Some("bastion".into());
        it.hints = vec![Hint::MissingUsername, Hint::ProxyCommandImported];
        it.proxy_command = Some("nc %h %p".into());
        it.startup = Some("cd /srv".into());
        it.forwards = vec![forward(), forward()];
        let lines = details(&it, "ssh_config", OnDuplicate::Skip);
        assert_eq!(lines.len(), 1 + 2 + 1 + 1 + 1 + 2);
        assert_eq!(lines[0], Detail::Jump("bastion".into()));
        assert_eq!(
            lines[5],
            Detail::ForwardsHeader {
                count: 2,
                merge: false
            }
        );
        assert_eq!(row_height(lines.len()), ROW_PADDING + MAIN_LINE + 8.);

        // 已存在的主机：跳过时转发不导入，不列出；合并时列出“将添加到已有的主机”
        it.duplicate_of = Some(HostId::from_raw("h_1"));
        assert_eq!(details(&it, "ssh_config", OnDuplicate::Skip).len(), 5);
        let merged = details(&it, "ssh_config", OnDuplicate::Merge);
        assert_eq!(
            merged[5],
            Detail::ForwardsHeader {
                count: 2,
                merge: true
            }
        );
    }

    #[test]
    fn address_omits_missing_user_and_brackets_ipv6() {
        let mut it = item("a");
        assert_eq!(address_text(&it), "root@10.0.0.1:22");
        it.username.clear();
        it.address = "::1".into();
        it.port = 2222;
        assert_eq!(address_text(&it), "[::1]:2222");
    }

    #[test]
    fn sources_pick_files_or_folders() {
        let all = ImportFrom::all();
        assert_eq!(all.len(), 11);
        assert_eq!(all[0], ImportFrom::HostFile);
        let folders: Vec<_> = all.iter().filter(|f| f.picks_directory()).collect();
        assert_eq!(
            folders,
            [
                &ImportFrom::Client(Source::Xshell),
                &ImportFrom::Client(Source::FinalShell),
                &ImportFrom::Client(Source::Termora),
                &ImportFrom::Client(Source::WindTerm)
            ]
        );
        assert_eq!(ImportFrom::Client(Source::Tabby).source_name(), "Tabby");
        assert_eq!(
            ImportFrom::Client(Source::Netcatty).source_name(),
            "Netcatty"
        );
        // 只有 NyaTerm 是只有快捷命令的来源
        let snippets: Vec<_> = all.iter().filter(|f| f.snippets_only()).collect();
        assert_eq!(snippets, [&ImportFrom::Client(Source::NyaTerm)]);
    }
}
