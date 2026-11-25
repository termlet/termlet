//! 设置中的“同步”页。
//!
//! 没有设置同步时是设置表单：同步位置（本机文件夹、WebDAV、S3 兼容存储或 GitHub Gist）、同步密码、本机名称。
//! WebDAV 与 S3 的地址是 `http://` 时显示“允许不安全的 HTTP”与它的风险，不勾选不能使用。“测试连接”检查位置与账号，同步位置中
//! 已有同步文件时用同步密码解开，显示其中的数据量与设备；还没有时这台电脑是第一台设备，同步密码要再输入一次。“开始同步”
//! 还没有测试过连接时先测试，再保存设置并立即同步一次。
//!
//! 设置之后显示状态（上次同步的时间与结果、正在同步、错误）、立即同步、更改设置、停止同步（确认；同步位置中的文件不动）、
//! 自动同步的开关与间隔、同步过的设备，以及上次同步中两边都修改了的数据（可以改用另一边，或全部忽略）。
//!
//! 这些操作都立即执行，不随设置的“保存”；同步遇到需要用户决定的事时打开 [`answer_pending`] 的对话框。

use std::path::Path;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::form::{Field, Form, field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _,
    Subscription, Task, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::cloudsync::{
    Conflict, DEFAULT_INTERVAL_MINUTES, Device, GIST_MAX_SIZE, MAX_DEVICE_NAME, MIN_PASSWORD_LEN,
    Pending, RemoteInfo, S3_DEFAULT_REGION, Service, SetupInput, SyncConfig, SyncError,
    SyncOutcome, SyncStatus, SyncTarget,
};
use termlet_core::secret::SecretString;
use termlet_core::time::format_local;

use super::labels::{Resolution, conflict_name, count_label, count_rows, kind_label};
use super::{SyncModel, answer_pending, cloud, describe, model, on_core};
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::prose::{ProseLabel as _, prose};
use crate::t;

/// 自动同步间隔的选项（分钟）；设置成别的值（命令行、旧的设置）时也列出来。
const INTERVALS: [u32; 4] = [5, 10, 30, 60];

/// 同步位置的种类（单选的顺序）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TargetKind {
    #[default]
    Folder,
    WebDav,
    S3,
    Gist,
}

const TARGET_KINDS: [TargetKind; 4] = [
    TargetKind::Folder,
    TargetKind::WebDav,
    TargetKind::S3,
    TargetKind::Gist,
];

impl TargetKind {
    fn of(target: &SyncTarget) -> Self {
        match target {
            SyncTarget::Folder { .. } => TargetKind::Folder,
            SyncTarget::WebDav { .. } => TargetKind::WebDav,
            SyncTarget::S3 { .. } => TargetKind::S3,
            SyncTarget::Gist { .. } => TargetKind::Gist,
        }
    }

    fn service(self) -> Service {
        match self {
            TargetKind::Folder => Service::Folder,
            TargetKind::WebDav => Service::WebDav,
            TargetKind::S3 => Service::S3,
            TargetKind::Gist => Service::Gist,
        }
    }

    fn label(self) -> SharedString {
        match self {
            TargetKind::Folder => t!("sync_ui.form.target_folder"),
            TargetKind::WebDav => t!("sync_ui.form.target_webdav"),
            TargetKind::S3 => t!("sync_ui.form.target_s3"),
            TargetKind::Gist => t!("sync_ui.form.target_gist"),
        }
    }
}

/// 地址是 `http://`（不加密）。
fn is_http(address: &str) -> bool {
    address
        .trim()
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"))
}

/// 表单中填写的内容（含密码、密钥与令牌，不实现 `Debug`）。
#[derive(Clone, Default)]
struct FormValues {
    kind: TargetKind,
    path: String,
    url: String,
    username: String,
    webdav_password: String,
    /// WebDAV、S3 的地址是 `http://` 时允许（局域网）
    allow_http: bool,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    access_key: String,
    secret_key: String,
    path_style: bool,
    token: String,
    gist_id: String,
    password: String,
    confirm: String,
    device_name: String,
}

/// 表单在提交前就能发现的问题（其余由核心检查）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FormError {
    PathRequired,
    PathNotAbsolute,
    UrlRequired,
    EndpointRequired,
    BucketRequired,
    AccessKeyRequired,
    SecretKeyRequired,
    TokenRequired,
    PasswordTooShort,
    /// 第一台设备：还没有再次输入同步密码
    ConfirmRequired,
    ConfirmMismatch,
    DeviceNameTooLong,
}

impl FormError {
    fn text(self) -> SharedString {
        match self {
            FormError::PathRequired => t!("sync_ui.form.path_required"),
            FormError::PathNotAbsolute => t!("sync_ui.error.path_not_absolute"),
            FormError::UrlRequired => t!("sync_ui.form.url_required"),
            FormError::EndpointRequired => t!("sync_ui.error.endpoint_required"),
            FormError::BucketRequired => t!("sync_ui.error.bucket_required"),
            FormError::AccessKeyRequired => t!("sync_ui.error.access_key_required"),
            FormError::SecretKeyRequired => t!("sync_ui.error.secret_key_required"),
            FormError::TokenRequired => t!("sync_ui.error.token_required"),
            FormError::PasswordTooShort => {
                t!("sync_ui.error.password_too_short", min = MIN_PASSWORD_LEN)
            }
            FormError::ConfirmRequired => t!("sync_ui.form.confirm_required"),
            FormError::ConfirmMismatch => t!("sync_ui.form.confirm_mismatch"),
            FormError::DeviceNameTooLong => {
                t!("sync_ui.error.device_name_too_long", max = MAX_DEVICE_NAME)
            }
        }
    }
}

