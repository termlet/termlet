//! 主机对话框：新建、编辑 SSH、Telnet、串口主机；删除主机的确认。
//!
//! 分页（SSH）：常规（协议、名称、地址、端口、用户名、认证、分组，多数主机只需填这一页）、连接（跳板机、代理或代理命令
//! （可沿用分组的）、兼容旧设备、agent 转发、X11 转发、只用一个通道）、验证码（TOTP）、终端（配色、编码、登录后执行的命令、tmux）、
//! 命令行工具、标签与备注。Telnet 没有验证码与命令行工具，串口另外没有连接页；它们的终端页是行规程
//! （回车键、本地回显、退格键，Telnet 还有终端类型）。
//! 有设置的分页在导航上带一个小圆点。保存过的密码、私钥等不回显，留空表示保持不变。
//! "测试连接"用填写的内容（含未保存的密码）试一次：SSH 连接并认证，显示耗时与服务器版本；Telnet 只建立
//! TCP 连接（不登录）；串口按参数打开后随即关闭。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::form::{Field, Form, field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::sidebar::{SidebarItem as _, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, TestSupportExt as _, Window, div, prelude::FluentBuilder as _,
    rems,
};
use termlet_core::id::{GroupId, HostId, ProfileId};
use termlet_core::inherit::{self, Inherited};
use termlet_core::model::{
    Auth, Backspace, CliAccess, CliAuth, CliCredential, Encoding, Enter, FlowControl, Group, Host,
    Jump, KeySource, LineOptions, LocalEcho, Parity, Profile, Protocol, Proxy, ProxyKind,
    SecretField, StopBits,
};
use termlet_core::secret::SecretString;
use termlet_core::serial::{self, PortInfo, SerialConfig};
use termlet_core::ssh::{self, HostDraft, Prompter, SecretEdit, SecretEdits};
use termlet_core::store::{Owner, Position, Store};
use termlet_core::telnet;
use termlet_core::totp::Totp;

use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::dialog_token::{DialogToken, close_own};
use crate::group_dialog::{jump_hosts, profile_choices, scheme_choices};
use crate::key_fields::{self, KeyFields};
use crate::notify::{self, NoticeLine};
use crate::prompt_dialog::{self, PromptRequest};
use crate::prose::prose;
use crate::{raw_errors, ssh_errors, store_errors, t};

/// 新建主机（放进 `group`）。
pub fn open_new(group: Option<GroupId>, window: &mut Window, cx: &mut App) {
    open(None, group, None, window, cx);
}

/// 新建主机，预填 `host` 的内容与密码（临时连接"保存到主机列表…"）。
pub fn open_prefilled(
    host: Host,
    password: Option<SecretString>,
    window: &mut Window,
    cx: &mut App,
) {
    open(None, None, Some((host, password)), window, cx);
}

/// 编辑主机。
pub fn open_edit(id: HostId, window: &mut Window, cx: &mut App) {
    let group = AppState::store(cx)
        .get::<Host>(&id)
        .and_then(|r| r.parent)
        .map(GroupId::from_raw);
    open(Some(id), group, None, window, cx);
}

fn open(
    id: Option<HostId>,
    group: Option<GroupId>,
    prefill: Option<(Host, Option<SecretString>)>,
    window: &mut Window,
    cx: &mut App,
) {
    let editing = id.is_some();
    let form = cx.new(|cx| HostForm::new(id, group, prefill, window, cx));
    let first_focus = Rc::new(Cell::new(true));
    let width = rems(46.).to_pixels(window.rem_size());
    let owner = form.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false) {
            form.read(cx).name.focus_handle(cx).focus(window, cx);
        }
        let save = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.save(window, cx))
            }
        };
        dialog
            .title(if editing {
                t!("host.edit.title")
            } else {
                t!("host.new.title")
            })
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
            .on_ok(save)
    });
    let token = DialogToken::take(window, cx);
    let notice = owner.update(cx, |form, _| {
        form.dialog = token;
        form.notice.clone()
    });
    notify::attach(&notice, window, cx);
}

/// 确认后删除主机（连同它保存的凭据与端口转发规则）。
pub fn confirm_delete(id: HostId, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    let Some(host) = store.get::<Host>(&id) else {
        return;
    };
    let name = host.doc.name;
    crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
        let store = store.clone();
        let id = id.clone();
        alert
            .confirm()
            .title(t!("host.delete.title", name = name.clone()))
            .description(prose(t!("host.delete.description")))
            .ok_text(t!("common.delete"))
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("common.cancel"))
            .on_ok(move |_, window, cx| {
                tracing::info!(id = %id, "删除主机");
                if let Err(e) = store.delete::<Host>(&id) {
                    crate::notify::error(store_errors::describe(&e), window, cx);
                }
                true
            })
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    General,
    Connection,
    Totp,
    Terminal,
    Cli,
    Tags,
}

impl Page {
    const ALL: [Page; 6] = [
        Page::General,
        Page::Connection,
        Page::Totp,
        Page::Terminal,
        Page::Cli,
        Page::Tags,
    ];

    fn title(self) -> SharedString {
        match self {
            Page::General => t!("host.page.general"),
            Page::Connection => t!("host.page.connection"),
            Page::Totp => t!("host.page.totp"),
            Page::Terminal => t!("host.page.terminal"),
            Page::Cli => t!("host.page.cli"),
            Page::Tags => t!("host.page.tags"),
        }
    }

