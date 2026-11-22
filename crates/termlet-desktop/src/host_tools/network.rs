//! 网络页：TCP、UDP 连接（有 ss 时用 ss，没有时读 /proc/net），可筛选（页签上写着各自的数量）、搜索；监听中的 TCP
//! 端口可以“转发到本机”（打开新建端口转发并预填），进程名跳到进程页并选中。显示时读一次，有刷新按钮；不能排序。
//! 不是 root 而有免密码 sudo 时经 sudo 读（看得到全部进程名，信息行尾标出“sudo”）；没有时信息行尾一个锁图标（只看
//! 得到自己的进程的进程名）。有 TIME-WAIT 的连接时信息行写出数量。
//!
//! - 监听视图（“监听”页签）：同一协议、同一端口的 IPv4 与 IPv6 合成一行，列为“端口 | 监听范围 | 进程 / 容器”，端口
//!   单独成列（窄的时候截短的是范围，端口不丢）。由容器发布的端口（Docker 页读到的端口映射，或者由 docker-proxy 等
//!   打开的）标出容器名（还没有 Docker 页的读数时只写“容器”），点了跳到 Docker 页并选中发布它的容器。
//! - 其余视图：列为“状态 | 本地地址 | 端口 | 对端地址 | 进程 / 容器”；列表窄时对端地址不显示，本地端口接在地址后面，
//!   地址截短时端口照样看得到。已连接的行用 ↙ / ↗ 标出入站（本机端口是监听着的端口）与出站；CLOSE-WAIT（对端已关闭、
//!   本机的程序还没有关）用警告色。当前行与其他页的选中行同样的底色。
//! - 筛选条件：从进程（“查看连接”）、服务的主进程、容器的端口跳过来时按进程或本机端口筛选，显示为可清除的小标签，
//!   不改搜索框。
//! - 键盘（见 [`super::keys`]）：↑ ↓ 等移动当前行，Enter 跳到进程（容器发布的端口跳到容器），Shift+F10 打开右键菜单。

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tab::Tab;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, VirtualListScrollHandle, h_flex, v_flex,
    v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, ScrollHandle, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _, Window,
    canvas, div, prelude::FluentBuilder as _, px, rems, size,
};
use termlet_core::hosttools::{
    Conn, ConnList, ConnProcess, ConnSource, DockerInfo, Privilege, Target,
};

use super::keys::{
    LIST_CONTEXT, OpenItem, OpenMenu, SelectFirst, SelectLast, SelectNext, SelectPageDown,
    SelectPageUp, SelectPrev,
};
use super::model::{
    BindPart, BrowserPreset, ConnChip, ConnFilter, ListenRow, Step, bind_parts, can_forward,
    conn_keys, conn_summary, filter_conns, forward_preset, host_port, is_listening, is_port_proxy,
    is_web_port, listen_rows, page_rows, port_owners, step_row,
};
use super::shared::{
    self, Feed, KeyMenu, Memo, PageContext, RootScope, Tone, ToolPage, error_line, hint_line,
    info_line, loading_text, notice, toolbar,
};
use crate::sftp::list_model::visible_name;
use crate::t;

/// 对端地址的列在列表宽度够时才显示（这时本地端口也单独成列；rem，量的是卡片里列表的宽度，行的左右各留 0.25rem）。
const PEER_COLUMN_FROM: f32 = 29.5;
/// 本地端口一列的宽度（rem）。
const PORT_WIDTH: f32 = 3.25;
/// 行高（rem）。
const ROW_HEIGHT: f32 = 1.5;
/// “进程 / 容器”一列的宽度（rem）。
const OWNER_WIDTH: f32 = 6.5;

/// 在浏览器中打开监听着的端口：交给面板（工作区转发到本机的空闲端口再打开）。
fn open_browser(context: &PageContext, conn: &Conn, cx: &mut App) {
    let preset = BrowserPreset::new(forward_preset(conn));
    let _ = context
        .panel
        .update(cx, |panel, cx| panel.open_in_browser(context, preset, cx));
}

/// 一行的“进程 / 容器”：由容器发布的监听端口写容器，否则写第一个进程，没有进程时写套接字的用户。
#[derive(Clone)]
enum Owner {
    /// 发布了这个端口的容器：容器名（还没有 Docker 页的读数时为 `None`）、协议与端口（跳到 Docker 页时按它找容器）
    Container {
        name: Option<String>,
        proto: String,
        port: u16,
    },
    Process(ConnProcess),
    User(Option<String>),
}

/// 监听端口是不是由容器发布的：Docker 页读到的端口映射中有它，或者打开它的是 docker-proxy 等转发进程。
fn owner(
    proto: &str,
    port: u16,
    listening: bool,
    processes: &[ConnProcess],
    user: Option<&String>,
    containers: &PortMap,
) -> Owner {
    if listening {
        let key = (proto.to_string(), port);
        if let Some(name) = containers.map.get(&key) {
            return Owner::Container {
                name: Some(name.clone()),
                proto: key.0,
                port,
            };
        }
        if processes.iter().any(|p| is_port_proxy(&p.name)) {
            return Owner::Container {
                name: None,
                proto: key.0,
                port,
            };
        }
    }
    match processes.first() {
        Some(p) => Owner::Process(p.clone()),
        None => Owner::User(user.cloned()),
    }
}