/// 由表单得到设置同步的输入。`require_confirm`：第一台设备，同步密码要两次输入一致。
/// 密码原样使用（可以有空格），地址、用户名、本机名称、访问密钥 ID、令牌去掉首尾空白。
fn build_input(values: &FormValues, require_confirm: bool) -> Result<SetupInput, FormError> {
    let target = match values.kind {
        TargetKind::Folder => {
            let path = values.path.trim();
            if path.is_empty() {
                return Err(FormError::PathRequired);
            }
            if !Path::new(path).is_absolute() {
                return Err(FormError::PathNotAbsolute);
            }
            SyncTarget::Folder { path: path.into() }
        }
        TargetKind::WebDav => {
            let url = values.url.trim();
            if url.is_empty() {
                return Err(FormError::UrlRequired);
            }
            SyncTarget::WebDav {
                url: url.into(),
                username: values.username.trim().into(),
                allow_http: values.allow_http && is_http(url),
            }
        }
        TargetKind::S3 => {
            let endpoint = values.endpoint.trim();
            if endpoint.is_empty() {
                return Err(FormError::EndpointRequired);
            }
            if values.bucket.trim().is_empty() {
                return Err(FormError::BucketRequired);
            }
            if values.access_key.trim().is_empty() {
                return Err(FormError::AccessKeyRequired);
            }
            if values.secret_key.trim().is_empty() {
                return Err(FormError::SecretKeyRequired);
            }
            SyncTarget::S3 {
                endpoint: endpoint.into(),
                region: values.region.trim().into(),
                bucket: values.bucket.trim().into(),
                prefix: values.prefix.trim().into(),
                access_key_id: values.access_key.trim().into(),
                path_style: values.path_style,
                allow_http: values.allow_http && is_http(endpoint),
            }
        }
        TargetKind::Gist => {
            if values.token.trim().is_empty() {
                return Err(FormError::TokenRequired);
            }
            SyncTarget::Gist {
                gist_id: values.gist_id.trim().into(),
            }
        }
    };
    if values.password.chars().count() < MIN_PASSWORD_LEN {
        return Err(FormError::PasswordTooShort);
    }
    if require_confirm {
        if values.confirm.is_empty() {
            return Err(FormError::ConfirmRequired);
        }
        if values.confirm != values.password {
            return Err(FormError::ConfirmMismatch);
        }
    }
    let device_name = values.device_name.trim();
    if device_name.chars().count() > MAX_DEVICE_NAME {
        return Err(FormError::DeviceNameTooLong);
    }
    // 只带当前种类的凭据（切换种类前填过的其他凭据不发出去）
    let credential = match values.kind {
        TargetKind::WebDav => values.webdav_password.clone(),
        TargetKind::S3 => values.secret_key.trim().to_string(),
        TargetKind::Gist => values.token.trim().to_string(),
        TargetKind::Folder => String::new(),
    };
    Ok(SetupInput {
        target,
        credential: SecretString::new(credential),
        password: SecretString::new(values.password.clone()),
        device_name: device_name.into(),
    })
}

/// 自动同步间隔的选项：固定的几项，加上当前的值。
fn interval_values(current: u32) -> Vec<u32> {
    let mut values = INTERVALS.to_vec();
    if !values.contains(&current) {
        values.push(current);
        values.sort_unstable();
    }
    values
}

fn interval_choices(current: u32) -> Vec<Choice<u32>> {
    interval_values(current)
        .into_iter()
        .map(|minutes| Choice::new(t!("sync_ui.status.every_minutes", count = minutes), minutes))
        .collect()
}

/// 本机名称的默认值（与核心相同，取电脑名），用作输入框的提示。
fn computer_name() -> Option<String> {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.trim().is_empty()))
}

/// 正在进行的操作（同一时间只进行一项；对应的按钮显示进度，其余按钮不可用）。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    Test,
    Start,
    SyncNow,
    Disable,
    /// 冲突改用另一边（记录键）
    UseOther(String),
}

/// “开始同步”的结果。
enum Started {
    /// 测试连接发现这是第一台设备，同步密码还要再输入一次（什么都没保存）
    NeedConfirm(RemoteInfo),
    /// 已保存设置并同步了一次（同步的结果；错误同时记在状态中）
    Synced(Result<SyncOutcome, SyncError>),
}