    /// 这个协议有的分页。
    fn for_protocol(protocol: Protocol) -> &'static [Page] {
        match protocol {
            Protocol::Ssh => &Page::ALL,
            Protocol::Telnet => &[Page::General, Page::Connection, Page::Terminal, Page::Tags],
            Protocol::Serial => &[Page::General, Page::Terminal, Page::Tags],
        }
    }

    fn icon(self) -> IconName {
        match self {
            Page::General => IconName::Server,
            Page::Connection => IconName::Network,
            Page::Totp => IconName::KeyRound,
            Page::Terminal => IconName::SquareTerminal,
            Page::Cli => IconName::Bot,
            Page::Tags => IconName::FileText,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthKind {
    Password,
    Key,
    Agent,
    Profile,
    Inherit,
}

/// 代理的选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProxyChoice {
    /// 沿用分组的代理命令（分组没有设置时不使用代理）
    Inherit,
    /// 不使用代理，也不沿用分组的代理命令
    Off,
    Kind(ProxyKind),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum JumpChoice {
    /// 沿用分组的（没有时直连）
    Inherit,
    Direct,
    Via(HostId),
}

enum TestState {
    Idle,
    Running,
    Passed(SharedString),
    Failed(SharedString),
}

/// 已经保存过的秘密（编辑时留空表示保持不变）。
#[derive(Default)]
struct Saved {
    password: bool,
    private_key: bool,
    passphrase: bool,
    totp: bool,
    proxy_password: bool,
    cli_password: bool,
    cli_private_key: bool,
    cli_passphrase: bool,
}

/// 保存前要检查的粘贴的私钥（所在的分页、私钥、私钥密码）。解密要跑 KDF，放到后台。
type KeyCheck = (Page, SecretString, SecretEdit);

struct HostForm {
    id: Option<HostId>,
    /// 编辑前的主机：对话框不管的字段（Telnet、串口的参数、不认识的字段）原样保留
    original: Host,
    store: Arc<Store>,
    page: Page,
    /// 左侧分页导航的焦点（见 [`crate::page_nav`]）
    nav_focus: gpui_kit::FocusHandle,
    protocol: Protocol,
    name: Entity<InputState>,
    address: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    auth: AuthKind,
    /// 新建时还没动过认证方式：换分组时自动在"沿用分组"与"密码"之间切换
    auth_touched: bool,
    password: Entity<InputState>,
    key: KeyFields,
    profile: Entity<ChoiceState<Option<ProfileId>>>,
    group: Entity<ChoiceState<Option<GroupId>>>,
    inherited: Inherited,
    jump: Entity<ChoiceState<JumpChoice>>,
    proxy_kind: Entity<ChoiceState<ProxyChoice>>,
    proxy_address: Entity<InputState>,
    proxy_port: Entity<InputState>,
    proxy_username: Entity<InputState>,
    proxy_password: Entity<InputState>,
    proxy_command: Entity<InputState>,
    agent_forward: bool,
    legacy: bool,
    single_channel: bool,
    x11_forward: bool,
    x11_trusted: bool,
    totp_enabled: bool,
    totp_secret: Entity<InputState>,
    /// 当前验证码与剩余秒数，或密钥不对的原因
    totp_preview: Option<Result<(String, u64), SharedString>>,
    scheme: Entity<ChoiceState<String>>,
    encoding: Entity<ChoiceState<Encoding>>,
    /// SFTP 文件名的编码（SSH）
    sftp_encoding: Entity<ChoiceState<Encoding>>,
    startup: Entity<TextareaState>,
    tmux: bool,
    /// 连接后自动录制
    record: bool,
    /// Telnet、串口的行规程（按当前协议解析过默认值）
    enter: Enter,
    local_echo: LocalEcho,
    backspace: Backspace,
    term_type: Entity<InputState>,
    /// 串口名（与 SSH、Telnet 的地址分开，切换协议时各自保留）
    serial_port: Entity<InputState>,
    baud: Entity<InputState>,
    data_bits: Entity<ChoiceState<u8>>,
    parity: Entity<ChoiceState<Parity>>,
    stop_bits: Entity<ChoiceState<StopBits>>,
    flow_control: Entity<ChoiceState<FlowControl>>,
    /// 检测到的串口；`None` 为还没有检测
    ports: Option<Vec<PortInfo>>,
    cli_allowed: bool,
    cli_separate: bool,
    cli_username: Entity<InputState>,
    cli_password_auth: bool,
    cli_password: Entity<InputState>,
    cli_key: KeyFields,
    tags: Entity<InputState>,
    note: Entity<TextareaState>,
    favorite: bool,
    saved: Saved,
    error: Option<SharedString>,
    /// 正在后台检查粘贴的私钥，检查完再保存
    saving: bool,
    /// 所在的对话框（后台检查完私钥、保存后只关它）
    dialog: Option<DialogToken>,
    /// 读入证书文件失败等立即执行的操作的说明（底部按钮的左边，见 [`crate::notify::show`]）
    notice: Entity<NoticeLine>,
    test: TestState,
    prompter: Prompter,
    _test: Option<Task<()>>,
    _save: Option<Task<()>>,
    _totp: Option<Task<()>>,
    _ports: Option<Task<()>>,
    _prompts: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl HostForm {
    fn new(
        id: Option<HostId>,
        group: Option<GroupId>,
        prefill: Option<(Host, Option<SecretString>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = AppState::store(cx);
        let (prefill, prefill_password) = prefill.unzip();
        let original = id
            .as_ref()
            .and_then(|id| store.get::<Host>(id))
            .map(|r| r.doc)
            .or(prefill)
            .unwrap_or_default();
        let owner = id.as_ref().map(Owner::host);
        let has = |field: SecretField| owner.as_ref().is_some_and(|o| store.has_secret(o, field));
        let saved = Saved {
            password: has(SecretField::Password),
            private_key: has(SecretField::PrivateKey),
            passphrase: has(SecretField::Passphrase),
            totp: has(SecretField::Totp),
            proxy_password: has(SecretField::ProxyPassword),
            cli_password: has(SecretField::CliPassword),
            cli_private_key: has(SecretField::CliPrivateKey),
            cli_passphrase: has(SecretField::CliPassphrase),
        };
        let inherited = inherit::inherited(&store, group.as_ref());
        let input = |value: String,
                     placeholder: SharedString,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let masked = |placeholder: SharedString, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(placeholder)
            })
        };

        let h = &original;
        let auth = match &h.auth {
            Auth::Password => AuthKind::Password,
            Auth::Key { .. } => AuthKind::Key,
            Auth::Agent => AuthKind::Agent,
            Auth::Profile { .. } => AuthKind::Profile,
            Auth::Inherit if inherited.profile.is_some() => AuthKind::Inherit,
            Auth::Inherit => AuthKind::Password,
        };
        let auth = if id.is_none() && inherited.profile.is_some() {
            AuthKind::Inherit
        } else {
            auth
        };
        let key_source = match &h.auth {
            Auth::Key { source } => Some(source),
            _ => None,
        };
        let profile_selected = match &h.auth {
            Auth::Profile { profile } => Some(profile.clone()),
            _ => None,
        };
        let jump_selected = match &h.jump {
            Jump::Inherit => JumpChoice::Inherit,
            Jump::Direct => JumpChoice::Direct,
            Jump::Via { host } => JumpChoice::Via(host.clone()),
        };
        let proxy = h.proxy.clone();
        let proxy_selected = match (&proxy, h.no_proxy) {
            (Some(p), _) => ProxyChoice::Kind(p.kind),
            (None, true) => ProxyChoice::Off,
            (None, false) => ProxyChoice::Inherit,
        };
        let cli_credential = h.cli.credential.clone();
        let (cli_password_auth, cli_key_source) = match cli_credential.as_ref().map(|c| &c.auth) {
            Some(CliAuth::Key { source }) => (false, Some(source.clone())),
            _ => (true, None),
        };

        let keep = |saved: bool| {
            if saved {
                t!("host.secret.keep")
            } else {
                SharedString::default()
            }
        };
        let name = input(
            h.name.clone(),
            t!("host.field.name_placeholder"),
            window,
            cx,
        );
        let serial_host = h.protocol == Protocol::Serial;
        let address = input(
            if serial_host {
                String::new()
            } else {
                h.address.clone()
            },
            SharedString::default(),
            window,
            cx,
        );
        let port = input(
            if h.port == 0 {
                String::new()
            } else {
                h.port.to_string()
            },
            default_port_text(h.protocol),
            window,
            cx,
        );
        let username = input(h.username.clone(), SharedString::default(), window, cx);
        let password = masked(
            password_placeholder(h.protocol, saved.password && auth == AuthKind::Password),
            window,
            cx,
        );
        let key = KeyFields::new(
            key_source,
            saved.private_key,
            saved.passphrase,
            &store,
            window,
            cx,
        );
        let profile = choice_state(profile_choices(&store), &profile_selected, true, window, cx);
        let group_state = choice_state(group_choices(&store), &group, true, window, cx);
        let jump = choice_state(
            Self::jump_choices(&store, id.as_ref(), &inherited),
            &jump_selected,
            true,
            window,
            cx,
        );
        let proxy_kind = choice_state(
            proxy_choices(&inherited),
            &proxy_selected,
            false,
            window,
            cx,
        );
        let p = proxy.clone().unwrap_or_default();
        let proxy_address = input(p.address.clone(), t!("host.proxy.address"), window, cx);
        let proxy_port = input(
            if p.port == 0 {
                String::new()
            } else {
                p.port.to_string()
            },
            t!("host.field.port"),
            window,
            cx,
        );
        let proxy_username = input(p.username.clone(), t!("host.proxy.username"), window, cx);
        let proxy_password = masked(
            if saved.proxy_password {
                t!("host.secret.keep")
            } else {
                t!("host.proxy.password")
            },
            window,
            cx,
        );
        let proxy_command = input(
            p.command.clone(),
            t!("host.proxy.command_placeholder"),
            window,
            cx,
        );
        let totp_secret = masked(
            if saved.totp {
                t!("host.totp.keep")
            } else {
                t!("host.totp.placeholder")
            },
            window,
            cx,
        );
        let scheme = choice_state(
            Self::scheme_choices(&store, group.as_ref()),
            &h.terminal.color_scheme,
            true,
            window,
            cx,
        );
        let encoding = choice_state(encoding_choices(), &h.terminal.encoding, false, window, cx);
        let sftp_encoding =
            choice_state(encoding_choices(), &h.ssh.sftp_encoding, false, window, cx);
        let startup = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(4)
                .placeholder(t!("host.terminal.startup_placeholder"))
                .default_value(h.terminal.startup.clone())
        });
        let term_type = input(
            h.telnet.term_type.clone(),
            "xterm-256color".into(),
            window,
            cx,
        );
        let serial_port = input(
            if serial_host {
                h.address.clone()
            } else {
                String::new()
            },
            t!("host.serial.port_placeholder"),
            window,
            cx,
        );
        let baud = input(h.serial.baud.to_string(), "115200".into(), window, cx);
        let data_bits = choice_state(
            [8u8, 7, 6, 5]
                .map(|bits| Choice::new(bits.to_string(), bits))
                .to_vec(),
            &h.serial.data_bits,
            false,
            window,
            cx,
        );
        let parity = choice_state(parity_choices(), &h.serial.parity, false, window, cx);
        let stop_bits = choice_state(stop_bits_choices(), &h.serial.stop_bits, false, window, cx);
        let flow_control = choice_state(
            flow_control_choices(),
            &h.serial.flow_control,
            false,
            window,
            cx,
        );
        let cli_username = input(
            cli_credential
                .as_ref()
                .map(|c| c.username.clone())
                .unwrap_or_default(),
            t!("host.cli.username_placeholder"),
            window,
            cx,
        );
        let cli_password = masked(keep(saved.cli_password), window, cx);
        let cli_key = KeyFields::new(
            cli_key_source.as_ref(),
            saved.cli_private_key,
            saved.cli_passphrase,
            &store,
            window,
            cx,
        );
        let tags = input(h.tags.join(", "), t!("host.tags.placeholder"), window, cx);
        let note = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(5)
                .default_value(h.note.clone())
        });

        // 测试连接时的询问（首次连接确认指纹等）：在这个窗口里逐个弹出
        let (prompt_tx, prompt_rx) = async_channel::unbounded::<PromptRequest>();
        let prompts = cx.spawn_in(window, async move |_, cx| {
            while let Ok(request) = prompt_rx.recv().await {
                let Ok(done) = cx.update(|window, cx| prompt_dialog::open(request, window, cx))
                else {
                    break;
                };
                let _ = done.await;
            }
        });

        let mut subscriptions = vec![
            cx.subscribe_in(
                &group_state,
                window,
                |this, _, _: &SelectEvent<_>, window, cx| this.on_group_changed(window, cx),
            ),
            cx.subscribe_in(
                &proxy_kind,
                window,
                |this, _, _: &SelectEvent<_>, window, cx| this.on_proxy_kind_changed(window, cx),
            ),
            cx.subscribe_in(
                &profile,
                window,
                |this, _, _: &SelectEvent<_>, window, cx| {
                    this.refresh_username_placeholder(window, cx);
                    this.clear_test(cx);
                },
            ),
            cx.subscribe_in(
                &totp_secret,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.schedule_totp_preview(cx);
                    }
                },
            ),
        ];
        // 改了与连接有关的内容：清掉上次的测试结果（串口名还要更新检测到的端口中高亮的一个）
        for input in [
            &address,
            &port,
            &username,
            &password,
            &proxy_address,
            &proxy_port,
            &proxy_command,
            &serial_port,
            &baud,
        ] {
            subscriptions.push(cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.clear_test(cx);
                    cx.notify();
                }
            }));
        }
        subscriptions.push(cx.subscribe(&data_bits, |this, _, _: &SelectEvent<_>, cx| {
            this.clear_test(cx)
        }));
        subscriptions.push(cx.subscribe(&parity, |this, _, _: &SelectEvent<_>, cx| {
            this.clear_test(cx)
        }));
        subscriptions.push(cx.subscribe(&stop_bits, |this, _, _: &SelectEvent<_>, cx| {
            this.clear_test(cx)
        }));
        subscriptions.push(
            cx.subscribe(&flow_control, |this, _, _: &SelectEvent<_>, cx| {
                this.clear_test(cx)
            }),
        );

        let mut form = Self {
            id,
            store,
            page: Page::General,
            nav_focus: cx.focus_handle().tab_stop(true),
            protocol: h.protocol,
            name,
            address,
            port,
            username,
            auth,
            auth_touched: false,
            password,
            key,
            profile,
            group: group_state,
            inherited,
            jump,
            proxy_kind,
            proxy_address,
            proxy_port,
            proxy_username,
            proxy_password,
            proxy_command,
            agent_forward: h.ssh.agent_forward,
            legacy: h.ssh.legacy_algorithms,
            single_channel: h.ssh.single_channel,
            x11_forward: h.ssh.x11_forward,
            x11_trusted: h.ssh.x11_trusted,
            totp_enabled: saved.totp,
            totp_secret,
            totp_preview: None,
            scheme,
            encoding,
            sftp_encoding,
            startup,
            tmux: h.terminal.tmux,
            record: h.terminal.record,
            enter: h.line.enter_for(h.protocol),
            local_echo: h.line.local_echo_for(h.protocol),
            backspace: h.line.backspace,
            term_type,
            serial_port,
            baud,
            data_bits,
            parity,
            stop_bits,
            flow_control,
            ports: None,
            cli_allowed: h.cli.allowed,
            cli_separate: cli_credential.is_some(),
            cli_username,
            cli_password_auth,
            cli_password,
            cli_key,
            tags,
            note,
            favorite: h.favorite,
            saved,
            error: None,
            saving: false,
            dialog: None,
            notice: NoticeLine::new(cx),
            test: TestState::Idle,
            prompter: prompt_dialog::prompter(prompt_tx),
            _test: None,
            _save: None,
            _totp: None,
            _ports: None,
            _prompts: prompts,
            _subscriptions: subscriptions,
            original,
        };
        form.refresh_username_placeholder(window, cx);
        if let Some(password) = prefill_password.flatten() {
            form.password.update(cx, |s, cx| {
                s.set_value(password.expose().to_string(), window, cx)
            });
        }
        if form.protocol == Protocol::Serial {
            form.detect_ports(cx);
        }
        form
    }

    /// 跳板机的选项：第一项是"沿用分组"（分组有默认值时写出它）或"不使用"，有分组默认值时多一项"直连"。
    fn jump_choices(
        store: &Store,
        exclude: Option<&HostId>,
        inherited: &Inherited,
    ) -> Vec<Choice<JumpChoice>> {
        let inherited_name = inherited
            .jump
            .as_ref()
            .filter(|j| Some(&j.value) != exclude)
            .and_then(|j| {
                store
                    .get::<Host>(&j.value)
                    .map(|h| (j.group_name.clone(), h.doc.name))
            });
        let mut choices = match inherited_name {
            Some((group, name)) => vec![
                Choice::new(
                    t!("host.jump.inherit", group = group, name = name),
                    JumpChoice::Inherit,
                ),
                Choice::new(t!("host.jump.direct_override"), JumpChoice::Direct),
            ],
            None => vec![Choice::new(t!("host.jump.none"), JumpChoice::Inherit)],
        };
        choices.extend(jump_hosts(store, exclude).into_iter().filter_map(|c| {
            c.value.map(|id| Choice {
                title: c.title,
                value: JumpChoice::Via(id),
            })
        }));
        choices
    }

    /// 终端配色的选项；第一项（不指定）写明沿用哪个分组的哪套配色。
    fn scheme_choices(store: &Store, group: Option<&GroupId>) -> Vec<Choice<String>> {
        let empty = match inherit_scheme(store, group) {
            Some((group_name, scheme)) => t!(
                "host.terminal.scheme_inherit",
                group = group_name,
                name = scheme
            ),
            None => t!("group.field.scheme_global"),
        };
        scheme_choices(store, empty)
    }

    fn on_group_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let group = chosen(&self.group, cx).flatten();
        self.inherited = inherit::inherited(&self.store, group.as_ref());
        // 终端配色的"沿用分组…"一项跟着分组变
        let scheme = chosen(&self.scheme, cx).unwrap_or_default();
        let choices = Self::scheme_choices(&self.store, group.as_ref());
        self.scheme.update(cx, |state, cx| {
            state.set_items(
                gpui_kit::component::searchable_list::SearchableVec::new(choices),
                window,
                cx,
            );
            state.set_selected_value(&scheme, window, cx);
        });
        // 跳板机的"沿用"一项跟着分组变
        let current = chosen(&self.jump, cx).unwrap_or(JumpChoice::Inherit);
        let choices = Self::jump_choices(&self.store, self.id.as_ref(), &self.inherited);
        self.jump.update(cx, |state, cx| {
            state.set_items(
                gpui_kit::component::searchable_list::SearchableVec::new(choices),
                window,
                cx,
            );
            state.set_selected_value(&current, window, cx);
        });
        // 代理的"沿用分组"一项也跟着分组变
        let current = chosen(&self.proxy_kind, cx).unwrap_or(ProxyChoice::Inherit);
        let choices = proxy_choices(&self.inherited);
        self.proxy_kind.update(cx, |state, cx| {
            state.set_items(
                gpui_kit::component::searchable_list::SearchableVec::new(choices),
                window,
                cx,
            );
            state.set_selected_value(&current, window, cx);
        });
        if self.id.is_none() && !self.auth_touched {
            self.auth = if self.inherited.profile.is_some() {
                AuthKind::Inherit
            } else {
                AuthKind::Password
            };
        }
        if self.auth == AuthKind::Inherit && self.inherited.profile.is_none() {
            self.auth = AuthKind::Password;
        }
        self.refresh_username_placeholder(window, cx);
        self.clear_test(cx);
        cx.notify();
    }

    /// 换协议：端口还是某个协议的常用端口时改用新协议的；回车键、本地回显还是原协议的默认值时改用新协议的
    /// （用户改过的保留）。当前页新协议没有时回到常规页。
    fn set_protocol(&mut self, protocol: Protocol, window: &mut Window, cx: &mut Context<Self>) {
        if protocol == self.protocol {
            return;
        }
        let old = self.protocol;
        let port = self.port.read(cx).value().trim().to_string();
        if matches!(port.as_str(), "" | "22" | "23") {
            self.port.update(cx, |s, cx| {
                s.set_value("", window, cx);
                s.set_placeholder(default_port_text(protocol), window, cx);
            });
        }
        let defaults = LineOptions::default();
        if self.enter == defaults.enter_for(old) {
            self.enter = defaults.enter_for(protocol);
        }
        if self.local_echo == defaults.local_echo_for(old) {
            self.local_echo = defaults.local_echo_for(protocol);
        }
        // 串口没有"自动"
        if protocol == Protocol::Serial && self.local_echo == LocalEcho::Auto {
            self.local_echo = LocalEcho::Off;
        }
        self.protocol = protocol;
        if !Page::for_protocol(protocol).contains(&self.page) {
            self.page = Page::General;
        }
        let keep =
            self.saved.password && (protocol != Protocol::Ssh || self.auth == AuthKind::Password);
        self.password.update(cx, |s, cx| {
            s.set_placeholder(password_placeholder(protocol, keep), window, cx)
        });
        if protocol == Protocol::Serial && self.ports.is_none() {
            self.detect_ports(cx);
        }
        self.error = None;
        self.refresh_username_placeholder(window, cx);
        self.test = TestState::Idle;
        cx.notify();
    }

    /// 检测本机的串口（读注册表或 /dev，在后台进行）；失败时按没有串口处理。
    fn detect_ports(&mut self, cx: &mut Context<Self>) {
        self._ports = Some(cx.spawn(async move |this, cx| {
            let ports = cx
                .background_executor()
                .spawn(async move {
                    serial::list_ports().unwrap_or_else(|e| {
                        tracing::warn!("列出串口失败：{e}");
                        Vec::new()
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.ports = Some(ports);
                cx.notify();
            });
        }));
    }

    /// 选用检测到的串口。
    fn pick_port(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.serial_port
            .update(cx, |s, cx| s.set_value(name.to_string(), window, cx));
        self.clear_test(cx);
        cx.notify();
    }

    /// 用户名可以沿用时，占位文字写出会用哪个（"不填时用 …"）。
    fn refresh_username_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = match self.inherited_username(cx) {
            Some(name) => t!("host.field.username_inherit", name = name),
            None => SharedString::default(),
        };
        self.username
            .update(cx, |s, cx| s.set_placeholder(placeholder, window, cx));
    }

    /// 换代理类型时：端口为空或仍是某种类型的默认端口，就换成新类型的默认端口。
    fn on_proxy_kind_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ProxyChoice::Kind(kind)) = chosen(&self.proxy_kind, cx) {
            let port = self.proxy_port.read(cx).value().trim().to_string();
            let defaults = ["", "1080", "8080", "443"];
            if defaults.contains(&port.as_str())
                && let Some(port) = default_proxy_port(kind)
            {
                self.proxy_port
                    .update(cx, |s, cx| s.set_value(port.to_string(), window, cx));
            }
        }
        self.clear_test(cx);
        cx.notify();
    }

    fn clear_test(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.test, TestState::Idle | TestState::Running) {
            self.test = TestState::Idle;
            cx.notify();
        }
    }

    /// 停止输入片刻后显示当前验证码，供与手机上的验证器对照；之后每秒更新剩余时间。
    fn schedule_totp_preview(&mut self, cx: &mut Context<Self>) {
        self._totp = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            loop {
                let alive = this.update(cx, |this, cx| {
                    let secret = this.totp_secret.read(cx).value().to_string();
                    this.totp_preview = if secret.trim().is_empty() {
                        None
                    } else {
                        Some(
                            Totp::parse(&secret)
                                .map(|totp| (totp.code_now(), totp.remaining_now()))
                                .map_err(|e| totp_error(e.reason)),
                        )
                    };
                    cx.notify();
                    matches!(this.totp_preview, Some(Ok(_)))
                });
                match alive {
                    Ok(true) => cx.background_executor().timer(Duration::from_secs(1)).await,
                    _ => break,
                }
            }
        }));
    }

    // ------------------------------------------------------------ 收集与保存

    /// 能沿用的用户名（凭据档案或分组默认值），用户名留空时用它。
    fn inherited_username(&self, cx: &App) -> Option<String> {
        match self.protocol {
            Protocol::Ssh => {}
            Protocol::Telnet => return self.inherited.username.as_ref().map(|u| u.value.clone()),
            Protocol::Serial => return None,
        }
        let profile = match self.auth {
            AuthKind::Profile => chosen(&self.profile, cx).flatten(),
            AuthKind::Inherit => self.inherited.profile.as_ref().map(|p| p.value.clone()),
            _ => None,
        };
        profile
            .and_then(|id| self.store.get::<Profile>(&id))
            .map(|p| p.doc.username)
            .filter(|u| !u.trim().is_empty())
            .or_else(|| self.inherited.username.as_ref().map(|u| u.value.clone()))
    }

    /// 按填写的内容生成主机、所在分组与秘密的修改；填写不完整时返回说明（并切到对应的页）。
    fn collect(
        &mut self,
        cx: &App,
    ) -> Result<(Host, Option<GroupId>, SecretEdits), (Page, SharedString)> {
        let mut host = self.original.clone();
        let mut secrets = SecretEdits::default();
        host.protocol = self.protocol;
        host.name = self.name.read(cx).value().trim().to_string();
        match self.protocol {
            Protocol::Ssh => self.collect_ssh(&mut host, &mut secrets, cx)?,
            Protocol::Telnet => self.collect_telnet(&mut host, &mut secrets, cx)?,
            Protocol::Serial => self.collect_serial(&mut host, &mut secrets, cx)?,
        }
        if self.protocol != Protocol::Ssh {
            // 只有 SSH 用得到的秘密：换成 Telnet、串口后删掉
            secrets.private_key = clear_if(self.saved.private_key);
            secrets.passphrase = clear_if(self.saved.passphrase);
            secrets.totp = clear_if(self.saved.totp);
            secrets.cli_password = clear_if(self.saved.cli_password);
            secrets.cli_private_key = clear_if(self.saved.cli_private_key);
            secrets.cli_passphrase = clear_if(self.saved.cli_passphrase);
            host.line = LineOptions {
                enter: Some(self.enter)
                    .filter(|e| *e != LineOptions::default().enter_for(self.protocol)),
                local_echo: Some(self.local_echo)
                    .filter(|e| *e != LineOptions::default().local_echo_for(self.protocol)),
                backspace: self.backspace,
            };
        }

        // 终端
        host.terminal.color_scheme = chosen(&self.scheme, cx).unwrap_or_default();
        host.terminal.encoding = chosen(&self.encoding, cx).unwrap_or_default();

        // 标签与备注
        host.tags = self
            .tags
            .read(cx)
            .value()
            .split([',', '，'])
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        host.note = self.note.read(cx).value().to_string();
        host.favorite = self.favorite;
        Ok((host, chosen(&self.group, cx).flatten(), secrets))
    }

    /// SSH 主机的地址、认证、连接、验证码、登录后执行的命令与命令行工具。
    fn collect_ssh(
        &self,
        host: &mut Host,
        secrets: &mut SecretEdits,
        cx: &App,
    ) -> Result<(), (Page, SharedString)> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        // 密码、私钥密码原样保存（首尾的空格也是密码的一部分，与连接时的询问框一致）
        let secret = |input: &Entity<InputState>| input.read(cx).value().to_string();
        self.collect_address(host, cx)?;
        if host.username.is_empty() && self.inherited_username(cx).is_none() {
            return Err((Page::General, t!("host.error.username_required")));
        }

        // 认证；换成别的方式后不再需要的秘密一并删除
        let password = secret(&self.password);
        secrets.password = if self.auth == AuthKind::Password {
            secret_edit(&password)
        } else {
            clear_if(self.saved.password)
        };
        host.auth = match self.auth {
            AuthKind::Password => Auth::Password,
            AuthKind::Agent => Auth::Agent,
            AuthKind::Inherit => Auth::Inherit,
            AuthKind::Profile => Auth::Profile {
                profile: chosen(&self.profile, cx)
                    .flatten()
                    .ok_or((Page::General, t!("host.error.profile_required")))?,
            },
            AuthKind::Key => Auth::Key {
                source: self
                    .key
                    .collect(&mut secrets.private_key, cx)
                    .map_err(|e| (Page::General, e))?,
            },
        };
        let uses_own_key = matches!(&host.auth, Auth::Key { source } if !matches!(source, KeySource::Library { .. }));
        // 不再用粘贴的私钥：删掉保存的那份
        let pasted = matches!(
            &host.auth,
            Auth::Key {
                source: KeySource::Pasted { .. }
            }
        );
        if !pasted && self.saved.private_key {
            secrets.private_key = SecretEdit::Clear;
        }
        secrets.passphrase = if uses_own_key {
            secret_edit(&secret(&self.key.passphrase))
        } else {
            clear_if(self.saved.passphrase)
        };

        // 连接
        self.collect_route(host, secrets, cx)?;
        host.ssh.agent_forward = self.agent_forward;
        host.ssh.legacy_algorithms = self.legacy;
        host.ssh.single_channel = self.single_channel;
        host.ssh.x11_forward = self.x11_forward;
        host.ssh.x11_trusted = self.x11_trusted;
        host.ssh.sftp_encoding = chosen(&self.sftp_encoding, cx).unwrap_or_default();

        // 验证码
        let totp = value(&self.totp_secret);
        secrets.totp = if !self.totp_enabled {
            clear_if(self.saved.totp)
        } else if totp.is_empty() {
            if !self.saved.totp {
                return Err((Page::Totp, t!("host.error.totp_required")));
            }
            SecretEdit::Keep
        } else {
            Totp::parse(&totp).map_err(|e| (Page::Totp, totp_error(e.reason)))?;
            SecretEdit::Set(SecretString::new(totp))
        };

        // 终端
        host.terminal.startup = self.startup.read(cx).value().to_string();
        host.terminal.tmux = self.tmux;
        host.terminal.record = self.record;

        // 命令行工具：没有允许时不动专用凭据（关掉后再打开还在）
        host.cli.allowed = self.cli_allowed;
        if self.cli_allowed {
            if self.cli_separate {
                let auth = if self.cli_password_auth {
                    secrets.cli_password = secret_edit(&secret(&self.cli_password));
                    secrets.cli_private_key = clear_if(self.saved.cli_private_key);
                    secrets.cli_passphrase = clear_if(self.saved.cli_passphrase);
                    CliAuth::Password
                } else {
                    secrets.cli_password = clear_if(self.saved.cli_password);
                    let source = self
                        .cli_key
                        .collect(&mut secrets.cli_private_key, cx)
                        .map_err(|e| (Page::Cli, e))?;
                    if !matches!(source, KeySource::Pasted { .. }) {
                        secrets.cli_private_key = clear_if(self.saved.cli_private_key);
                    }
                    // 密钥库中的私钥已解密保存，不需要私钥密码
                    secrets.cli_passphrase = if matches!(source, KeySource::Library { .. }) {
                        clear_if(self.saved.cli_passphrase)
                    } else {
                        secret_edit(&secret(&self.cli_key.passphrase))
                    };
                    CliAuth::Key { source }
                };
                let missing = matches!(auth, CliAuth::Password)
                    && matches!(secrets.cli_password, SecretEdit::Keep)
                    && !self.saved.cli_password;
                if missing {
                    return Err((Page::Cli, t!("host.error.cli_credential_required")));
                }
                host.cli.credential = Some(CliCredential {
                    username: value(&self.cli_username),
                    auth,
                });
            } else {
                host.cli.credential = None;
                secrets.cli_password = clear_if(self.saved.cli_password);
                secrets.cli_private_key = clear_if(self.saved.cli_private_key);
                secrets.cli_passphrase = clear_if(self.saved.cli_passphrase);
            }
        } else {
            host.cli = CliAccess {
                allowed: false,
                credential: self.original.cli.credential.clone(),
            };
        }
        Ok(())
    }

    /// Telnet 主机：地址、自动登录的用户名与密码（都可以不填）、跳板机与代理、终端类型。
    fn collect_telnet(
        &self,
        host: &mut Host,
        secrets: &mut SecretEdits,
        cx: &App,
    ) -> Result<(), (Page, SharedString)> {
        self.collect_address(host, cx)?;
        host.auth = Auth::Password;
        secrets.password = secret_edit(&self.password.read(cx).value());
        self.collect_route(host, secrets, cx)?;
        host.telnet.term_type = self.term_type.read(cx).value().trim().to_string();
        Ok(())
    }

    /// 串口主机：串口名与参数；检测到的设备记下 USB 标识（换了 USB 口时据此找回）。
    fn collect_serial(
        &self,
        host: &mut Host,
        secrets: &mut SecretEdits,
        cx: &App,
    ) -> Result<(), (Page, SharedString)> {
        let port = self.serial_port.read(cx).value().trim().to_string();
        if port.is_empty() {
            return Err((Page::General, t!("host.error.serial_port_required")));
        }
        let baud = self.baud.read(cx).value().trim().to_string();
        let baud = if baud.is_empty() {
            0
        } else {
            baud.parse::<u32>()
                .ok()
                .filter(|b| termlet_core::model::SERIAL_BAUD_RANGE.contains(b))
                .ok_or((Page::General, t!("host.error.baud")))?
        };
        let found = self
            .ports
            .iter()
            .flatten()
            .find(|p| p.name.eq_ignore_ascii_case(&port));
        let usb = match found {
            Some(found) => found.usb.clone(),
            None if self.original.protocol == Protocol::Serial
                && self.original.address.eq_ignore_ascii_case(&port) =>
            {
                self.original.serial.usb.clone()
            }
            None => None,
        };
        host.address = port;
        host.port = 0;
        host.username.clear();
        host.auth = Auth::Password;
        host.jump = Jump::Direct;
        host.proxy = None;
        host.no_proxy = false;
        secrets.password = clear_if(self.saved.password);
        secrets.proxy_password = clear_if(self.saved.proxy_password);
        let serial = &mut host.serial;
        serial.baud = baud;
        serial.data_bits = chosen(&self.data_bits, cx).unwrap_or(8);
        serial.parity = chosen(&self.parity, cx).unwrap_or_default();
        serial.stop_bits = chosen(&self.stop_bits, cx).unwrap_or_default();
        serial.flow_control = chosen(&self.flow_control, cx).unwrap_or_default();
        serial.usb = usb;
        Ok(())
    }

    /// SSH、Telnet 的地址、端口与用户名。
    fn collect_address(&self, host: &mut Host, cx: &App) -> Result<(), (Page, SharedString)> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let address = value(&self.address);
        if address.is_empty() {
            return Err((Page::General, t!("host.error.address_required")));
        }
        host.address = address;
        host.port = parse_port(&value(&self.port)).ok_or((Page::General, t!("host.error.port")))?;
        host.username = value(&self.username);
        Ok(())
    }

    /// 跳板机与代理（SSH、Telnet）。
    fn collect_route(
        &self,
        host: &mut Host,
        secrets: &mut SecretEdits,
        cx: &App,
    ) -> Result<(), (Page, SharedString)> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        host.jump = match chosen(&self.jump, cx).unwrap_or(JumpChoice::Inherit) {
            JumpChoice::Inherit => Jump::Inherit,
            JumpChoice::Direct => Jump::Direct,
            JumpChoice::Via(id) => Jump::Via { host: id },
        };
        let choice = chosen(&self.proxy_kind, cx).unwrap_or(ProxyChoice::Inherit);
        host.no_proxy = choice == ProxyChoice::Off;
        host.proxy = match choice {
            ProxyChoice::Inherit | ProxyChoice::Off => {
                secrets.proxy_password = clear_if(self.saved.proxy_password);
                None
            }
            ProxyChoice::Kind(kind) => {
                let username = value(&self.proxy_username);
                // SOCKS4 没有密码；没填用户名时不保存密码
                secrets.proxy_password = if kind == ProxyKind::Socks4 || username.is_empty() {
                    clear_if(self.saved.proxy_password)
                } else {
                    secret_edit(&self.proxy_password.read(cx).value())
                };
                let port = value(&self.proxy_port);
                Some(Proxy {
                    kind,
                    address: value(&self.proxy_address),
                    port: if kind == ProxyKind::Command {
                        0
                    } else {
                        port.parse()
                            .map_err(|_| (Page::Connection, t!("store.invalid.proxy_port")))?
                    },
                    username: if kind == ProxyKind::Command {
                        String::new()
                    } else {
                        username
                    },
                    command: if kind == ProxyKind::Command {
                        value(&self.proxy_command)
                    } else {
                        String::new()
                    },
                })
            }
        };
        Ok(())
    }

    /// 保存；成功时返回 true（关闭对话框）。有粘贴的私钥时先在后台检查，返回 false，检查通过后保存并关闭。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.saving {
            return false;
        }
        let (host, group, secrets) = match self.collect(cx) {
            Ok(collected) => collected,
            Err((page, message)) => {
                self.page = page;
                self.error = Some(message);
                cx.notify();
                return false;
            }
        };
        let checks = key_checks(&host, &secrets);
        if checks.is_empty() {
            let saved = self.write(host, group, secrets, cx);
            if saved {
                self.reveal(window, cx);
            }
            return saved;
        }
        self.saving = true;
        self.error = None;
        cx.notify();
        let job = cx.background_spawn(async move {
            checks.into_iter().find_map(|(page, text, passphrase)| {
                key_fields::check_private_key(&text, &passphrase)
                    .err()
                    .map(|e| (page, e))
            })
        });
        self._save = Some(cx.spawn_in(window, async move |this, cx| {
            let failed = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                if let Some((page, message)) = failed {
                    this.page = page;
                    this.error = Some(message);
                    cx.notify();
                } else if this.write(host, group, secrets, cx) {
                    this.saving = true;
                    this.reveal(window, cx);
                    close_own(this.dialog.as_ref(), window, cx);
                }
            });
        }));
        false
    }

    /// 保存后在侧边栏中展开主机所在的各级分组并选中它，看得到存到了哪里（分组默认折叠）。
    fn reveal(&self, window: &Window, cx: &mut App) {
        if let Some(id) = self.id.clone() {
            crate::workspace::Workspace::reveal_in_sidebar(
                crate::sidebar::rows::RowKey::Host {
                    id,
                    favorite: false,
                },
                window,
                cx,
            );
        }
    }

    /// 写进存储；成功时返回 true。
    fn write(
        &mut self,
        host: Host,
        group: Option<GroupId>,
        secrets: SecretEdits,
        cx: &mut Context<Self>,
    ) -> bool {
        let parent = group.as_ref().map(|g| g.as_str().to_string());
        let creating = self.id.is_none();
        let (name, address, port, protocol) = (
            host.name.clone(),
            host.address.clone(),
            host.port,
            host.protocol,
        );
        let result = match &self.id {
            None => self
                .store
                .create(host, parent, Position::Last)
                .and_then(|id| {
                    // 先记下 ID：秘密写入失败后再点保存时改为更新，不会再建一台
                    self.id = Some(id.clone());
                    secrets.apply(&self.store, &Owner::host(&id))
                }),
            Some(id) => {
                let moved = self.store.get::<Host>(id).map(|r| r.parent) != Some(parent.clone());
                self.store
                    .update(id, host)
                    .and_then(|()| {
                        if moved {
                            self.store.move_to::<Host>(id, parent, Position::Last)
                        } else {
                            Ok(())
                        }
                    })
                    .and_then(|()| secrets.apply(&self.store, &Owner::host(id)))
            }
        };
        match result {
            Ok(()) => {
                tracing::info!(
                    host = ?self.id,
                    name = %name,
                    address = %address,
                    port,
                    ?protocol,
                    creating,
                    "已保存主机"
                );
                true
            }
            Err(e) => {
                tracing::warn!(host = ?self.id, creating, "保存主机失败：{e}");
                self.error = Some(store_errors::describe(&e));
                cx.notify();
                false
            }
        }
    }

    fn test_connection(&mut self, cx: &mut Context<Self>) {
        if matches!(self.test, TestState::Running) {
            return;
        }
        let (host, group, secrets) = match self.collect(cx) {
            Ok(collected) => collected,
            Err((page, message)) => {
                self.page = page;
                self.error = Some(message);
                cx.notify();
                return;
            }
        };
        self.error = None;
        if host.protocol == Protocol::Serial {
            self.test_serial(SerialConfig::for_host(&host), cx);
            return;
        }
        let draft = HostDraft {
            id: self.id.as_ref(),
            group: group.as_ref(),
            host: &host,
            secrets: &secrets,
        };
        let store = self.store.clone();
        let prompter = self.prompter.clone();
        let options = ssh::connect_options(&store.settings());
        // 两种协议的结果都转成要显示的文字
        let job = if host.protocol == Protocol::Telnet {
            let plan = match telnet::plan_for_draft(&self.store, &draft) {
                Ok(plan) => plan,
                Err(e) => {
                    self.test = TestState::Failed(ssh_errors::describe(&e));
                    cx.notify();
                    return;
                }
            };
            termlet_core::runtime::spawn(async move {
                telnet::test_connection(store, &plan, prompter, &options)
                    .await
                    .map(|elapsed| t!("host.test.passed_telnet", ms = elapsed.as_millis()))
            })
        } else {
            let plan = match ssh::plan_for_draft(&self.store, &draft) {
                Ok(plan) => plan,
                Err(e) => {
                    self.test = TestState::Failed(ssh_errors::describe(&e));
                    cx.notify();
                    return;
                }
            };
            termlet_core::runtime::spawn(async move {
                ssh::test_connection(store, &plan, prompter, &options)
                    .await
                    .map(|report| {
                        t!(
                            "host.test.passed",
                            ms = report.elapsed.as_millis(),
                            version = if report.server_version.is_empty() {
                                "-".to_string()
                            } else {
                                report.server_version
                            }
                        )
                    })
            })
        };
        self.test = TestState::Running;
        self._test = Some(cx.spawn(async move |this, cx| {
            let result = job
                .await
                .unwrap_or_else(|e| Err(ssh::SshError::Protocol(e.to_string())));
            let _ = this.update(cx, |this, cx| {
                this.test = match result {
                    Ok(message) => TestState::Passed(message),
                    Err(e) => TestState::Failed(ssh_errors::describe(&e)),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 串口的测试：按参数打开随即关闭（打开可能要等驱动，在后台线程进行）。
    fn test_serial(&mut self, config: SerialConfig, cx: &mut Context<Self>) {
        self.test = TestState::Running;
        self._test = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { serial::test_open(&config) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.test = match result {
                    Ok(_) => TestState::Passed(t!("host.test.passed_serial")),
                    Err(e) => TestState::Failed(raw_errors::describe_serial(&e)),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------ 渲染

    /// 分页上有没有设置（导航上的小圆点）。
    fn page_has_settings(&self, page: Page, cx: &App) -> bool {
        match page {
            Page::General => false,
            Page::Connection => {
                let ssh = self.protocol == Protocol::Ssh;
                matches!(chosen(&self.jump, cx), Some(JumpChoice::Via(_)))
                    || matches!(
                        chosen(&self.proxy_kind, cx),
                        Some(ProxyChoice::Kind(_) | ProxyChoice::Off)
                    )
                    || (ssh
                        && (self.agent_forward
                            || self.legacy
                            || self.single_channel
                            || self.x11_forward))
            }
            Page::Totp => self.totp_enabled,
            Page::Terminal => {
                let common = chosen(&self.encoding, cx).is_some_and(|e| e != Encoding::Utf8)
                    || chosen(&self.scheme, cx).is_some_and(|s| !s.is_empty())
                    || self.record;
                let defaults = LineOptions::default();
                common
                    || match self.protocol {
                        Protocol::Ssh => {
                            self.tmux
                                || !self.startup.read(cx).value().trim().is_empty()
                                || chosen(&self.sftp_encoding, cx)
                                    .is_some_and(|e| e != Encoding::Utf8)
                        }
                        protocol => {
                            self.enter != defaults.enter_for(protocol)
                                || self.local_echo != defaults.local_echo_for(protocol)
                                || self.backspace != Backspace::Del
                        }
                    }
            }
            Page::Cli => self.cli_allowed,
            Page::Tags => {
                self.favorite
                    || !self.tags.read(cx).value().trim().is_empty()
                    || !self.note.read(cx).value().trim().is_empty()
            }
        }
    }

    fn render_nav(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let items = Page::for_protocol(self.protocol).iter().map(|&page| {
            let dot = self.page_has_settings(page, cx);
            let this = cx.entity().downgrade();
            let item = SidebarMenuItem::new(page.title())
                .icon(Icon::new(page.icon()))
                .active(self.page == page);
            let item = if dot {
                item.suffix(|_, cx| div().size_1p5().rounded_full().bg(cx.theme().primary))
            } else {
                item
            };
            item.on_click(move |_, _, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.page = page;
                    cx.notify();
                });
            })
        });
        let pages = Page::for_protocol(self.protocol);
        let current = pages.iter().position(|p| *p == self.page).unwrap_or(0);
        let frame = crate::page_nav::frame("host-nav", &self.nav_focus, window, cx).on_key_down(
            cx.listener(move |this, event: &gpui_kit::KeyDownEvent, _, cx| {
                if let Some(ix) = crate::page_nav::step(event, current, pages.len()) {
                    this.page = pages[ix];
                    cx.notify();
                    cx.stop_propagation();
                }
            }),
        );
        let column = crate::page_nav::column(cx);
        // 菜单的元素借用着 window 与 cx，最后再渲染
        let menu = SidebarMenu::new()
            .gap_1()
            .children(items)
            .render("host-pages", window, cx);
        column.child(frame.child(menu))
    }

    fn render_general(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let protocols = [Protocol::Ssh, Protocol::Telnet, Protocol::Serial];
        let protocol_radio = RadioGroup::horizontal("host-protocol")
            .children([
                SharedString::from("SSH"),
                SharedString::from("Telnet"),
                t!("host.protocol.serial"),
            ])
            .selected_index(protocols.iter().position(|p| *p == self.protocol))
            .on_change(cx.listener(move |this, ix: &usize, window, cx| {
                if let Some(protocol) = protocols.get(*ix) {
                    this.set_protocol(*protocol, window, cx);
                }
            }));
        let form = v_form()
            .child(
                field()
                    .label(t!("host.field.protocol"))
                    .child(protocol_radio),
            )
            .child(
                field()
                    .label(t!("host.field.name"))
                    .child(Input::new(&self.name).id("host-name")),
            );
        let form = match self.protocol {
            Protocol::Ssh => self.render_ssh_general(form, cx),
            Protocol::Telnet => self.render_telnet_general(form),
            Protocol::Serial => self.render_serial_general(form, cx),
        };
        form.child(
            field()
                .label(t!("host.field.group"))
                .child(Select::new(&self.group)),
        )
        .into_any_element()
    }

    /// 地址与端口排在一行。
    fn address_field(&self) -> Field {
        field().child(
            h_flex()
                .gap_3()
                .items_start()
                .child(
                    div().flex_1().child(
                        field()
                            .label(t!("host.field.address"))
                            .required(true)
                            .child(Input::new(&self.address).id("host-address")),
                    ),
                )
                .child(
                    div().w(rems(6.)).child(
                        field()
                            .label(t!("host.field.port"))
                            .child(Input::new(&self.port).id("host-port")),
                    ),
                ),
        )
    }

    fn render_telnet_general(&mut self, form: Form) -> Form {
        form.child(self.address_field())
            .child(
                field()
                    .label(t!("host.field.username"))
                    .description(prose(t!("host.telnet.login_hint")))
                    .child(Input::new(&self.username).id("host-username")),
            )
            .child(
                field()
                    .label(t!("host.field.password"))
                    .child(Input::new(&self.password).id("host-password").mask_toggle()),
            )
    }

    fn render_serial_general(&mut self, form: Form, cx: &mut Context<Self>) -> Form {
        let this = cx.entity().downgrade();
        let muted = cx.theme().muted_foreground;
        let current = self.serial_port.read(cx).value().trim().to_string();
        let ports = self.ports.as_ref().map(|ports| {
            if ports.is_empty() {
                return div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("host.serial.none")))
                    .into_any_element();
            }
            h_flex()
                .flex_wrap()
                .gap_1()
                .children(ports.iter().map(|port| {
                    let description = port_description(port);
                    let tooltip = port_tooltip(port);
                    let selected = port.name.eq_ignore_ascii_case(&current);
                    let (this, name) = (this.clone(), port.name.clone());
                    let button =
                        Button::new(SharedString::from(format!("serial-port-{}", port.name)))
                            .xsmall()
                            .child(h_flex().gap_1().child(port.name.clone()).when_some(
                                description,
                                |el, description| {
                                    el.child(
                                        div()
                                            .max_w(rems(12.))
                                            .truncate()
                                            .text_color(muted)
                                            .child(prose(description)),
                                    )
                                },
                            ))
                            .when_some(tooltip, |b, tooltip| b.tooltip(tooltip))
                            .on_click(move |_, window, cx| {
                                let _ =
                                    this.update(cx, |this, cx| this.pick_port(&name, window, cx));
                            });
                    if selected {
                        button.primary()
                    } else {
                        button.outline()
                    }
                }))
                .into_any_element()
        });
        let presets = {
            let this = this.clone();
            Button::new("host-baud-presets")
                .ghost()
                .small()
                .icon(IconName::ChevronDown)
                .tooltip(t!("host.serial.baud_presets"))
                .dropdown_menu(move |menu, _, _| {
                    BAUD_PRESETS.iter().fold(menu, |menu, baud| {
                        let this = this.clone();
                        let baud = *baud;
                        menu.item(PopupMenuItem::new(baud.to_string()).on_click(
                            move |_, window, cx| {
                                let _ = this.update(cx, |this, cx| {
                                    this.baud.update(cx, |s, cx| {
                                        s.set_value(baud.to_string(), window, cx)
                                    });
                                    this.clear_test(cx);
                                    cx.notify();
                                });
                            },
                        ))
                    })
                })
        };
        let column = |label: SharedString, child: AnyElement| {
            div()
                .flex_1()
                .min_w_0()
                .child(field().label(label).child(child))
        };
        form.child(
            field().label(t!("host.serial.port")).required(true).child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.serial_port).id("host-serial-port")),
                    )
                    .child(
                        Button::new("host-serial-refresh")
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .tooltip(t!("host.serial.refresh"))
                            .on_click(cx.listener(|this, _, _, cx| this.detect_ports(cx))),
                    ),
            ),
        )
        .when_some(ports, |form, ports| form.child(field().child(ports)))
        .child(
            field().child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .child(column(
                        t!("host.serial.baud"),
                        Input::new(&self.baud)
                            .id("host-serial-baud")
                            .suffix(presets)
                            .into_any_element(),
                    ))
                    .child(column(
                        t!("host.serial.data_bits"),
                        Select::new(&self.data_bits).into_any_element(),
                    ))
                    .child(column(
                        t!("host.serial.parity"),
                        Select::new(&self.parity).into_any_element(),
                    ))
                    .child(column(
                        t!("host.serial.stop_bits"),
                        Select::new(&self.stop_bits).into_any_element(),
                    )),
            ),
        )
        .child(
            field()
                .label(t!("host.serial.flow_control"))
                .description(t!("host.serial.flow_control_hint"))
                .child(Select::new(&self.flow_control)),
        )
    }

    fn render_ssh_general(&mut self, form: Form, cx: &mut Context<Self>) -> Form {
        let mut auth_options = vec![
            (AuthKind::Password, t!("host.auth.password")),
            (AuthKind::Key, t!("host.auth.key")),
            (AuthKind::Agent, t!("host.auth.agent")),
            (AuthKind::Profile, t!("host.auth.profile")),
        ];
        if self.inherited.profile.is_some() {
            auth_options.push((AuthKind::Inherit, t!("host.auth.inherit")));
        }
        let selected = auth_options.iter().position(|(k, _)| *k == self.auth);
        let kinds: Vec<AuthKind> = auth_options.iter().map(|(k, _)| *k).collect();
        let auth_radio = RadioGroup::horizontal("host-auth")
            .children(auth_options.into_iter().map(|(_, label)| label))
            .selected_index(selected)
            .on_change(cx.listener(move |this, ix: &usize, window, cx| {
                if let Some(kind) = kinds.get(*ix) {
                    this.auth = *kind;
                    this.auth_touched = true;
                    this.refresh_username_placeholder(window, cx);
                    this.clear_test(cx);
                    cx.notify();
                }
            }));
        form.child(self.address_field())
            .child(
                field()
                    .label(t!("host.field.username"))
                    .child(Input::new(&self.username).id("host-username")),
            )
            .child(field().label(t!("host.field.auth")).child(auth_radio))
            .child(field().child(self.render_auth_fields(cx)))
    }

    fn render_auth_fields(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        match self.auth {
            AuthKind::Password => field()
                .label(t!("host.field.password"))
                .child(Input::new(&self.password).id("host-password").mask_toggle())
                .into_any_element(),
            AuthKind::Key => self.render_key_fields(false, cx),
            AuthKind::Agent => div()
                .text_sm()
                .text_color(muted)
                .child(prose(t!("host.auth.agent_hint")))
                .into_any_element(),
            AuthKind::Profile => {
                if self.store.list::<Profile>().is_empty() {
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(prose(t!("host.auth.no_profiles")))
                        .into_any_element()
                } else {
                    field()
                        .label(t!("host.field.profile"))
                        .child(Select::new(&self.profile))
                        .into_any_element()
                }
            }
            AuthKind::Inherit => {
                let text = self
                    .inherited
                    .profile
                    .as_ref()
                    .map(|p| {
                        let name = self
                            .store
                            .get::<Profile>(&p.value)
                            .map(|r| r.doc.name)
                            .unwrap_or_default();
                        t!(
                            "host.auth.inherit_hint",
                            group = p.group_name.clone(),
                            profile = name
                        )
                    })
                    .unwrap_or_default();
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(prose(text))
                    .into_any_element()
            }
        }
    }

    /// 私钥的来源与字段；`cli` 为真时是命令行工具专用凭据的那一组。
    fn render_key_fields(&mut self, cli: bool, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.clone();
        let changed: fn(&mut Self, &mut Context<Self>) = |this, cx| this.clear_test(cx);
        if cli {
            self.cli_key.render(
                "cli-key-from",
                &store,
                |this: &mut Self| &mut this.cli_key,
                changed,
                cx,
            )
        } else {
            self.key.render(
                "host-key-from",
                &store,
                |this: &mut Self| &mut this.key,
                changed,
                cx,
            )
        }
    }

    fn render_connection(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let choice = chosen(&self.proxy_kind, cx).unwrap_or(ProxyChoice::Inherit);
        let jump_set = !matches!(
            chosen(&self.jump, cx),
            Some(JumpChoice::Inherit | JumpChoice::Direct) | None
        ) || (matches!(chosen(&self.jump, cx), Some(JumpChoice::Inherit))
            && self.inherited.jump.is_some());
        // 沿用的分组代理命令：只读地写出来，看得到连接时会在本机执行什么
        let inherited_command = match choice {
            ProxyChoice::Inherit => self
                .inherited
                .proxy_command
                .as_ref()
                .map(|c| c.value.clone()),
            _ => None,
        };
        let proxy_fields = match choice {
            ProxyChoice::Inherit => inherited_command.clone().map(|command| {
                let theme = cx.theme();
                field()
                    .description(prose(proxy_command_hint()))
                    .child(
                        div()
                            .id("host-proxy-inherited")
                            .px_3()
                            .py_2()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .text_sm()
                            .font_family(theme.mono_font_family.clone())
                            .child(command),
                    )
                    .into_any_element()
            }),
            ProxyChoice::Off => None,
            ProxyChoice::Kind(ProxyKind::Command) => Some(
                field()
                    .label(t!("host.proxy.command"))
                    .description(prose(proxy_command_hint()))
                    .child(Input::new(&self.proxy_command).id("host-proxy-command"))
                    .into_any_element(),
            ),
            ProxyChoice::Kind(kind) => Some(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_3()
                            .child(div().flex_1().child(Input::new(&self.proxy_address)))
                            .child(div().w(rems(6.)).child(Input::new(&self.proxy_port))),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .child(div().flex_1().child(Input::new(&self.proxy_username)))
                            .child(
                                div().flex_1().child(
                                    Input::new(&self.proxy_password)
                                        .mask_toggle()
                                        .disabled(kind == ProxyKind::Socks4),
                                ),
                            ),
                    )
                    .into_any_element(),
            ),
        };
        let uses_proxy = matches!(choice, ProxyChoice::Kind(_)) || inherited_command.is_some();
        v_form()
            .child(
                field()
                    .label(t!("host.field.jump"))
                    .description(prose(t!("host.jump.hint")))
                    .child(Select::new(&self.jump)),
            )
            .child(
                field()
                    .label(t!("host.field.proxy"))
                    .child(Select::new(&self.proxy_kind).id("host-proxy-kind")),
            )
            .when_some(proxy_fields, |this, fields| {
                this.child(field().child(fields))
            })
            .when(uses_proxy && jump_set, |this| {
                this.child(
                    field().child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().warning)
                            .child(prose(t!("host.proxy.with_jump"))),
                    ),
                )
            })
            .when(self.protocol == Protocol::Ssh, |form| {
                form.child(self.checkbox(
                    "host-agent-forward",
                    t!("host.ssh.agent_forward"),
                    t!("host.ssh.agent_forward_hint"),
                    self.agent_forward,
                    |this| &mut this.agent_forward,
                    cx,
                ))
                .child(self.checkbox(
                    "host-x11",
                    t!("host.ssh.x11"),
                    t!("host.ssh.x11_hint"),
                    self.x11_forward,
                    |this| &mut this.x11_forward,
                    cx,
                ))
                .when(self.x11_forward, |form| {
                    form.child(
                        self.checkbox(
                            "host-x11-trusted",
                            t!("host.ssh.x11_trusted"),
                            t!("host.ssh.x11_trusted_hint"),
                            self.x11_trusted,
                            |this| &mut this.x11_trusted,
                            cx,
                        )
                        .pl_6(),
                    )
                })
                .child(self.checkbox(
                    "host-legacy",
                    t!("host.ssh.legacy"),
                    t!("host.ssh.legacy_hint"),
                    self.legacy,
                    |this| &mut this.legacy,
                    cx,
                ))
                .child(self.checkbox(
                    "host-single-channel",
                    t!("host.ssh.single_channel"),
                    t!("host.ssh.single_channel_hint"),
                    self.single_channel,
                    |this| &mut this.single_channel,
                    cx,
                ))
            })
            .into_any_element()
    }

    /// 带说明的复选框，勾选状态存在 `slot` 指向的字段中。
    fn checkbox(
        &self,
        id: &'static str,
        label: SharedString,
        hint: SharedString,
        checked: bool,
        slot: fn(&mut Self) -> &mut bool,
        cx: &mut Context<Self>,
    ) -> Field {
        field().child(
            v_flex()
                .gap_1()
                .child(
                    Checkbox::new(id)
                        .label(label)
                        .checked(checked)
                        .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                            *slot(this) = *checked;
                            this.clear_test(cx);
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .pl_6()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(prose(hint)),
                ),
        )
    }

    fn render_totp(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let preview = match &self.totp_preview {
            Some(Ok((code, seconds))) => Some(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(t!("host.totp.current"))
                    .child(
                        div()
                            .font_family("JetBrains Mono")
                            .font_medium()
                            .child(code.clone()),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(prose(t!("host.totp.remaining", seconds = seconds))),
                    )
                    .into_any_element(),
            ),
            Some(Err(error)) => Some(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(prose(error.clone()))
                    .into_any_element(),
            ),
            None => None,
        };
        v_form()
            .child(self.checkbox(
                "host-totp",
                t!("host.totp.enable"),
                t!("host.totp.hint"),
                self.totp_enabled,
                |this| &mut this.totp_enabled,
                cx,
            ))
            .when(self.totp_enabled, |this| {
                this.child(
                    field().child(
                        Input::new(&self.totp_secret)
                            .id("host-totp-secret")
                            .mask_toggle(),
                    ),
                )
                .when_some(preview, |this, preview| this.child(field().child(preview)))
                .child(
                    field().child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().warning)
                            .child(prose(t!("host.totp.risk"))),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_terminal(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let form = v_form()
            .child(
                field()
                    .label(t!("host.terminal.scheme"))
                    .description(prose(t!("host.terminal.scheme_hint")))
                    .child(Select::new(&self.scheme)),
            )
            .child(
                field()
                    .label(t!("host.terminal.encoding"))
                    .description(prose(t!("host.terminal.encoding_hint")))
                    .child(Select::new(&self.encoding)),
            )
            .child(self.checkbox(
                "host-record",
                t!("host.terminal.record"),
                t!("host.terminal.record_hint"),
                self.record,
                |this| &mut this.record,
                cx,
            ));
        if self.protocol != Protocol::Ssh {
            return self.render_line(form, cx).into_any_element();
        }
        form.child(
            field()
                .label(t!("host.sftp.encoding"))
                .description(prose(t!("host.sftp.encoding_hint")))
                .child(Select::new(&self.sftp_encoding).id("host-sftp-encoding")),
        )
        .child(
            field()
                .label(t!("host.terminal.startup"))
                .description(prose(t!("host.terminal.startup_hint")))
                .child(Textarea::new(&self.startup)),
        )
        .child(self.checkbox(
            "host-tmux",
            t!("host.terminal.tmux"),
            t!("host.terminal.tmux_hint"),
            self.tmux,
            |this| &mut this.tmux,
            cx,
        ))
        .into_any_element()
    }

    /// Telnet、串口的行规程：回车键、本地回显、退格键，Telnet 还有终端类型。
    fn render_line(&mut self, form: Form, cx: &mut Context<Self>) -> Form {
        let enters = [Enter::Crlf, Enter::Cr, Enter::Lf];
        let echoes: &'static [LocalEcho] = if self.protocol == Protocol::Telnet {
            &[LocalEcho::Auto, LocalEcho::On, LocalEcho::Off]
        } else {
            &[LocalEcho::Off, LocalEcho::On]
        };
        let backspaces = [Backspace::Del, Backspace::Bs];
        let enter = RadioGroup::horizontal("host-enter")
            .children(["CR LF", "CR", "LF"])
            .selected_index(enters.iter().position(|e| *e == self.enter))
            .on_change(cx.listener(move |this, ix: &usize, _, cx| {
                if let Some(enter) = enters.get(*ix) {
                    this.enter = *enter;
                    cx.notify();
                }
            }));
        let echo = RadioGroup::horizontal("host-local-echo")
            .children(echoes.iter().map(|e| match e {
                LocalEcho::Auto => t!("host.line.echo_auto"),
                LocalEcho::On => t!("host.line.echo_on"),
                LocalEcho::Off => t!("host.line.echo_off"),
            }))
            .selected_index(echoes.iter().position(|e| *e == self.local_echo))
            .on_change(cx.listener(move |this, ix: &usize, _, cx| {
                if let Some(echo) = echoes.get(*ix) {
                    this.local_echo = *echo;
                    cx.notify();
                }
            }));
        let backspace = RadioGroup::horizontal("host-backspace")
            .children(["DEL (0x7F)", "BS (0x08)"])
            .selected_index(backspaces.iter().position(|b| *b == self.backspace))
            .on_change(cx.listener(move |this, ix: &usize, _, cx| {
                if let Some(backspace) = backspaces.get(*ix) {
                    this.backspace = *backspace;
                    cx.notify();
                }
            }));
        form.child(
            field()
                .label(t!("host.line.enter"))
                .description(prose(t!("host.line.enter_hint")))
                .child(enter),
        )
        .child(
            field()
                .label(t!("host.line.local_echo"))
                .description(if self.protocol == Protocol::Telnet {
                    t!("host.line.local_echo_hint_telnet")
                } else {
                    t!("host.line.local_echo_hint_serial")
                })
                .child(echo),
        )
        .child(
            field()
                .label(t!("host.line.backspace"))
                .description(prose(t!("host.line.backspace_hint")))
                .child(backspace),
        )
        .when(self.protocol == Protocol::Telnet, |form| {
            form.child(
                field()
                    .label(t!("host.telnet.term_type"))
                    .description(prose(t!("host.telnet.term_type_hint")))
                    .child(Input::new(&self.term_type).id("host-term-type")),
            )
        })
    }

    fn render_cli(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut form = v_form().child(self.checkbox(
            "host-cli",
            t!("host.cli.allow"),
            t!("host.cli.allow_hint"),
            self.cli_allowed,
            |this| &mut this.cli_allowed,
            cx,
        ));
        if self.cli_allowed {
            form = form.child(self.checkbox(
                "host-cli-separate",
                t!("host.cli.separate"),
                t!("host.cli.separate_hint"),
                self.cli_separate,
                |this| &mut this.cli_separate,
                cx,
            ));
        }
        if self.cli_allowed && self.cli_separate {
            let radio = RadioGroup::horizontal("host-cli-auth")
                .children([t!("host.auth.password"), t!("host.auth.key")])
                .selected_index(Some(if self.cli_password_auth { 0 } else { 1 }))
                .on_change(cx.listener(|this, ix: &usize, _, cx| {
                    this.cli_password_auth = *ix == 0;
                    cx.notify();
                }));
            let auth_fields = if self.cli_password_auth {
                Input::new(&self.cli_password)
                    .mask_toggle()
                    .into_any_element()
            } else {
                self.render_key_fields(true, cx)
            };
            form = form
                .child(
                    field()
                        .label(t!("host.field.username"))
                        .child(Input::new(&self.cli_username)),
                )
                .child(field().label(t!("host.field.auth")).child(radio))
                .child(field().child(auth_fields));
        }
        form.into_any_element()
    }

    fn render_tags(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // 已有的标签中还没填的，点一下追加
        let current: Vec<String> = self
            .tags
            .read(cx)
            .value()
            .split([',', '，'])
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        let mut known: Vec<String> = self
            .store
            .list::<Host>()
            .into_iter()
            .flat_map(|h| h.doc.tags)
            .collect();
        known.sort_by_key(|t| t.to_lowercase());
        known.dedup_by_key(|t| t.to_lowercase());
        let suggestions: Vec<String> = known
            .into_iter()
            .filter(|t| !current.contains(&t.to_lowercase()))
            .take(12)
            .collect();
        v_form()
            .child(
                field()
                    .label(t!("host.tags.label"))
                    .child(Input::new(&self.tags).id("host-tags"))
                    .when(!suggestions.is_empty(), |field| {
                        field.description_fn({
                            let this = cx.entity().downgrade();
                            move |_, _| {
                                h_flex()
                                    .flex_wrap()
                                    .gap_1()
                                    .children(suggestions.iter().map(|tag| {
                                        let (this, tag) = (this.clone(), tag.clone());
                                        Button::new(SharedString::from(format!(
                                            "tag-suggest-{tag}"
                                        )))
                                        .ghost()
                                        .xsmall()
                                        .label(format!("+ {tag}"))
                                        .on_click(
                                            move |_, window, cx| {
                                                let _ = this.update(cx, |this, cx| {
                                                    this.add_tag(&tag, window, cx)
                                                });
                                            },
                                        )
                                    }))
                            }
                        })
                    }),
            )
            .child(
                field()
                    .label(t!("host.tags.note"))
                    .child(Textarea::new(&self.note)),
            )
            .child(
                field().child(
                    Checkbox::new("host-favorite")
                        .label(t!("host.tags.favorite"))
                        .checked(self.favorite)
                        .on_change(cx.listener(|this, checked: &bool, _, cx| {
                            this.favorite = *checked;
                            cx.notify();
                        })),
                ),
            )
            .into_any_element()
    }

    fn add_tag(&mut self, tag: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .tags
            .read(cx)
            .value()
            .trim()
            .trim_end_matches([',', '，'])
            .to_string();
        let next = if current.is_empty() {
            tag.to_string()
        } else {
            format!("{current}, {tag}")
        };
        self.tags.update(cx, |s, cx| s.set_value(next, window, cx));
        cx.notify();
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let test = match &self.test {
            TestState::Idle => None,
            TestState::Running => Some(
                div()
                    .text_color(theme.muted_foreground)
                    .child(prose(t!("host.test.running"))),
            ),
            TestState::Passed(message) => Some(
                div()
                    .text_color(theme.success)
                    .child(prose(message.clone())),
            ),
            TestState::Failed(message) => {
                Some(div().text_color(theme.danger).child(prose(message.clone())))
            }
        };
        let running = matches!(self.test, TestState::Running);
        v_flex()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.danger).child(prose(error)))
            })
            .when_some(test, |this, test| {
                this.child(test.text_sm().max_h(rems(4.)).overflow_hidden())
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("host-test")
                            .outline()
                            .label(t!("host.test.button"))
                            .loading(running)
                            .disabled(running)
                            .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .child(self.notice.clone()),
                    )
                    .child(
                        Button::new("host-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("host-save")
                            .primary()
                            .label(t!("common.save"))
                            .loading(self.saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.save(window, cx) {
                                    window.close_dialog(cx);
                                }
                            })),
                    ),
            )
    }
}