/// Docker 页读到的各容器发布的端口：（协议, 主机端口）→ 容器名。
#[derive(Default)]
struct PortMap {
    map: HashMap<(String, u16), String>,
}

impl PortMap {
    fn new(docker: Option<&DockerInfo>) -> Self {
        let map = docker
            .map(|info| {
                port_owners(&info.containers)
                    .into_iter()
                    .map(|(key, ix)| (key, visible_name(&info.containers[ix].name)))
                    .collect()
            })
            .unwrap_or_default();
        Self { map }
    }
}

pub(super) struct NetworkPage {
    context: PageContext,
    feed: Feed<ConnList>,
    search: Entity<InputState>,
    filter: ConnFilter,
    /// 按进程或端口的筛选条件（从别的页跳过来时加上）
    chip: Option<ConnChip>,
    /// 各连接的元素 ID 与它对应的数据
    keys: Option<(Rc<ConnList>, Rc<Vec<String>>)>,
    /// 筛选后的行（读数、搜索、筛选、筛选条件变了时重算）
    rows: Memo<ConnList, (String, ConnFilter, Option<ConnChip>), Rc<Vec<usize>>>,
    /// 监听视图合并后的行
    listen: Memo<ConnList, (String, Option<ConnChip>), Rc<Vec<ListenRow>>>,
    /// 各筛选页签上的数量（读数、筛选条件变了时重算；不受搜索影响）
    counts: Memo<ConnList, Option<ConnChip>, [usize; ConnFilter::ALL.len()]>,
    /// 监听着的（协议, 端口）：已连接的行据此分出入站与出站
    listening: Memo<ConnList, (), Rc<HashSet<(String, u16)>>>,
    scroll: VirtualListScrollHandle,
    /// 上方筛选页签的滚动状态：按它量页签排开的宽度
    tabs_scroll: ScrollHandle,
    /// 上一帧列表的宽度（决定是否显示对端地址）
    width: Rc<Cell<Pixels>>,
    /// 列表的焦点（键盘操作）
    focus: FocusHandle,
    key_menu: KeyMenu,
    /// 当前行（连接或监听端口的元素 ID）
    cursor: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl ToolPage for NetworkPage {
    type Data = ConnList;

    fn feed(&mut self) -> &mut Feed<ConnList> {
        &mut self.feed
    }

    fn context(&self) -> &PageContext {
        &self.context
    }

    fn after_load(&mut self, cx: &mut Context<Self>) {
        self.drop_hidden(cx);
    }
}

impl NetworkPage {
    pub fn new(context: PageContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = shared::search_input(|| t!("tools.network.search"), window, cx);
        let subscriptions = vec![cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.drop_hidden(cx);
                cx.notify();
            }
        })];
        Self {
            feed: Feed::new(&context),
            context,
            search,
            filter: ConnFilter::All,
            chip: None,
            keys: None,
            rows: Memo::default(),
            listen: Memo::default(),
            counts: Memo::default(),
            listening: Memo::default(),
            scroll: VirtualListScrollHandle::new(),
            tabs_scroll: ScrollHandle::new(),
            width: Rc::new(Cell::new(px(0.))),
            focus: shared::list_focus(cx),
            key_menu: KeyMenu::default(),
            cursor: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_connection(&mut self, target: Option<Target>, _: &mut Context<Self>) {
        self.context.target = target;
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        shared::activate(self, active, cx);
    }

    /// 上方筛选页签按文案排开比可见区域宽出多少（[`crate::tab_fit::overflow`]）：面板据此放宽最小宽度。
    pub fn tabs_overflow(&self) -> Option<Pixels> {
        crate::tab_fit::overflow(&self.tabs_scroll, ConnFilter::ALL.len())
    }

    /// 列表的焦点（面板切到这一页、跳过来时放进来）。
    pub fn list_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 搜索框（列表显示着时才有：还在读取、不可用时没有搜索框）。
    pub fn search(&self) -> Option<Entity<InputState>> {
        self.feed.data().map(|_| self.search.clone())
    }

    /// 从别的页跳过来：清空搜索、看全部，按进程或端口筛选（显示为可清除的小标签）。
    pub fn set_chip(&mut self, chip: ConnChip, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        self.filter = ConnFilter::All;
        self.chip = Some(chip);
        self.cursor = None;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn set_filter(&mut self, filter: ConnFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.cursor = None;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// 当前行被搜索藏起来（或已不在列表中）时去掉：Enter、右键菜单不对看不见的行起作用。
    fn drop_hidden(&mut self, cx: &mut Context<Self>) {
        let (Some(cursor), Some(data)) = (self.cursor.clone(), self.feed.data()) else {
            return;
        };
        if !self.row_keys(&data, cx).contains(&cursor) {
            self.cursor = None;
            cx.notify();
        }
    }

    /// 各筛选页签上的数量（不受搜索影响，与列表一致：监听按合并 IPv4 / IPv6 后的行数）。没有筛选条件时全部、已连接、
    /// TCP、UDP 取各状态的总数（不受列出上限的影响），有筛选条件时数列出的连接。
    fn tab_counts(&mut self, data: &Rc<ConnList>) -> [usize; ConnFilter::ALL.len()] {
        self.counts.get(data, self.chip.clone(), |data, chip| {
            let listen = || {
                let rows = filter_conns(&data.conns, ConnFilter::Listen, chip.as_ref(), "");
                listen_rows(&data.conns, &rows).len()
            };
            match chip {
                None => {
                    let sum = conn_summary(data);
                    [sum.total, listen(), sum.established, sum.tcp, sum.udp]
                }
                Some(chip) => ConnFilter::ALL.map(|filter| match filter {
                    ConnFilter::Listen => listen(),
                    filter => filter_conns(&data.conns, filter, Some(chip), "").len(),
                }),
            }
        })
    }

    fn listening_view(&self) -> bool {
        self.filter == ConnFilter::Listen
    }

    /// 各连接的元素 ID：数据变了时重新计算。
    fn keys(&mut self, data: &Rc<ConnList>) -> Rc<Vec<String>> {
        match &self.keys {
            Some((shown, keys)) if Rc::ptr_eq(shown, data) => keys.clone(),
            _ => {
                let keys = Rc::new(conn_keys(&data.conns));
                self.keys = Some((data.clone(), keys.clone()));
                keys
            }
        }
    }

    /// 筛选后的连接（在列表中的位置）。
    fn shown_rows(&mut self, data: &Rc<ConnList>, cx: &App) -> Rc<Vec<usize>> {
        let query = self.search.read(cx).value().to_string();
        self.rows.get(
            data,
            (query, self.filter, self.chip.clone()),
            |data, (query, filter, chip)| {
                Rc::new(filter_conns(&data.conns, *filter, chip.as_ref(), query))
            },
        )
    }

    /// 监听视图的行：筛选后的监听连接按协议、端口合并。
    fn shown_listen(&mut self, data: &Rc<ConnList>, cx: &App) -> Rc<Vec<ListenRow>> {
        let rows = self.shown_rows(data, cx);
        let query = self.search.read(cx).value().to_string();
        self.listen
            .get(data, (query, self.chip.clone()), |data, _| {
                Rc::new(listen_rows(&data.conns, &rows))
            })
    }

    /// 当前视图各行的元素 ID（键盘移动用）。
    fn row_keys(&mut self, data: &Rc<ConnList>, cx: &App) -> Vec<String> {
        if self.listening_view() {
            self.shown_listen(data, cx)
                .iter()
                .map(|row| row.key.clone())
                .collect()
        } else {
            let keys = self.keys(data);
            self.shown_rows(data, cx)
                .iter()
                .map(|&ix| keys[ix].clone())
                .collect()
        }
    }

    /// 当前行的“进程 / 容器”与它的右键菜单。
    fn cursor_row(&mut self, cx: &mut Context<Self>) -> Option<(Owner, MenuRow)> {
        let data = self.feed.data()?;
        let key = self.cursor.clone()?;
        let ports = PortMap::new(shared::docker_cache(&self.context).as_deref());
        if self.listening_view() {
            let rows = self.shown_rows(&data, cx);
            let listen = self.shown_listen(&data, cx);
            let row = listen.iter().find(|row| row.key == key)?;
            let owner = owner(
                &row.proto,
                row.port,
                true,
                &row.processes,
                row.user.as_ref(),
                &ports,
            );
            let forward = row.forward_conn(&data.conns, &rows).cloned();
            Some((owner.clone(), MenuRow::Listen(row.clone(), forward, owner)))
        } else {
            let keys = self.keys(&data);
            let ix = keys.iter().position(|k| *k == key)?;
            let conn = data.conns[ix].clone();
            let owner = owner(
                &conn.proto,
                conn.local_port,
                is_listening(&conn.proto, &conn.state),
                &conn.processes,
                conn.user.as_ref(),
                &ports,
            );
            Some((owner.clone(), MenuRow::Conn(conn, owner)))
        }
    }

    // ------------------------------------------------------------ 跳转与键盘

    /// 跳到“进程 / 容器”：容器跳到 Docker 页并选中，进程跳到进程页并选中。
    fn show_owner(&self, owner: &Owner, window: &mut Window, cx: &mut App) {
        show_owner(&self.context, owner, window, cx);
    }

    /// ↑ ↓ 等：移动当前行，滚动跟随。
    fn move_cursor(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let keys = self.row_keys(&data, cx);
        let current = self
            .cursor
            .as_deref()
            .and_then(|key| keys.iter().position(|k| k == key));
        let row = f32::from(rems(ROW_HEIGHT).to_pixels(window.rem_size()));
        let page = page_rows(f32::from(self.scroll.bounds().size.height), row);
        let Some(ix) = step_row(current, keys.len(), step, page, |_| true) else {
            return;
        };
        self.cursor = Some(keys[ix].clone());
        self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Enter：跳到当前行的进程（容器发布的端口跳到容器）；没有当前行时到第一行。
    fn open_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cursor.is_none() {
            self.move_cursor(Step::First, window, cx);
            return;
        }
        if let Some((owner, _)) = self.cursor_row(cx) {
            self.show_owner(&owner, window, cx);
        }
    }

    /// Shift+F10、菜单键：在当前行下面打开它的右键菜单。
    fn open_key_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, row)) = self.cursor_row(cx) {
            let build = self.menu_builder(row);
            KeyMenu::open(
                window,
                cx,
                self.focus.clone(),
                |page| &mut page.key_menu,
                build,
            );
        }
    }

    fn set_cursor(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        self.cursor = Some(key);
        cx.notify();
    }

    /// 行的右键菜单（鼠标右键与键盘打开的是同一个）：转发到本机、在浏览器中打开（转发到本机的空闲端口，监听着的 TCP
    /// 端口都有，按端口猜 http 或 https）、查看容器与进程、复制地址。
    fn menu_builder(
        &self,
        row: MenuRow,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let context = self.context.clone();
        let ssh = self.context.ssh_host.is_some();
        move |menu, _, _| {
            let (conn, owner, processes, copy): (Option<Conn>, &Owner, Vec<ConnProcess>, _) =
                match &row {
                    MenuRow::Conn(conn, owner) => (
                        Some(conn.clone()),
                        owner,
                        conn.processes.clone(),
                        vec![
                            Some((
                                t!("tools.network.copy_local"),
                                host_port(&conn.local_addr, conn.local_port),
                            )),
                            (conn.peer_port > 0).then(|| {
                                (
                                    t!("tools.network.copy_peer"),
                                    host_port(&conn.peer_addr, conn.peer_port),
                                )
                            }),
                        ],
                    ),
                    MenuRow::Listen(row, forward, owner) => (
                        forward.clone(),
                        owner,
                        row.processes.clone(),
                        vec![Some((t!("tools.network.copy_port"), row.port.to_string()))],
                    ),
                };
            let mut menu = menu;
            if let Some(conn) = conn.filter(|c| ssh && can_forward(c)) {
                let (forward_context, browser_context) = (context.clone(), context.clone());
                let browser_conn = conn.clone();
                menu = menu
                    .item(
                        PopupMenuItem::new(t!("tools.network.forward_local"))
                            .icon(IconName::ArrowRightLeft)
                            .on_click(move |_, _, cx| {
                                let (context, preset) =
                                    (forward_context.clone(), forward_preset(&conn));
                                let _ = context
                                    .panel
                                    .update(cx, |panel, cx| panel.forward(&context, preset, cx));
                            }),
                    )
                    .item(
                        PopupMenuItem::new(t!("tools.network.open_browser"))
                            .icon(IconName::Globe)
                            .on_click(move |_, _, cx| {
                                open_browser(&browser_context, &browser_conn, cx)
                            }),
                    );
            }
            if let Owner::Container { name, .. } = owner {
                let (context, owner) = (context.clone(), owner.clone());
                let label = match name {
                    Some(name) => t!("tools.network.show_container", name = name.clone()),
                    None => t!("tools.network.show_container_unknown"),
                };
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .icon(IconName::Container)
                        .on_click(move |_, window, cx| show_owner(&context, &owner, window, cx)),
                );
            }
            for process in processes {
                let (context, pid) = (context.clone(), process.pid);
                menu = menu.item(
                    PopupMenuItem::new(t!(
                        "tools.network.show_process",
                        name = visible_name(&process.name),
                        pid = pid
                    ))
                    .icon(IconName::Cpu)
                    .on_click(move |_, window, cx| {
                        shared::go(&context, window, cx, move |panel, window, cx| {
                            panel.show_process(pid, window, cx)
                        });
                    }),
                );
            }
            menu = menu.separator();
            for (label, text) in copy.iter().flatten().cloned() {
                menu = menu.item(PopupMenuItem::new(label).icon(IconName::Copy).on_click(
                    move |_, window, cx| shared::copy_text(text.clone(), text.clone(), window, cx),
                ));
            }
            menu
        }
    }

    // ------------------------------------------------------------ 渲染

    fn render_header(&self, peer: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let header = shared::list_header(cx).px_1().gap_2();
        if self.listening_view() {
            return header
                .child(
                    div()
                        .flex_none()
                        .w(rems(5.))
                        .child(t!("tools.network.col_port")),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(t!("tools.network.col_scope")),
                )
                .child(
                    div()
                        .flex_none()
                        .w(rems(OWNER_WIDTH))
                        .truncate()
                        .child(t!("tools.network.col_owner")),
                );
        }
        header
            .child(
                div()
                    .flex_none()
                    .w(rems(6.))
                    .child(t!("tools.network.col_state")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(t!("tools.network.col_local")),
            )
            .when(peer, |this| {
                this.child(
                    div()
                        .flex_none()
                        .w(rems(PORT_WIDTH))
                        .truncate()
                        .child(t!("tools.network.col_port")),
                )
            })
            .when(peer, |this| {
                this.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(t!("tools.network.col_peer")),
                )
            })
            .child(
                div()
                    .flex_none()
                    .w(rems(OWNER_WIDTH))
                    .truncate()
                    .child(t!("tools.network.col_owner")),
            )
    }

    /// “进程 / 容器”一格：容器、进程是可以点的（跳过去），没有进程时写套接字的用户。`menu_open`：窗口中开着右键
    /// 菜单，按钮不挂悬停提示（按钮的提示只在按下左键时收起，会浮在菜单上，见 [`crate::menu_tooltip`]）。
    fn render_owner(
        &self,
        key: &str,
        owner: &Owner,
        processes: &[ConnProcess],
        menu_open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let names: SharedString = processes
            .iter()
            .map(|p| format!("{} ({})", visible_name(&p.name), p.pid))
            .collect::<Vec<_>>()
            .join(", ")
            .into();
        let (id, label, tooltip) = match owner {
            Owner::User(user) => {
                return div()
                    .truncate()
                    .text_color(muted.opacity(0.6))
                    .child(user.clone().unwrap_or_default())
                    .into_any_element();
            }
            Owner::Process(p) => (
                format!("tools-conn-process-{key}"),
                visible_name(&p.name),
                names,
            ),
            Owner::Container { name, port, .. } => {
                let label = name
                    .clone()
                    .unwrap_or_else(|| t!("tools.network.container").to_string());
                let tip = match name {
                    Some(name) => t!(
                        "tools.network.container_hint",
                        name = name.clone(),
                        port = port
                    ),
                    None => t!("tools.network.container_unknown_hint", port = port),
                };
                let tip = if names.is_empty() {
                    tip
                } else {
                    format!("{tip}\n{names}").into()
                };
                (format!("tools-conn-container-{key}"), label, tip)
            }
        };
        let target = owner.clone();
        let button = Button::new(SharedString::from(id))
            .ghost()
            .xsmall()
            .compact()
            .label(label)
            .when(!menu_open, |this| this.tooltip(tooltip))
            .on_click(cx.listener(move |this, _, window, cx| this.show_owner(&target, window, cx)));
        let button = if matches!(owner, Owner::Container { .. }) {
            button.icon(IconName::Container)
        } else {
            button
        };
        button.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_row(
        &self,
        conn: &Conn,
        key: &str,
        peer: bool,
        ports: &PortMap,
        listening: &HashSet<(String, u16)>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cursor = self.cursor.as_deref() == Some(key);
        let ring = cursor && (self.focus.is_focused(window) || self.key_menu.is_open());
        let menu_layer = cursor.then(|| self.key_menu.layer(window)).flatten();
        let owner = owner(
            &conn.proto,
            conn.local_port,
            is_listening(&conn.proto, &conn.state),
            &conn.processes,
            conn.user.as_ref(),
            ports,
        );
        // 窗口中开着右键菜单时行内的按钮不挂悬停提示（会浮在菜单上，见 crate::menu_tooltip）
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        let owner_cell = self.render_owner(key, &owner, &conn.processes, menu_open, cx);
        let menu = self.menu_builder(MenuRow::Conn(conn.clone(), owner));
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mono = theme.mono_font_family.clone();
        // 监听着的是“运行中”（绿），CLOSE-WAIT（对端已关闭、本机的程序还没有关）用警告色，其余用正文的颜色（各页共用的
        // 颜色表）
        let state_color = match conn.state.as_str() {
            "LISTEN" => Tone::Good.color(cx),
            "CLOSE-WAIT" => Tone::Warning.color(cx),
            _ => theme.foreground,
        };
        // 已连接的：本机端口是监听着的端口为入站（↙），否则为出站（↗）
        let direction = (conn.state == "ESTAB").then(|| {
            if listening.contains(&(conn.proto.clone(), conn.local_port)) {
                (
                    "in",
                    IconName::ArrowDownLeft,
                    t!("tools.network.inbound", port = conn.local_port),
                )
            } else {
                ("out", IconName::ArrowUpRight, t!("tools.network.outbound"))
            }
        });
        let local = host_port(&conn.local_addr, conn.local_port);
        let remote = host_port(&conn.peer_addr, conn.peer_port);
        let (local_tip, remote_tip) = (local.clone(), remote.clone());
        let cursor_key = key.to_string();
        // 监听着常见网页端口的：地址后面直接有“在浏览器中打开”
        let browser =
            (self.context.ssh_host.is_some() && can_forward(conn) && is_web_port(conn.local_port))
                .then(|| {
                    let (context, conn) = (self.context.clone(), conn.clone());
                    Button::new(SharedString::from(format!("tools-conn-browser-{key}")))
                        .ghost()
                        .xsmall()
                        .flex_none()
                        .icon(IconName::Globe)
                        .when(!menu_open, |this| {
                            this.tooltip(t!(
                                "tools.network.open_browser_hint",
                                port = conn.local_port
                            ))
                        })
                        .on_click(move |_, _, cx| open_browser(&context, &conn, cx))
                });
        let row = h_flex()
            .id(SharedString::from(format!("tools-conn-{key}")))
            .test_support()
            .size_full()
            .px_1()
            .gap_2()
            .text_xs()
            .cursor_default()
            .when(cursor, |el| el.bg(theme.list_active))
            .when(!cursor, |el| el.hover(|s| s.bg(theme.list_hover)));
        shared::ring(row, ring, cx)
            .children(menu_layer)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.set_cursor(cursor_key.clone(), window, cx)
            }))
            .child(
                h_flex()
                    .flex_none()
                    .w(rems(6.))
                    .gap_1()
                    .child(div().text_color(muted).child(conn.proto.clone()))
                    .children(direction.map(|(kind, icon, tip)| {
                        div()
                            .id(SharedString::from(format!("tools-conn-{kind}-{key}")))
                            .test_support()
                            .flex()
                            .flex_none()
                            .child(Icon::new(icon).xsmall().text_color(muted))
                            .tooltip(shared::row_tooltip(tip))
                    }))
                    .child(
                        div()
                            .id(SharedString::from(format!("tools-conn-state-{key}")))
                            .test_support()
                            .min_w_0()
                            .truncate()
                            .text_color(state_color)
                            .child(conn.state.clone()),
                    ),
            )
            .child(
                // 地址截短时端口照样看得到：列表够宽时端口单独成列，否则接在地址后面；“在浏览器中打开”在悬停说明的
                // 范围之外（不与地址的悬停说明同时出现）
                h_flex()
                    .id(SharedString::from(format!("tools-conn-local-{key}")))
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .font_family(mono.clone())
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("tools-conn-address-{key}")))
                            .flex_shrink(1.)
                            .min_w_0()
                            .child(
                                div()
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .truncate()
                                    .child(address_text(&conn.local_addr)),
                            )
                            .when(!peer, |this| {
                                this.child(div().flex_none().child(port_text(conn.local_port)))
                            })
                            .tooltip(shared::row_tooltip(local_tip.into())),
                    )
                    .children(browser),
            )
            .when(peer, |this| {
                this.child(
                    div()
                        .id(SharedString::from(format!("tools-conn-port-{key}")))
                        .test_support()
                        .flex_none()
                        .w(rems(PORT_WIDTH))
                        .truncate()
                        .font_family(mono.clone())
                        .font_medium()
                        .child(port_label(conn.local_port)),
                )
                .child(
                    h_flex()
                        .id(SharedString::from(format!("tools-conn-peer-{key}")))
                        .flex_1()
                        .min_w_0()
                        .font_family(mono)
                        .text_color(muted)
                        .child(
                            div()
                                .flex_shrink(1.)
                                .min_w_0()
                                .truncate()
                                .child(address_text(&conn.peer_addr)),
                        )
                        .child(div().flex_none().child(port_text(conn.peer_port)))
                        .tooltip(shared::row_tooltip(remote_tip.into())),
                )
            })
            .child(
                // 按钮按自己的宽度靠左（与表头对齐）
                h_flex()
                    .flex_none()
                    .w(rems(OWNER_WIDTH))
                    .min_w_0()
                    .child(owner_cell),
            )
            .context_menu(menu)
            .into_any_element()
    }

    /// 监听视图的一行：端口（单独成列，不截短）、监听范围（窄的时候截短，悬停看全部地址）、进程 / 容器。
    fn render_listen_row(
        &self,
        row: &ListenRow,
        forward: Option<Conn>,
        ports: &PortMap,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = row.key.as_str();
        let cursor = self.cursor.as_deref() == Some(key);
        let ring = cursor && (self.focus.is_focused(window) || self.key_menu.is_open());
        let menu_layer = cursor.then(|| self.key_menu.layer(window)).flatten();
        let owner = owner(
            &row.proto,
            row.port,
            true,
            &row.processes,
            row.user.as_ref(),
            ports,
        );
        // 窗口中开着右键菜单时行内的按钮不挂悬停提示（会浮在菜单上，见 crate::menu_tooltip）
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        let owner_cell = self.render_owner(key, &owner, &row.processes, menu_open, cx);
        // 看起来是网页的端口（常见的 HTTP、HTTPS 端口）：范围后面直接有“在浏览器中打开”
        let browser = forward
            .clone()
            .filter(|_| self.context.ssh_host.is_some() && is_web_port(row.port))
            .map(|conn| {
                let context = self.context.clone();
                Button::new(SharedString::from(format!("tools-listen-browser-{key}")))
                    .ghost()
                    .xsmall()
                    .flex_none()
                    .icon(IconName::Globe)
                    .when(!menu_open, |this| {
                        this.tooltip(t!("tools.network.open_browser_hint", port = row.port))
                    })
                    .on_click(move |_, _, cx| open_browser(&context, &conn, cx))
            });
        let menu = self.menu_builder(MenuRow::Listen(row.clone(), forward, owner));
        let scope = bind_parts(&row.addrs)
            .into_iter()
            .map(|part| match part {
                BindPart::Any => t!("tools.network.scope_any").to_string(),
                BindPart::AnyV4 => t!("tools.network.scope_any_v4").to_string(),
                BindPart::AnyV6 => t!("tools.network.scope_any_v6").to_string(),
                BindPart::Addr(addr) => addr,
            })
            .collect::<Vec<_>>()
            .join(&t!("tools.list_separator"));
        let scope_tip: SharedString = row
            .addrs
            .iter()
            .map(|addr| host_port(addr, row.port))
            .collect::<Vec<_>>()
            .join("\n")
            .into();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mono = theme.mono_font_family.clone();
        let cursor_key = row.key.clone();
        let line = h_flex()
            .id(SharedString::from(format!("tools-listen-{key}")))
            .test_support()
            .size_full()
            .px_1()
            .gap_2()
            .text_xs()
            .cursor_default()
            .when(cursor, |el| el.bg(theme.list_active))
            .when(!cursor, |el| el.hover(|s| s.bg(theme.list_hover)));
        shared::ring(line, ring, cx)
            .children(menu_layer)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.set_cursor(cursor_key.clone(), window, cx)
            }))
            .child(
                h_flex()
                    .id(SharedString::from(format!("tools-listen-port-{key}")))
                    .test_support()
                    .flex_none()
                    .w(rems(5.))
                    .gap_1()
                    .whitespace_nowrap()
                    .child(div().text_color(muted).child(row.proto.clone()))
                    .child(
                        div()
                            .font_family(mono.clone())
                            .font_medium()
                            .child(row.port.to_string()),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .id(SharedString::from(format!("tools-listen-scope-{key}")))
                            .test_support()
                            .flex_shrink(1.)
                            .min_w_0()
                            .truncate()
                            .font_family(mono)
                            .child(scope)
                            .tooltip(shared::row_tooltip(scope_tip)),
                    )
                    .children(browser),
            )
            .child(
                // 按钮按自己的宽度靠左（与表头对齐）
                h_flex()
                    .flex_none()
                    .w(rems(OWNER_WIDTH))
                    .min_w_0()
                    .child(owner_cell),
            )
            .context_menu(menu)
            .into_any_element()
    }

    fn render_list(
        &mut self,
        data: &Rc<ConnList>,
        peer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = self.shown_rows(data, cx);
        let row_height = rems(ROW_HEIGHT).to_pixels(window.rem_size());
        let ports = Rc::new(PortMap::new(shared::docker_cache(&self.context).as_deref()));
        if self.listening_view() {
            let listen = self.shown_listen(data, cx);
            if listen.is_empty() {
                return shared::empty_text(self.empty_text(data), cx);
            }
            let sizes = Rc::new(vec![size(px(0.), row_height); listen.len()]);
            let data = data.clone();
            return v_virtual_list(
                cx.entity(),
                "tools-listen-rows",
                sizes,
                move |this, range, window, cx| {
                    range
                        .filter_map(|ix| listen.get(ix))
                        .map(|row| {
                            let forward = row.forward_conn(&data.conns, &rows).cloned();
                            this.render_listen_row(row, forward, &ports, window, cx)
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element();
        }
        if rows.is_empty() {
            return shared::empty_text(self.empty_text(data), cx);
        }
        let keys = self.keys(data);
        let listening = self.listening.get(data, (), |data, _| {
            Rc::new(
                data.conns
                    .iter()
                    .filter(|c| is_listening(&c.proto, &c.state))
                    .map(|c| (c.proto.clone(), c.local_port))
                    .collect(),
            )
        });
        let sizes = Rc::new(vec![size(px(0.), row_height); rows.len()]);
        let data = data.clone();
        v_virtual_list(
            cx.entity(),
            "tools-conn-rows",
            sizes,
            move |this, range, window, cx| {
                range
                    .filter_map(|ix| rows.get(ix).copied())
                    .map(|i| {
                        this.render_row(
                            &data.conns[i],
                            &keys[i],
                            peer,
                            &ports,
                            &listening,
                            window,
                            cx,
                        )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.scroll)
        .size_full()
        .into_any_element()
    }

    /// 没有连接时的说明：按进程筛选而不是 root 时，别的用户的进程的连接看不到进程（ss -p 的限制），另外说明。
    fn empty_text(&self, data: &ConnList) -> SharedString {
        match &self.chip {
            Some(ConnChip::Pid { .. }) if data.uid != Some(0) && !data.sudo => {
                t!("tools.network.empty_pid_not_root")
            }
            _ => t!("tools.network.empty"),
        }
    }

    /// 筛选条件的小标签。
    fn render_chips(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let chip = self.chip.as_ref()?;
        let label = match chip {
            ConnChip::Pid {
                pid,
                name: Some(name),
            } => t!("tools.network.chip_pid", name = name.clone(), pid = pid),
            ConnChip::Pid { pid, name: None } => t!("tools.network.chip_pid_only", pid = pid),
            ConnChip::Port(port) => t!("tools.network.chip_port", port = port),
        };
        shared::chip_row(vec![shared::filter_chip(
            "tools-conn-chip",
            label,
            cx.listener(|this, _, _, cx| {
                this.chip = None;
                cx.notify();
            }),
            cx,
        )])
    }
}

/// 右键菜单所在的行。
#[derive(Clone)]
enum MenuRow {
    Conn(Conn, Owner),
    /// 监听视图的一行与用于“转发到本机…”的那个连接
    Listen(ListenRow, Option<Conn>, Owner),
}

/// 跳到“进程 / 容器”：容器跳到 Docker 页并选中（按端口找发布它的容器），进程跳到进程页并选中。
fn show_owner(context: &PageContext, owner: &Owner, window: &mut Window, cx: &mut App) {
    match owner.clone() {
        Owner::Container { proto, port, .. } => {
            shared::go(context, window, cx, move |panel, window, cx| {
                panel.show_container_port(proto, port, window, cx)
            })
        }
        Owner::Process(p) => shared::go(context, window, cx, move |panel, window, cx| {
            panel.show_process(p.pid, window, cx)
        }),
        Owner::User(_) => {}
    }
}

impl Render for NetworkPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(data) = self.feed.data() else {
            let body = match &self.feed.error {
                Some(error) => {
                    let this = cx.entity().downgrade();
                    notice(
                        error,
                        false,
                        move |_, _, cx| {
                            let _ = this.update(cx, |page, cx| shared::refresh(page, false, cx));
                        },
                        cx,
                    )
                }
                None => loading_text(cx),
            };
            return shared::focus_body(&self.focus, body);
        };
        let theme = cx.theme();
        let (muted, warning) = (theme.muted_foreground, theme.warning);
        let rem = window.rem_size();
        let peer = self.width.get() >= rems(PEER_COLUMN_FROM).to_pixels(rem);
        let sum = conn_summary(&data);
        // 信息行：只列出了一部分时写明；有 TIME-WAIT 的连接时写出数量
        let info = h_flex()
            .id("tools-conn-info")
            .test_support()
            .gap_2()
            .when(data.truncated, |this| {
                this.child(t!(
                    "tools.network.truncated",
                    shown = data.conns.len(),
                    total = data.total
                ))
            })
            .when(sum.time_wait > 0, |this| {
                this.child(
                    div()
                        .id("tools-conn-time-wait")
                        .test_support()
                        .child(t!("tools.network.time_wait", count = sum.time_wait)),
                )
            });
        let hint = (data.source == ConnSource::Proc).then(|| t!("tools.network.no_ss"));
        // 经 sudo 读到全部进程名时“sudo”，不是 root 又没有免密码 sudo 时锁图标
        let privilege = match self.feed.privilege {
            _ if data.uid == Some(0) => None,
            Some(Privilege::Sudo) if !data.sudo => None,
            privilege => privilege,
        };
        let marker = shared::privilege_marker(
            RootScope::Network,
            privilege,
            t!("tools.privilege.sudo_network"),
            &self.context,
            cx,
        );
        let counts = self.tab_counts(&data);
        let filter_ix = ConnFilter::ALL
            .iter()
            .position(|f| *f == self.filter)
            .unwrap_or(0);
        let body = self.render_list(&data, peer, window, cx);
        let chips = self.render_chips(cx);
        let measured = self.width.clone();
        let entity = cx.entity().downgrade();
        let threshold = rems(PEER_COLUMN_FROM).to_pixels(rem);
        let loading = self.feed.loading;
        let this = cx.entity().downgrade();
        shared::list_page()
            .child(toolbar(
                "tools-conn",
                &self.search,
                loading,
                move |_, _, cx| {
                    let _ = this.update(cx, |page, cx| shared::refresh(page, false, cx));
                },
            ))
            .child(shared::filter_tabs(
                "tools-conn-filter",
                &self.tabs_scroll,
                filter_ix,
                ConnFilter::ALL
                    .into_iter()
                    .zip(counts)
                    .map(|(filter, count)| {
                        Tab::new()
                            .label(filter_label(filter))
                            .suffix(div().text_color(muted).child(count.to_string()))
                    })
                    .collect(),
                cx.listener(|this, ix: &usize, _, cx| {
                    if let Some(filter) = ConnFilter::ALL.get(*ix) {
                        this.set_filter(*filter, cx);
                    }
                }),
            ))
            .children(chips)
            .child(info_line(info, marker, self.feed.shown.at, cx))
            .children(hint.map(|hint| hint_line(hint, warning)))
            .children(error_line(self.feed.error.as_ref(), cx))
            .child(
                shared::list_card(v_flex(), true, cx)
                    .id("tools-conn-list")
                    .test_support()
                    .key_context(LIST_CONTEXT)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(|this, _: &SelectPrev, window, cx| {
                        this.move_cursor(Step::Prev, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectNext, window, cx| {
                        this.move_cursor(Step::Next, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectPageUp, window, cx| {
                        this.move_cursor(Step::PageUp, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectPageDown, window, cx| {
                        this.move_cursor(Step::PageDown, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectFirst, window, cx| {
                        this.move_cursor(Step::First, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectLast, window, cx| {
                        this.move_cursor(Step::Last, window, cx)
                    }))
                    .on_action(
                        cx.listener(|this, _: &OpenItem, window, cx| this.open_item(window, cx)),
                    )
                    .on_action(
                        cx.listener(|this, _: &OpenMenu, window, cx| {
                            this.open_key_menu(window, cx)
                        }),
                    )
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        // 宽度跨过显示 / 隐藏对端地址的界限时重新渲染
                        canvas(
                            move |bounds, _, cx| {
                                let old = measured.replace(bounds.size.width);
                                if (old >= threshold) != (bounds.size.width >= threshold) {
                                    let _ = entity.update(cx, |_, cx| cx.notify());
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(self.render_header(peer, cx))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(body)
                            .vertical_scrollbar(&self.scroll),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
impl NetworkPage {
    pub(super) fn chip(&self) -> Option<ConnChip> {
        self.chip.clone()
    }

    pub(super) fn cursor(&self) -> Option<String> {
        self.cursor.clone()
    }

    pub(super) fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().to_string()
    }

    /// 各筛选页签上的数量（全部、监听、已连接、TCP、UDP）。
    pub(super) fn counts(&mut self) -> [usize; ConnFilter::ALL.len()] {
        let data = self.feed.data().expect("网络页的数据");
        self.tab_counts(&data)
    }
}

/// 地址（IPv6 加方括号）：端口另外写，地址截短时端口照样看得到。
fn address_text(addr: &str) -> String {
    if addr.contains(':') {
        format!("[{addr}]")
    } else {
        addr.to_string()
    }
}

/// 接在地址后面的端口：`:22`（端口为 0 时写 `:*`）。
fn port_text(port: u16) -> String {
    format!(":{}", port_label(port))
}

/// 端口一列：`22`（为 0 时写 `*`）。
fn port_label(port: u16) -> String {
    if port == 0 {
        "*".into()
    } else {
        port.to_string()
    }
}

fn filter_label(filter: ConnFilter) -> SharedString {
    match filter {
        ConnFilter::All => t!("tools.network.filter_all"),
        ConnFilter::Listen => t!("tools.network.filter_listen"),
        ConnFilter::Established => t!("tools.network.filter_established"),
        ConnFilter::Tcp => "TCP".into(),
        ConnFilter::Udp => "UDP".into(),
    }
}