/// 设置中的“同步”页（页标题与导航由设置画）。
pub(crate) struct SyncPage {
    model: Entity<SyncModel>,
    /// 已设置同步时正在更改设置
    editing: bool,
    kind: TargetKind,
    path: Entity<InputState>,
    url: Entity<InputState>,
    username: Entity<InputState>,
    webdav_password: Entity<InputState>,
    /// WebDAV、S3 的地址是 `http://` 时允许
    allow_http: bool,
    endpoint: Entity<InputState>,
    region: Entity<InputState>,
    bucket: Entity<InputState>,
    prefix: Entity<InputState>,
    access_key: Entity<InputState>,
    secret_key: Entity<InputState>,
    path_style: bool,
    token: Entity<InputState>,
    gist_id: Entity<InputState>,
    password: Entity<InputState>,
    confirm: Entity<InputState>,
    device_name: Entity<InputState>,
    interval: Entity<ChoiceState<u32>>,
    /// 最近一次测试连接的结果；改了位置、账号或同步密码后作废
    probe: Option<RemoteInfo>,
    /// 位置、账号、同步密码每改一次加一：测试的结果回来时已经改过就不采用
    revision: u64,
    busy: Option<Op>,
    /// 正在修改的自动同步开关（修改完成之前先显示新的值）
    auto_override: Option<bool>,
    /// 正在修改间隔：这期间状态的变化不改回下拉框
    interval_saving: bool,
    /// 表单的错误（测试连接、设置失败）
    form_error: Option<SharedString>,
    /// 其他操作（停止同步、修改自动同步、改用另一边）的错误
    error: Option<SharedString>,
    _task: Option<Task<()>>,
    /// 修改自动同步开关、修改间隔各用一个任务（共用一个会互相取消，收尾就不执行了）
    _auto: Option<Task<()>>,
    _interval: Option<Task<()>>,
    _pick: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SyncPage {
    /// 页面自己的操作（开始同步、立即同步、停止……）还没有收尾（测试等它：这期间的点击会被忽略）。
    #[cfg(test)]
    pub(super) fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    /// 表单的错误（测试用）。
    #[cfg(test)]
    pub(super) fn form_error(&self) -> Option<SharedString> {
        self.form_error.clone()
    }

    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = model(cx);
        let config = model.read(cx).status().config.clone();
        let text = |value: String,
                    placeholder: SharedString,
                    masked: bool,
                    window: &mut Window,
                    cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(masked)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let kind = config
            .as_ref()
            .map_or(TargetKind::Folder, |c| TargetKind::of(&c.target));
        let prefill = Prefill::of(config.as_ref().map(|c| &c.target));
        let device = config
            .as_ref()
            .map(|c| c.device_name.clone())
            .unwrap_or_default();
        let device_placeholder = computer_name()
            .map(|name| t!("sync_ui.form.device_name_placeholder", name = name))
            .unwrap_or_default();
        let path = text(
            prefill.path,
            t!("sync_ui.form.folder_placeholder"),
            false,
            window,
            cx,
        );
        let url = text(
            prefill.url,
            t!("sync_ui.form.url_placeholder"),
            false,
            window,
            cx,
        );
        let username = text(prefill.username, SharedString::default(), false, window, cx);
        let webdav_password = text(String::new(), SharedString::default(), true, window, cx);
        let endpoint = text(
            prefill.endpoint,
            t!("sync_ui.form.endpoint_placeholder"),
            false,
            window,
            cx,
        );
        let region = text(
            prefill.region,
            SharedString::from(S3_DEFAULT_REGION),
            false,
            window,
            cx,
        );
        let bucket = text(prefill.bucket, SharedString::default(), false, window, cx);
        let prefix = text(
            prefill.prefix,
            t!("sync_ui.form.prefix_placeholder"),
            false,
            window,
            cx,
        );
        let access_key = text(
            prefill.access_key,
            SharedString::default(),
            false,
            window,
            cx,
        );
        let secret_key = text(String::new(), SharedString::default(), true, window, cx);
        let token = text(String::new(), SharedString::default(), true, window, cx);
        let gist_id = text(
            prefill.gist_id,
            t!("sync_ui.form.gist_id_placeholder"),
            false,
            window,
            cx,
        );
        let password = text(
            String::new(),
            t!("sync_ui.form.password_placeholder", min = MIN_PASSWORD_LEN),
            true,
            window,
            cx,
        );
        let confirm = text(String::new(), SharedString::default(), true, window, cx);
        let device_name = text(device, device_placeholder, false, window, cx);
        let minutes = config
            .as_ref()
            .map_or(DEFAULT_INTERVAL_MINUTES, |c| c.interval_minutes);
        let interval = choice_state(interval_choices(minutes), &minutes, false, window, cx);

        let mut subscriptions = vec![
            cx.observe_in(&model, window, |this, _, window, cx| {
                this.on_status_changed(window, cx)
            }),
            cx.subscribe_in(
                &interval,
                window,
                |this, _, event: &SelectEvent<_>, window, cx| {
                    let SelectEvent::Confirm(Some(minutes)) = event else {
                        return;
                    };
                    this.set_interval(*minutes, window, cx);
                },
            ),
        ];
        // 改了位置、账号或同步密码：测试连接的结果作废
        for state in [
            &path,
            &url,
            &username,
            &webdav_password,
            &endpoint,
            &region,
            &bucket,
            &prefix,
            &access_key,
            &secret_key,
            &token,
            &gist_id,
            &password,
        ] {
            subscriptions.push(cx.subscribe(state, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.invalidate_probe();
                    cx.notify();
                }
            }));
        }
        for state in [&confirm, &device_name] {
            subscriptions.push(cx.subscribe(state, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.form_error = None;
                    cx.notify();
                }
            }));
        }
        Self {
            model,
            editing: false,
            kind,
            path,
            url,
            username,
            webdav_password,
            allow_http: prefill.allow_http,
            endpoint,
            region,
            bucket,
            prefix,
            access_key,
            secret_key,
            path_style: prefill.path_style,
            token,
            gist_id,
            password,
            confirm,
            device_name,
            interval,
            probe: None,
            revision: 0,
            busy: None,
            auto_override: None,
            interval_saving: false,
            form_error: None,
            error: None,
            _task: None,
            _auto: None,
            _interval: None,
            _pick: None,
            _subscriptions: subscriptions,
        }
    }

    fn invalidate_probe(&mut self) {
        self.probe = None;
        self.revision += 1;
        self.form_error = None;
    }

    /// 测试连接的结果说明这是第一台设备。
    fn first_device(&self) -> bool {
        self.probe.as_ref().is_some_and(|p| !p.exists)
    }

    fn values(&self, cx: &App) -> FormValues {
        let value = |state: &Entity<InputState>| state.read(cx).value().to_string();
        FormValues {
            kind: self.kind,
            path: value(&self.path),
            url: value(&self.url),
            username: value(&self.username),
            webdav_password: value(&self.webdav_password),
            allow_http: self.allow_http,
            endpoint: value(&self.endpoint),
            region: value(&self.region),
            bucket: value(&self.bucket),
            prefix: value(&self.prefix),
            access_key: value(&self.access_key),
            secret_key: value(&self.secret_key),
            path_style: self.path_style,
            token: value(&self.token),
            gist_id: value(&self.gist_id),
            password: value(&self.password),
            confirm: value(&self.confirm),
            device_name: value(&self.device_name),
        }
    }

    /// 位置与同步密码都填了（按钮可用；其余在提交时检查）。
    fn is_filled(&self, cx: &App) -> bool {
        let filled = |state: &Entity<InputState>| !state.read(cx).value().trim().is_empty();
        let location = match self.kind {
            TargetKind::Folder => filled(&self.path),
            TargetKind::WebDav => filled(&self.url),
            TargetKind::S3 => filled(&self.endpoint) && filled(&self.bucket),
            TargetKind::Gist => filled(&self.token),
        };
        location && !self.password.read(cx).value().is_empty()
    }

    /// 锁定时清空输入到一半的秘密（密码、访问密钥、令牌）。
    pub(crate) fn clear_secrets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for state in self.secret_inputs() {
            state.update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    /// 输入了秘密的框（完成、取消设置时清空，不留在表单里）。
    fn secret_inputs(&self) -> [&Entity<InputState>; 5] {
        [
            &self.password,
            &self.confirm,
            &self.webdav_password,
            &self.secret_key,
            &self.token,
        ]
    }

    /// 当前种类的地址是 `http://`（显示“允许不安全的 HTTP”）。
    fn address_is_http(&self, cx: &App) -> bool {
        match self.kind {
            TargetKind::WebDav => is_http(&self.url.read(cx).value()),
            TargetKind::S3 => is_http(&self.endpoint.read(cx).value()),
            TargetKind::Folder | TargetKind::Gist => false,
        }
    }

    fn on_status_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.model.read(cx).status().config.clone() {
            Some(config) if !self.interval_saving => self.sync_interval(&config, window, cx),
            Some(_) => {}
            // 停止了同步（别处停止的也是）：显示设置表单
            None => self.editing = false,
        }
        cx.notify();
    }

    /// 下拉框显示设置中的间隔。
    fn sync_interval(&mut self, config: &SyncConfig, window: &mut Window, cx: &mut Context<Self>) {
        let minutes = config.interval_minutes;
        if chosen(&self.interval, cx) == Some(minutes) {
            return;
        }
        self.interval.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(interval_choices(minutes)), window, cx);
            state.set_selected_value(&minutes, window, cx);
        });
    }

    // ------------------------------------------------------------ 设置

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        self._pick = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.path.update(cx, |state, cx| {
                    state.set_value(dir.display().to_string(), window, cx)
                });
                // set_value 不发 Change：换了文件夹，上一次测试连接的结果作废
                this.invalidate_probe();
                cx.notify();
            });
        }));
    }

    fn focus_confirm(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm.focus_handle(cx).focus(window, cx);
    }

    fn test_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let input = match build_input(&self.values(cx), false) {
            Ok(input) => input,
            Err(error) => {
                self.form_error = Some(error.text());
                cx.notify();
                return;
            }
        };
        let cloud = cloud(cx);
        tracing::info!(target = input.target.kind_name(), "云同步：测试连接");
        let service = self.kind.service();
        let job = on_core(async move { cloud.test_connection(&input).await });
        let revision = self.revision;
        self.busy = Some(Op::Test);
        self.form_error = None;
        self.probe = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(info) if this.revision == revision => {
                        let first = !info.exists;
                        this.probe = Some(info);
                        if first {
                            this.focus_confirm(window, cx);
                        }
                    }
                    // 测试期间改了位置或密码：结果作废
                    Ok(_) => {}
                    Err(error) => this.form_error = Some(describe(&error, service)),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 开始同步：还没有测试过连接时先测试（第一台设备要再次输入同步密码），然后保存设置并同步一次。
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let values = self.values(cx);
        let input = match build_input(&values, self.first_device()) {
            Ok(input) => input,
            Err(error) => {
                self.form_error = Some(error.text());
                if error == FormError::ConfirmRequired {
                    self.focus_confirm(window, cx);
                }
                cx.notify();
                return;
            }
        };
        let need_probe = self.probe.is_none();
        let confirmed = !values.confirm.is_empty() && values.confirm == values.password;
        tracing::info!(
            target = input.target.kind_name(),
            need_probe,
            "云同步：开始同步（设置）"
        );
        let service = self.kind.service();
        let cloud = cloud(cx);
        let job = on_core(async move {
            if need_probe {
                let info = cloud.test_connection(&input).await?;
                if !info.exists && !confirmed {
                    return Ok(Started::NeedConfirm(info));
                }
            }
            cloud.setup(input).await?;
            Ok(Started::Synced(cloud.sync_now().await))
        });
        let revision = self.revision;
        self.busy = Some(Op::Start);
        self.form_error = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                this.model.update(cx, |model, cx| model.refresh(cx));
                match result {
                    Ok(Started::NeedConfirm(info)) => {
                        if this.revision == revision {
                            this.probe = Some(info);
                            this.focus_confirm(window, cx);
                        }
                    }
                    Ok(Started::Synced(outcome)) => {
                        this.finish_setup(window, cx);
                        // 同步的错误显示在状态中
                        if let Ok(SyncOutcome::Pending(_)) = outcome {
                            answer_pending(window, cx);
                        }
                    }
                    Err(error) => this.form_error = Some(describe(&error, service)),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 设置保存了：回到状态，清空表单中的密码。
    fn finish_setup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = false;
        self.error = None;
        for state in self.secret_inputs() {
            state.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.invalidate_probe();
    }

    /// 更改设置：表单预填当前的设置，密码要重新输入。
    fn edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.model.read(cx).status().config.clone() else {
            return;
        };
        let set = |state: &Entity<InputState>, value: String, window: &mut Window, cx: &mut App| {
            state.update(cx, |state, cx| state.set_value(value, window, cx));
        };
        self.kind = TargetKind::of(&config.target);
        let prefill = Prefill::of(Some(&config.target));
        match &config.target {
            SyncTarget::Folder { .. } => set(&self.path, prefill.path, window, cx),
            SyncTarget::WebDav { .. } => {
                set(&self.url, prefill.url, window, cx);
                set(&self.username, prefill.username, window, cx);
            }
            SyncTarget::S3 { .. } => {
                set(&self.endpoint, prefill.endpoint, window, cx);
                set(&self.region, prefill.region, window, cx);
                set(&self.bucket, prefill.bucket, window, cx);
                set(&self.prefix, prefill.prefix, window, cx);
                set(&self.access_key, prefill.access_key, window, cx);
            }
            SyncTarget::Gist { .. } => set(&self.gist_id, prefill.gist_id, window, cx),
        }
        self.allow_http = prefill.allow_http;
        self.path_style = prefill.path_style;
        set(&self.device_name, config.device_name.clone(), window, cx);
        for state in self.secret_inputs() {
            set(state, String::new(), window, cx);
        }
        self.editing = true;
        self.error = None;
        self.invalidate_probe();
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = false;
        // 输入过的密码、密钥、令牌不留在表单里
        for state in self.secret_inputs() {
            state.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.invalidate_probe();
        cx.notify();
    }

    /// 表单中的 Enter（设置把它转成 Confirm）：与点“开始同步”相同。
    fn on_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let configured = self.model.read(cx).status().config.is_some();
        if (!configured || self.editing) && self.busy.is_none() && self.is_filled(cx) {
            self.start(window, cx);
        }
    }

    // ------------------------------------------------------------ 同步与选项

    fn sync_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let cloud = cloud(cx);
        tracing::info!("云同步：立即同步");
        let job = on_core(async move { cloud.sync_now().await });
        self.busy = Some(Op::SyncNow);
        self.error = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                this.model.update(cx, |model, cx| model.refresh(cx));
                // 错误记在状态中（上次同步失败的原因）
                if let Ok(SyncOutcome::Pending(_)) = result {
                    answer_pending(window, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_auto(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let cloud = cloud(cx);
        tracing::info!(on, "云同步：自动同步");
        let job = on_core(async move { cloud.set_options(Some(on), None).await });
        self.auto_override = Some(on);
        self.error = None;
        self._auto = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.auto_override = None;
                this.model.update(cx, |model, cx| model.refresh(cx));
                if let Err(error) = result {
                    this.error = Some(describe(&error, this.model.read(cx).service()));
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_interval(&mut self, minutes: u32, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .model
            .read(cx)
            .status()
            .config
            .as_ref()
            .map(|c| c.interval_minutes);
        if current.is_none_or(|current| current == minutes) {
            return;
        }
        let cloud = cloud(cx);
        tracing::info!(minutes, "云同步：自动同步的间隔");
        let job = on_core(async move { cloud.set_options(None, Some(minutes)).await });
        self.interval_saving = true;
        self.error = None;
        self._interval = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.interval_saving = false;
                this.model.update(cx, |model, cx| model.refresh(cx));
                if let Err(error) = result {
                    this.error = Some(describe(&error, this.model.read(cx).service()));
                }
                if let Some(config) = this.model.read(cx).status().config.clone() {
                    this.sync_interval(&config, window, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn confirm_disable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity().downgrade();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let page = page.clone();
            alert
                .confirm()
                .title(t!("sync_ui.disable.title"))
                .description(prose(t!("sync_ui.disable.description")))
                .ok_text(t!("sync_ui.disable.ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = page.update(cx, |page, cx| page.disable(window, cx));
                    true
                })
        });
    }

    fn disable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let cloud = cloud(cx);
        tracing::info!("云同步：停止同步");
        let job = on_core(async move { cloud.disable().await });
        self.busy = Some(Op::Disable);
        self.error = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                this.model.update(cx, |model, cx| model.refresh(cx));
                match result {
                    Ok(()) => this.finish_setup(window, cx),
                    Err(error) => {
                        this.error = Some(describe(&error, this.model.read(cx).service()))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------ 冲突

    /// 改用落败的一边；落败的一边是删除时先确认。
    fn use_other(&mut self, conflict: Conflict, window: &mut Window, cx: &mut Context<Self>) {
        if !Resolution::of(&conflict).other_is_delete() {
            self.run_use_other(conflict.key, window, cx);
            return;
        }
        let page = cx.entity().downgrade();
        let name = conflict_name(&conflict);
        let key = conflict.key;
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (page, key) = (page.clone(), key.clone());
            alert
                .confirm()
                .title(t!("sync_ui.conflict.delete_title", name = name.clone()))
                .description(prose(t!("sync_ui.conflict.delete_description")))
                .ok_text(t!("sync_ui.conflict.delete_ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let key = key.clone();
                    let _ = page.update(cx, |page, cx| page.run_use_other(key, window, cx));
                    true
                })
        });
    }

    fn run_use_other(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let cloud = cloud(cx);
        let record = key.clone();
        tracing::info!(record = %key, "云同步：冲突改用另一边");
        let job = on_core(async move { cloud.use_other_version(&record).await });
        self.busy = Some(Op::UseOther(key));
        self.error = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.busy = None;
                this.model.update(cx, |model, cx| model.refresh(cx));
                if let Err(error) = result {
                    this.error = Some(describe(&error, this.model.read(cx).service()));
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn dismiss_conflicts(&mut self, cx: &mut Context<Self>) {
        cloud(cx).dismiss_conflicts();
        self.model.update(cx, |model, cx| model.refresh(cx));
        cx.notify();
    }

    // ------------------------------------------------------------ 渲染

    fn render_form(&mut self, configured: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let busy = self.busy.is_some();
        let filled = self.is_filled(cx);
        let first_device = self.first_device();
        let testing = self.busy == Some(Op::Test);
        let starting = self.busy == Some(Op::Start);
        let target = RadioGroup::horizontal("sync-target")
            .children(TARGET_KINDS.map(TargetKind::label))
            .selected_index(TARGET_KINDS.iter().position(|k| *k == self.kind))
            .disabled(busy)
            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                if let Some(kind) = TARGET_KINDS.get(*ix)
                    && this.kind != *kind
                {
                    this.kind = *kind;
                    this.invalidate_probe();
                    cx.notify();
                }
            }));
        let form = v_form().child(field().label(t!("sync_ui.form.target")).child(target));
        let form = match self.kind {
            TargetKind::Folder => form.child(
                field()
                    .label(t!("sync_ui.form.folder"))
                    .description(prose(t!("sync_ui.form.folder_hint")))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.path).id("sync-path").disabled(busy)),
                            )
                            .child(
                                Button::new("sync-browse")
                                    .outline()
                                    .label(t!("sync_ui.form.browse"))
                                    .disabled(busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.browse(window, cx)),
                                    ),
                            ),
                    ),
            ),
            TargetKind::WebDav => form
                .child(
                    field()
                        .label(t!("sync_ui.form.url"))
                        .description(prose(t!("sync_ui.form.url_hint")))
                        .child(Input::new(&self.url).id("sync-url").disabled(busy)),
                )
                .children(self.render_allow_http(cx))
                .child(
                    field().child(
                        h_flex()
                            .gap_3()
                            .items_start()
                            .child(
                                div().flex_1().child(
                                    field().label(t!("sync_ui.form.username")).child(
                                        Input::new(&self.username)
                                            .id("sync-username")
                                            .disabled(busy),
                                    ),
                                ),
                            )
                            .child(
                                div().flex_1().child(
                                    field().label(t!("sync_ui.form.webdav_password")).child(
                                        Input::new(&self.webdav_password)
                                            .id("sync-webdav-password")
                                            .mask_toggle()
                                            .disabled(busy),
                                    ),
                                ),
                            ),
                    ),
                ),
            TargetKind::S3 => self.render_s3_fields(form, busy, cx),
            TargetKind::Gist => form
                .child(
                    field()
                        .label(t!("sync_ui.form.token"))
                        .description(prose(t!("sync_ui.form.token_hint")))
                        .child(
                            Input::new(&self.token)
                                .id("sync-gist-token")
                                .mask_toggle()
                                .disabled(busy),
                        ),
                )
                .child(
                    field()
                        .label(t!("sync_ui.form.gist_id"))
                        .description(prose(t!("sync_ui.form.gist_id_hint")))
                        .child(Input::new(&self.gist_id).id("sync-gist-id").disabled(busy)),
                )
                .child(
                    field().child(div().text_xs().text_color(muted).child(prose(t!(
                        "sync_ui.form.gist_limit",
                        limit = GIST_MAX_SIZE >> 20
                    )))),
                ),
        };
        form.child(
            field()
                .label(t!("sync_ui.form.password"))
                .description(prose(t!("sync_ui.form.password_hint")))
                .child(
                    Input::new(&self.password)
                        .id("sync-password")
                        .mask_toggle()
                        .disabled(busy),
                ),
        )
        .when(first_device, |form| {
            form.child(
                field().label(t!("sync_ui.form.confirm")).child(
                    Input::new(&self.confirm)
                        .id("sync-password-confirm")
                        .mask_toggle()
                        .disabled(busy),
                ),
            )
        })
        .child(
            field()
                .label(t!("sync_ui.form.device_name"))
                .description(prose(t!("sync_ui.form.device_name_hint")))
                .child(
                    div().w(rems(16.)).child(
                        Input::new(&self.device_name)
                            .id("sync-device-name")
                            .disabled(busy),
                    ),
                ),
        )
        .when_some(self.probe.as_ref(), |form, info| {
            form.child(field().child(render_probe(info, self.kind, cx)))
        })
        .when(configured, |form| {
            form.child(
                field().child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(prose(t!("sync_ui.form.edit_hint"))),
                ),
            )
        })
        .child(
            field().child(
                h_flex()
                    .gap_2()
                    .when(configured, |row| {
                        row.child(
                            Button::new("sync-edit-cancel")
                                .ghost()
                                .label(t!("common.cancel"))
                                .disabled(busy)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.cancel_edit(window, cx)),
                                ),
                        )
                    })
                    .child(
                        Button::new("sync-test")
                            .outline()
                            .label(t!("sync_ui.form.test"))
                            .loading(testing)
                            .disabled(!filled || (busy && !testing))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.test_connection(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("sync-start")
                            .primary()
                            .label(t!("sync_ui.form.start"))
                            .loading(starting)
                            .disabled(!filled || (busy && !starting))
                            .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
                    ),
            ),
        )
        .when_some(self.form_error.clone(), |form, error| {
            form.child(
                field().child(
                    div()
                        .id("sync-form-error")
                        .text_sm()
                        .text_color(danger)
                        .child(prose(error)),
                ),
            )
        })
        .into_any_element()
    }

    /// 地址是 `http://` 时：“允许不安全的 HTTP”与它的风险（按种类说明）。
    fn render_allow_http(&self, cx: &mut Context<Self>) -> Option<Field> {
        if !self.address_is_http(cx) {
            return None;
        }
        let theme = cx.theme();
        let color = if self.allow_http {
            theme.warning
        } else {
            theme.muted_foreground
        };
        let hint = match self.kind {
            TargetKind::S3 => t!("sync_ui.form.allow_http_hint_s3"),
            _ => t!("sync_ui.form.allow_http_hint_webdav"),
        };
        Some(
            field().child(
                v_flex()
                    .gap_1()
                    .child(
                        Checkbox::new("sync-allow-http")
                            .prose_label(t!("sync_ui.form.allow_http"))
                            .checked(self.allow_http)
                            .disabled(self.busy.is_some())
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                this.allow_http = *checked;
                                this.invalidate_probe();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("sync-allow-http-hint")
                            .pl_6()
                            .text_xs()
                            .text_color(color)
                            .child(prose(hint)),
                    ),
            ),
        )
    }

    /// S3 兼容存储的字段：终端节点、区域、桶、路径前缀、访问密钥。
    fn render_s3_fields(&self, form: Form, busy: bool, cx: &mut Context<Self>) -> Form {
        let pair = |left: AnyElement, right: AnyElement| {
            field().child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .child(div().flex_1().min_w_0().child(left))
                    .child(div().flex_1().min_w_0().child(right)),
            )
        };
        form.child(
            field()
                .label(t!("sync_ui.form.endpoint"))
                .description(prose(t!("sync_ui.form.endpoint_hint")))
                .child(
                    Input::new(&self.endpoint)
                        .id("sync-s3-endpoint")
                        .disabled(busy),
                ),
        )
        .children(self.render_allow_http(cx))
        .child(pair(
            field()
                .label(t!("sync_ui.form.bucket"))
                .child(Input::new(&self.bucket).id("sync-s3-bucket").disabled(busy))
                .into_any_element(),
            field()
                .label(t!("sync_ui.form.region"))
                .description(prose(t!("sync_ui.form.region_hint")))
                .child(Input::new(&self.region).id("sync-s3-region").disabled(busy))
                .into_any_element(),
        ))
        .child(
            field()
                .label(t!("sync_ui.form.prefix"))
                .description(prose(t!("sync_ui.form.prefix_hint")))
                .child(Input::new(&self.prefix).id("sync-s3-prefix").disabled(busy)),
        )
        .child(pair(
            field()
                .label(t!("sync_ui.form.access_key"))
                .child(
                    Input::new(&self.access_key)
                        .id("sync-s3-access-key")
                        .disabled(busy),
                )
                .into_any_element(),
            field()
                .label(t!("sync_ui.form.secret_key"))
                .child(
                    Input::new(&self.secret_key)
                        .id("sync-s3-secret-key")
                        .mask_toggle()
                        .disabled(busy),
                )
                .into_any_element(),
        ))
        .child(
            field().child(
                v_flex()
                    .gap_1()
                    .child(
                        Checkbox::new("sync-s3-path-style")
                            .prose_label(t!("sync_ui.form.path_style"))
                            .checked(self.path_style)
                            .disabled(busy)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                this.path_style = *checked;
                                this.invalidate_probe();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .pl_6()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(prose(t!("sync_ui.form.path_style_hint"))),
                    ),
            ),
        )
    }

    fn render_status(
        &mut self,
        status: &SyncStatus,
        config: &SyncConfig,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, danger, warning, border, radius) = (
            theme.muted_foreground,
            theme.danger,
            theme.warning,
            theme.border,
            theme.radius,
        );
        let busy = self.busy.is_some();
        let syncing = status.syncing || self.busy == Some(Op::SyncNow);
        let auto = self.auto_override.unwrap_or(config.auto);
        let target_kind = TargetKind::of(&config.target).label();
        let location = config.target.location();
        // S3 的终端节点、Gist 的网址：位置下面的一行
        let endpoint = match &config.target {
            SyncTarget::S3 { endpoint, .. } => Some(endpoint.clone()),
            _ => None,
        };
        let gist_url = match &config.target {
            SyncTarget::Gist { .. } => Some(location.clone()),
            _ => None,
        };
        let service = config.target.service();
        let state_line = if status.syncing {
            h_flex()
                .gap_2()
                .child(Spinner::new().small())
                .child(prose(t!("sync_ui.status.syncing")))
                .into_any_element()
        } else if let Some(at) = status.last_sync {
            div()
                .child(prose(t!(
                    "sync_ui.status.last_sync",
                    time = format_local(at)
                )))
                .into_any_element()
        } else {
            div()
                .text_color(muted)
                .child(t!("sync_ui.status.never"))
                .into_any_element()
        };
        let report = status.last_report.as_ref().filter(|_| !status.syncing);
        let report_line = report.map(|r| {
            if r.pulled == 0 && r.pushed == 0 {
                t!("sync_ui.status.no_changes")
            } else {
                t!(
                    "sync_ui.status.report",
                    pulled = r.pulled,
                    pushed = r.pushed
                )
            }
        });
        let card = v_flex()
            .id("sync-status")
            .gap_1p5()
            .p_3()
            .rounded(radius)
            .border_1()
            .border_color(border)
            .text_sm()
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(div().flex_none().text_color(muted).child(target_kind))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                                    .child(location),
                            )
                            .when_some(endpoint, |col, endpoint| {
                                col.child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                                        .child(endpoint),
                                )
                            }),
                    )
                    .when_some(gist_url, |row, url| {
                        row.child(
                            Button::new("sync-open-gist")
                                .ghost()
                                .xsmall()
                                .flex_none()
                                .label(t!("sync_ui.status.open_gist"))
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    }),
            )
            .child(div().text_color(muted).child(prose(t!(
                "sync_ui.status.this_device",
                name = config.device_name.clone()
            ))))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(state_line)
                    .when_some(report_line, |row, line| {
                        row.child(div().text_color(muted).child(line))
                    }),
            )
            .when_some(status.last_error.as_ref(), |card, error| {
                card.child(
                    div()
                        .id("sync-last-error")
                        .text_color(danger)
                        .child(prose(t!(
                            "sync_ui.status.failed",
                            error = describe(error, service)
                        ))),
                )
            })
            .when(report.is_some_and(|r| r.reuploaded), |card| {
                card.child(
                    div()
                        .text_color(warning)
                        .child(prose(t!("sync_ui.status.reuploaded"))),
                )
            })
            .when(report.is_some_and(|r| r.lost_write), |card| {
                card.child(
                    div()
                        .text_color(warning)
                        .child(prose(t!("sync_ui.status.lost_write"))),
                )
            });

        let pending_note = status.pending.as_ref().map(|pending| {
            h_flex()
                .id("sync-pending-note")
                .gap_3()
                .p_3()
                .rounded(radius)
                .bg(warning.opacity(0.12))
                .text_sm()
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .small()
                        .flex_none()
                        .text_color(warning),
                )
                .child(div().flex_1().child(prose(match pending {
                    Pending::FirstSync { .. } => t!("sync_ui.status.pending_first"),
                    Pending::ConfirmDeletes { .. } => t!("sync_ui.status.pending_deletes"),
                })))
                .child(
                    Button::new("sync-answer")
                        .small()
                        .label(t!("sync_ui.status.answer"))
                        .disabled(busy)
                        .on_click(|_, window, cx| answer_pending(window, cx)),
                )
        });

        let disabling = self.busy == Some(Op::Disable);
        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("sync-now")
                    .icon(IconName::RefreshCw)
                    .label(t!("sync_ui.status.sync_now"))
                    .loading(syncing)
                    .disabled(busy && !syncing)
                    .on_click(cx.listener(|this, _, window, cx| this.sync_now(window, cx))),
            )
            .child(
                Button::new("sync-edit")
                    .outline()
                    .label(t!("sync_ui.status.edit"))
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.edit(window, cx))),
            )
            .child(div().flex_1())
            .child(
                Button::new("sync-disable")
                    .ghost()
                    .label(t!("sync_ui.status.disable"))
                    .loading(disabling)
                    .disabled(busy && !disabling)
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_disable(window, cx))),
            );

        let options = v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Switch::new("sync-auto")
                            .label(t!("sync_ui.status.auto"))
                            .checked(auto)
                            .disabled(busy)
                            .on_change(cx.listener(|this, on: &bool, window, cx| {
                                this.set_auto(*on, window, cx)
                            })),
                    )
                    .child(
                        div().w(rems(10.)).child(
                            Select::new(&self.interval)
                                .id("sync-interval")
                                .small()
                                .disabled(!auto || busy),
                        ),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("sync_ui.status.auto_hint"))),
            );

        let this_device = status.device_id.as_deref();
        let devices = (!status.devices.is_empty()).then(|| {
            let last = status.devices.len() - 1;
            v_flex()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_medium()
                        .child(t!("sync_ui.devices.title")),
                )
                .child(
                    v_flex()
                        .id("sync-devices")
                        .rounded(radius)
                        .border_1()
                        .border_color(border)
                        .children(status.devices.iter().enumerate().map(|(i, device)| {
                            div()
                                .px_3()
                                .py_1p5()
                                .when(i < last, |row| row.border_b_1().border_color(border))
                                .child(device_row(device, this_device, cx))
                        })),
                )
        });

        let conflicts = (!status.conflicts.is_empty()).then(|| {
            let last = status.conflicts.len() - 1;
            v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .child(t!("sync_ui.conflict.title")),
                        )
                        .child(
                            Button::new("sync-conflicts-dismiss")
                                .ghost()
                                .xsmall()
                                .label(t!("sync_ui.conflict.dismiss"))
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| this.dismiss_conflicts(cx))),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(prose(t!("sync_ui.conflict.hint"))),
                )
                .child(
                    v_flex()
                        .id("sync-conflicts")
                        .rounded(radius)
                        .border_1()
                        .border_color(border)
                        .children(status.conflicts.iter().enumerate().map(|(i, conflict)| {
                            self.render_conflict(conflict, cx)
                                .when(i < last, |row| row.border_b_1().border_color(border))
                        })),
                )
        });

        v_flex()
            .gap_4()
            .child(card)
            .children(pending_note)
            .child(
                v_flex()
                    .gap_2()
                    .child(actions)
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            div()
                                .id("sync-error")
                                .text_sm()
                                .text_color(danger)
                                .child(prose(error)),
                        )
                    }),
            )
            .child(options)
            .children(devices)
            .children(conflicts)
            .into_any_element()
    }

    fn render_conflict(
        &self,
        conflict: &Conflict,
        cx: &Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let muted = cx.theme().muted_foreground;
        let resolution = Resolution::of(conflict);
        let busy = self.busy.is_some();
        let running = self.busy.as_ref() == Some(&Op::UseOther(conflict.key.clone()));
        h_flex()
            .id(SharedString::from(format!(
                "sync-conflict-{}",
                conflict.key
            )))
            .gap_2()
            .px_3()
            .py_1p5()
            .text_sm()
            .child(
                div()
                    .w(rems(6.))
                    .flex_none()
                    .text_color(muted)
                    .child(kind_label(conflict.kind)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().child(conflict_name(conflict)))
                    .child(div().text_xs().text_color(muted).child(resolution.text())),
            )
            .when(conflict.can_use_other, |row| {
                let target = conflict.clone();
                row.child(
                    Button::new(SharedString::from(format!(
                        "sync-conflict-use-{}",
                        conflict.key
                    )))
                    .outline()
                    .xsmall()
                    .label(if resolution.other_is_delete() {
                        t!("sync_ui.conflict.use_delete")
                    } else {
                        t!("sync_ui.conflict.use_other")
                    })
                    .loading(running)
                    .disabled(busy && !running)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.use_other(target.clone(), window, cx)
                    })),
                )
            })
    }
}