impl Render for HostForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.page {
            Page::General => self.render_general(cx),
            Page::Connection => self.render_connection(cx),
            Page::Totp => self.render_totp(cx),
            Page::Terminal => self.render_terminal(cx),
            Page::Cli => self.render_cli(cx),
            Page::Tags => self.render_tags(cx),
        };
        // 高 32rem，窗口矮时缩到窗口内（与设置对话框相同）：对话框放不下时 gpui-kit 0.8 起滚轮只滚对话框本身，
        // 右侧分页里的内容就滚不到了
        let height = rems(32.)
            .to_pixels(window.rem_size())
            .min(window.viewport_size().height - rems(8.).to_pixels(window.rem_size()));
        v_flex()
            .h(height)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .gap_4()
                    .child(self.render_nav(window, cx))
                    .child(
                        div()
                            .id("host-page")
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .pr_1()
                            .child(
                                v_flex()
                                    .gap_3()
                                    .child(div().text_base().font_medium().child(self.page.title()))
                                    .child(body),
                            ),
                    ),
            )
            .child(self.render_footer(cx))
    }
}

// ------------------------------------------------------------ 小工具

/// 粘贴的私钥（主机认证、命令行工具专用凭据）中这次新填的，保存前要检查。
fn key_checks(host: &Host, secrets: &SecretEdits) -> Vec<KeyCheck> {
    let mut checks = Vec::new();
    if let Auth::Key {
        source: KeySource::Pasted { .. },
    } = &host.auth
        && let SecretEdit::Set(text) = &secrets.private_key
    {
        checks.push((Page::General, text.clone(), secrets.passphrase.clone()));
    }
    if host.cli.allowed
        && let Some(CliCredential {
            auth: CliAuth::Key {
                source: KeySource::Pasted { .. },
            },
            ..
        }) = &host.cli.credential
        && let SecretEdit::Set(text) = &secrets.cli_private_key
    {
        checks.push((Page::Cli, text.clone(), secrets.cli_passphrase.clone()));
    }
    checks
}

fn secret_edit(value: &str) -> SecretEdit {
    if value.is_empty() {
        SecretEdit::Keep
    } else {
        SecretEdit::Set(SecretString::new(value))
    }
}

fn clear_if(saved: bool) -> SecretEdit {
    if saved {
        SecretEdit::Clear
    } else {
        SecretEdit::Keep
    }
}

/// 常用的波特率（输入框旁的下拉）。
const BAUD_PRESETS: [u32; 11] = [
    1200, 2400, 4800, 9600, 19200, 38400, 57600, 115_200, 230_400, 460_800, 921_600,
];

/// 端口输入框的占位：协议的常用端口。
fn default_port_text(protocol: Protocol) -> SharedString {
    match protocol {
        Protocol::Telnet => "23".into(),
        _ => "22".into(),
    }
}

/// 密码输入框的占位：保存过时说留空不改；Telnet 说明密码只用于自动登录。
fn password_placeholder(protocol: Protocol, saved: bool) -> SharedString {
    if saved {
        t!("host.secret.keep")
    } else if protocol == Protocol::Telnet {
        t!("host.telnet.password_placeholder")
    } else {
        SharedString::default()
    }
}

/// 检测到的串口按钮上的设备名称：去掉末尾重复的"（COMx）"；与端口名相同时不显示。
fn port_description(port: &PortInfo) -> Option<String> {
    let suffix = format!("({})", port.name);
    let description = port.description.trim();
    let description = description
        .strip_suffix(&suffix)
        .unwrap_or(description)
        .trim();
    (!description.is_empty() && !description.eq_ignore_ascii_case(&port.name))
        .then(|| description.to_string())
}