/// 测试连接的结果：第一台设备的说明，或同步位置中已有的数据量与设备。
fn render_probe(info: &RemoteInfo, kind: TargetKind, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let frame = v_flex()
        .id("sync-probe")
        .gap_2()
        .p_3()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .text_sm();
    if !info.exists {
        return frame.child(
            h_flex()
                .items_start()
                .gap_2()
                .child(
                    Icon::new(IconName::Info)
                        .small()
                        .flex_none()
                        .text_color(muted),
                )
                // 说明要能在框内换行：占满剩余宽度、允许收缩
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(prose(if kind == TargetKind::Gist {
                            t!("sync_ui.probe.first_device_gist")
                        } else {
                            t!("sync_ui.probe.first_device")
                        })),
                ),
        );
    }
    let rows = count_rows(&Default::default(), &info.counts);
    frame
        .child(
            h_flex()
                .items_start()
                .gap_2()
                .child(
                    Icon::new(IconName::CircleCheck)
                        .small()
                        .flex_none()
                        .text_color(theme.success),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(prose(t!("sync_ui.probe.existing"))),
                ),
        )
        .map(|frame| {
            if rows.is_empty() {
                frame.child(
                    div()
                        .text_color(muted)
                        .child(prose(t!("sync_ui.probe.empty"))),
                )
            } else {
                frame.child(h_flex().flex_wrap().gap_x_4().gap_y_1().children(
                    rows.into_iter().map(|row| {
                        h_flex()
                            .gap_1()
                            .child(div().text_color(muted).child(count_label(row.kind)))
                            .child(row.remote.to_string())
                    }),
                ))
            }
        })
        .when(!info.devices.is_empty(), |frame| {
            frame.child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("sync_ui.probe.devices")),
                    )
                    .children(info.devices.iter().map(|d| device_row(d, None, cx))),
            )
        })
}

/// 设备列表的一行：名称（本机标出）、最近一次写入时的版本与时间。
fn device_row(device: &Device, this_device: Option<&str>, cx: &App) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    let name = if device.name.is_empty() {
        device.id.clone()
    } else {
        device.name.clone()
    };
    h_flex()
        .id(SharedString::from(format!("sync-device-{}", device.id)))
        .gap_2()
        .text_sm()
        .child(div().min_w_0().truncate().child(name))
        .when(this_device == Some(device.id.as_str()), |row| {
            row.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("sync_ui.devices.this_device"))),
            )
        })
        .child(div().flex_1())
        .child(
            div()
                .w(rems(8.))
                .flex_none()
                .text_xs()
                .text_color(muted)
                .when(!device.app.is_empty(), |cell| {
                    cell.child(t!("sync_ui.devices.version", version = device.app.clone()))
                }),
        )
        .child(
            div()
                .w(rems(8.))
                .flex_none()
                .text_right()
                .text_xs()
                .text_color(muted)
                .child(format_local(device.last_sync)),
        )
}

impl Render for SyncPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.model.read(cx).status().clone();
        let muted = cx.theme().muted_foreground;
        let body = match status.config.clone() {
            Some(config) if !self.editing => self.render_status(&status, &config, cx),
            config => self.render_form(config.is_some(), cx),
        };
        v_flex()
            .id("sync-page")
            .gap_4()
            .on_action(
                cx.listener(|this, _: &gpui_kit::base::actions::Confirm, window, cx| {
                    this.on_confirm(window, cx)
                }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("sync_ui.description"))),
            )
            .child(body)
    }
}