/// 检测到的串口的悬停提示：设备名称、VID / PID、序列号。
fn port_tooltip(port: &PortInfo) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !port.description.trim().is_empty() {
        parts.push(port.description.trim().to_string());
    }
    if let Some(usb) = &port.usb {
        if !usb.vid.is_empty() || !usb.pid.is_empty() {
            parts.push(format!("VID {} PID {}", usb.vid, usb.pid));
        }
        if !usb.serial_number.is_empty() {
            parts.push(usb.serial_number.clone());
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn parity_choices() -> Vec<Choice<Parity>> {
    vec![
        Choice::new(t!("host.serial.parity_none"), Parity::None),
        Choice::new(t!("host.serial.parity_odd"), Parity::Odd),
        Choice::new(t!("host.serial.parity_even"), Parity::Even),
        Choice::new(t!("host.serial.parity_mark"), Parity::Mark),
        Choice::new(t!("host.serial.parity_space"), Parity::Space),
    ]
}

fn stop_bits_choices() -> Vec<Choice<StopBits>> {
    vec![
        Choice::new("1", StopBits::One),
        Choice::new("1.5", StopBits::OnePointFive),
        Choice::new("2", StopBits::Two),
    ]
}

fn flow_control_choices() -> Vec<Choice<FlowControl>> {
    vec![
        Choice::new(t!("host.serial.flow_none"), FlowControl::None),
        Choice::new(t!("host.serial.flow_rts_cts"), FlowControl::RtsCts),
        Choice::new(t!("host.serial.flow_xon_xoff"), FlowControl::XonXoff),
    ]
}

/// 端口：留空为默认端口（0），否则需在 1 到 65535 之间。
fn parse_port(value: &str) -> Option<u16> {
    if value.is_empty() {
        return Some(0);
    }
    value.parse::<u16>().ok().filter(|p| *p > 0)
}

fn default_proxy_port(kind: ProxyKind) -> Option<u16> {
    match kind {
        ProxyKind::Socks5 | ProxyKind::Socks4 => Some(1080),
        ProxyKind::Http => Some(8080),
        ProxyKind::Https => Some(443),
        ProxyKind::Command => None,
    }
}

fn totp_error(reason: &str) -> SharedString {
    match reason {
        "bad_link" => t!("host.totp.error.bad_link"),
        "not_time_based" => t!("host.totp.error.not_time_based"),
        "bad_digits" => t!("host.totp.error.bad_digits"),
        "bad_period" => t!("host.totp.error.bad_period"),
        "bad_algorithm" => t!("host.totp.error.bad_algorithm"),
        "missing_secret" => t!("host.totp.error.missing_secret"),
        "bad_base32" => t!("host.totp.error.bad_base32"),
        "too_short" => t!("host.totp.error.too_short"),
        _ => t!("host.totp.error.invalid"),
    }
}

/// 分组的选项：第一项"不分组"，之后按树的先序，每深一层前面多一个全角空格。
fn group_choices(store: &Store) -> Vec<Choice<Option<GroupId>>> {
    let groups = store.list::<Group>();
    let ids: std::collections::HashSet<&str> = groups.iter().map(|g| g.id.as_str()).collect();
    let mut choices = vec![Choice::new(t!("host.field.no_group"), None)];
    fn walk(
        groups: &[termlet_core::store::Record<Group>],
        ids: &std::collections::HashSet<&str>,
        parent: Option<&str>,
        depth: usize,
        out: &mut Vec<Choice<Option<GroupId>>>,
    ) {
        if depth > 32 {
            return;
        }
        for g in groups
            .iter()
            .filter(|g| g.parent.as_deref().filter(|p| ids.contains(p)) == parent)
        {
            out.push(Choice::new(
                format!("{}{}", "　".repeat(depth), g.doc.name),
                Some(g.id.clone()),
            ));
            walk(groups, ids, Some(g.id.as_str()), depth + 1, out);
        }
    }
    walk(&groups, &ids, None, 0, &mut choices);
    choices
}

/// 代理的选项：第一项是"沿用分组"（分组有代理命令时写出分组名）或"不使用"，分组有代理命令时多一项"不使用代理"。
fn proxy_choices(inherited: &Inherited) -> Vec<Choice<ProxyChoice>> {
    let mut choices = match &inherited.proxy_command {
        Some(command) => vec![
            Choice::new(
                t!("host.proxy.inherit", group = command.group_name.clone()),
                ProxyChoice::Inherit,
            ),
            Choice::new(t!("host.proxy.off_override"), ProxyChoice::Off),
        ],
        None => vec![Choice::new(t!("host.proxy.none"), ProxyChoice::Inherit)],
    };
    choices.extend([
        Choice::new("SOCKS5", ProxyChoice::Kind(ProxyKind::Socks5)),
        Choice::new("SOCKS4 / 4a", ProxyChoice::Kind(ProxyKind::Socks4)),
        Choice::new(t!("host.proxy.http"), ProxyChoice::Kind(ProxyKind::Http)),
        Choice::new(t!("host.proxy.https"), ProxyChoice::Kind(ProxyKind::Https)),
        Choice::new(
            t!("host.proxy.command_kind"),
            ProxyChoice::Kind(ProxyKind::Command),
        ),
    ]);
    choices
}

/// 代理命令的说明：占位符、这台电脑上怎么启动（Windows 不经过 cmd，其他系统交给 shell）、第一次执行前的确认。
pub(crate) fn proxy_command_hint() -> SharedString {
    if cfg!(windows) {
        t!("host.proxy.command_hint_windows")
    } else {
        t!("host.proxy.command_hint_unix")
    }
}

fn encoding_choices() -> Vec<Choice<Encoding>> {
    vec![
        Choice::new("UTF-8", Encoding::Utf8),
        Choice::new("GBK / GB18030", Encoding::Gb18030),
        Choice::new("Big5", Encoding::Big5),
    ]
}

/// 分组链上设置的终端配色：（分组名称，配色名称）。
fn inherit_scheme(store: &Store, group: Option<&GroupId>) -> Option<(String, String)> {
    store.group_chain(group).into_iter().find_map(|g| {
        let id = g.doc.defaults.color_scheme.trim().to_string();
        if id.is_empty() {
            return None;
        }
        let name = termlet_core::schemes::builtin_scheme(&id)
            .map(|s| s.name().to_string())
            .or_else(|| {
                store
                    .list::<termlet_core::model::ColorScheme>()
                    .into_iter()
                    .find(|s| s.id.as_str() == id)
                    .map(|s| s.doc.name)
            })
            .unwrap_or(id);
        Some((g.doc.name, name))
    })
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, SharedString, TestAppContext};

    use super::Page;
    use crate::i18n::{Language, set_language};
    use crate::testkit::{assert_nav_labels_fit, open_workspace};

    // 左侧导航的每一项在每种语言下都放得下分页名（SSH 主机的分页最多）
    #[gpui_kit::test]
    fn nav_labels_fit_in_every_language(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        for language in Language::ALL {
            set_language(language);
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.click("new-host", cx);
                window.render_frame(cx);
                let items = Page::ALL.iter().enumerate().map(|(ix, page)| {
                    (SharedString::from(format!("host-pages-{ix}")), page.title())
                });
                assert_nav_labels_fit(window, items, &format!("主机对话框（{language:?}）"));
                window.click("host-cancel", cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        set_language(Language::ZhCn);
    }

    // 分组的代理命令：新主机默认沿用；X11 转发与受信任一并保存。
    // 之后在编辑时选"不使用代理"，保存为不沿用
    #[gpui_kit::test]
    fn group_proxy_command_and_x11_are_saved(cx: &mut TestAppContext) {
        use termlet_core::model::{Group, GroupDefaults, Host};
        use termlet_core::store::Position;

        use crate::app_state::AppState;

        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        let group = cx.update(|cx| {
            AppState::store(cx)
                .create(
                    Group {
                        name: "AWS".into(),
                        defaults: GroupDefaults {
                            proxy_command: "aws ssm start-session --target %n".into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            super::open_new(Some(group.clone()), window, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.input("i-0abc", cx);
            window.click("host-address", cx);
            window.input("i-0abc", cx);
            window.click("host-username", cx);
            window.input("ec2-user", cx);
            window.click("host-pages-1", cx);
            window.render_frame(cx);
            assert!(window.try_find("host-x11-trusted").is_none());
            window.click("host-x11", cx);
            window.render_frame(cx);
            window.click("host-x11-trusted", cx);
            window.click("host-save", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let host = cx.update(|cx| AppState::store(cx).list::<Host>().remove(0));
        assert!(host.doc.proxy.is_none() && !host.doc.no_proxy, "沿用分组的");
        assert!(host.doc.ssh.x11_forward && host.doc.ssh.x11_trusted);

        // 编辑：选"不使用代理"
        cx.update_window(handle, |_, window, cx| {
            super::open_edit(host.id.clone(), window, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("host-pages-1", cx);
            window.render_frame(cx);
            // 打开代理的下拉框，选第二项"不使用代理（不沿用分组的代理命令）"
            window.click("host-proxy-kind", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("host-save", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let saved = cx.update(|cx| AppState::store(cx).get::<Host>(&host.id).unwrap().doc);
        assert!(
            saved.proxy.is_none() && saved.no_proxy,
            "不沿用分组的代理命令"
        );
    }
}