/// 表单的预填内容（已有的设置；秘密不预填）。
#[derive(Default)]
struct Prefill {
    path: String,
    url: String,
    username: String,
    allow_http: bool,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    access_key: String,
    path_style: bool,
    gist_id: String,
}

impl Prefill {
    fn of(target: Option<&SyncTarget>) -> Self {
        match target {
            Some(SyncTarget::Folder { path }) => Self {
                path: path.display().to_string(),
                ..Default::default()
            },
            // 旧的设置中 allow_http 默认允许：地址不是 http:// 时不预先勾选
            Some(SyncTarget::WebDav {
                url,
                username,
                allow_http,
            }) => Self {
                url: url.clone(),
                username: username.clone(),
                allow_http: *allow_http && is_http(url),
                ..Default::default()
            },
            Some(SyncTarget::S3 {
                endpoint,
                region,
                bucket,
                prefix,
                access_key_id,
                path_style,
                allow_http,
            }) => Self {
                endpoint: endpoint.clone(),
                region: region.clone(),
                bucket: bucket.clone(),
                prefix: prefix.clone(),
                access_key: access_key_id.clone(),
                path_style: *path_style,
                allow_http: *allow_http && is_http(endpoint),
                ..Default::default()
            },
            Some(SyncTarget::Gist { gist_id }) => Self {
                gist_id: gist_id.clone(),
                ..Default::default()
            },
            None => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> FormValues {
        FormValues {
            path: if cfg!(windows) {
                r"D:\Sync\termlet".into()
            } else {
                "/sync/termlet".into()
            },
            password: "correct horse".into(),
            ..Default::default()
        }
    }

    #[test]
    fn folder_needs_an_absolute_path() {
        let mut v = values();
        v.path = "  ".into();
        assert_eq!(build_input(&v, false).err(), Some(FormError::PathRequired));
        v.path = "relative/dir".into();
        assert_eq!(
            build_input(&v, false).err(),
            Some(FormError::PathNotAbsolute)
        );
        let input = build_input(&values(), false).unwrap();
        assert!(matches!(input.target, SyncTarget::Folder { .. }));
        assert!(input.credential.expose().is_empty(), "本机文件夹不带凭据");
    }

    #[test]
    fn webdav_needs_an_address_and_trims_the_account() {
        let mut v = values();
        v.kind = TargetKind::WebDav;
        assert_eq!(build_input(&v, false).err(), Some(FormError::UrlRequired));
        v.url = " https://dav.jianguoyun.com/dav/termlet/ ".into();
        v.username = " me@example.com ".into();
        v.webdav_password = " app pass ".into();
        let input = build_input(&v, false).unwrap();
        assert_eq!(
            input.target,
            SyncTarget::WebDav {
                url: "https://dav.jianguoyun.com/dav/termlet/".into(),
                username: "me@example.com".into(),
                allow_http: false,
            }
        );
        assert_eq!(input.credential.expose(), " app pass ", "密码原样保留");
        // 勾选了允许 HTTP 而地址是 https://：不带上这个允许
        v.allow_http = true;
        let SyncTarget::WebDav { allow_http, .. } = build_input(&v, false).unwrap().target else {
            panic!("应是 WebDAV")
        };
        assert!(!allow_http);
        v.url = "HTTP://nas.local/dav/".into();
        let SyncTarget::WebDav { allow_http, .. } = build_input(&v, false).unwrap().target else {
            panic!("应是 WebDAV")
        };
        assert!(allow_http);
    }

    #[test]
    fn s3_needs_endpoint_bucket_and_keys() {
        let mut v = values();
        v.kind = TargetKind::S3;
        v.webdav_password = "not this".into();
        assert_eq!(
            build_input(&v, false).err(),
            Some(FormError::EndpointRequired)
        );
        v.endpoint = " http://192.168.1.10:9000 ".into();
        assert_eq!(
            build_input(&v, false).err(),
            Some(FormError::BucketRequired)
        );
        v.bucket = " termlet ".into();
        assert_eq!(
            build_input(&v, false).err(),
            Some(FormError::AccessKeyRequired)
        );
        v.access_key = " AKID ".into();
        assert_eq!(
            build_input(&v, false).err(),
            Some(FormError::SecretKeyRequired)
        );
        v.secret_key = " secret ".into();
        v.prefix = " /team/ ".into();
        v.path_style = true;
        v.allow_http = true;
        let input = build_input(&v, false).unwrap();
        assert_eq!(
            input.target,
            SyncTarget::S3 {
                endpoint: "http://192.168.1.10:9000".into(),
                region: String::new(),
                bucket: "termlet".into(),
                prefix: "/team/".into(),
                access_key_id: "AKID".into(),
                path_style: true,
                allow_http: true,
            }
        );
        assert_eq!(input.credential.expose(), "secret", "只带 S3 的密钥");
    }

    #[test]
    fn gist_needs_a_token() {
        let mut v = values();
        v.kind = TargetKind::Gist;
        assert_eq!(build_input(&v, false).err(), Some(FormError::TokenRequired));
        v.token = " ghp_x ".into();
        let input = build_input(&v, false).unwrap();
        assert_eq!(
            input.target,
            SyncTarget::Gist {
                gist_id: String::new()
            }
        );
        assert_eq!(input.credential.expose(), "ghp_x");
        assert!(is_http("HTTP://x") && !is_http("https://x") && !is_http("http:/"));
    }

    #[test]
    fn first_device_types_the_password_twice() {
        let mut v = values();
        v.password = "short".into();
        assert_eq!(
            build_input(&v, false).err(),
            Some(FormError::PasswordTooShort)
        );
        let mut v = values();
        assert!(build_input(&v, false).is_ok(), "已有同步文件时只输入一次");
        assert_eq!(
            build_input(&v, true).err(),
            Some(FormError::ConfirmRequired)
        );
        v.confirm = "correct horsE".into();
        assert_eq!(
            build_input(&v, true).err(),
            Some(FormError::ConfirmMismatch)
        );
        v.confirm = v.password.clone();
        let input = build_input(&v, true).unwrap();
        assert_eq!(input.password.expose(), "correct horse");
    }

    #[test]
    fn device_name_is_trimmed_and_limited() {
        let mut v = values();
        v.device_name = "  办公室  ".into();
        assert_eq!(build_input(&v, false).unwrap().device_name, "办公室");
        v.device_name = "长".repeat(MAX_DEVICE_NAME + 1);
        assert_eq!(
            build_input(&v, false).err(),
            Some(FormError::DeviceNameTooLong)
        );
        v.device_name = "长".repeat(MAX_DEVICE_NAME);
        assert!(build_input(&v, false).is_ok(), "按字符计数");
    }

    #[test]
    fn interval_choices_include_the_current_value() {
        assert_eq!(interval_values(10), vec![5, 10, 30, 60]);
        assert_eq!(interval_values(15), vec![5, 10, 15, 30, 60]);
        assert_eq!(interval_values(1440), vec![5, 10, 30, 60, 1440]);
    }
}
