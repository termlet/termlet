//! 回放 Tab：在中间区域回放会话录制（asciicast `.cast`，v1 / v2 / v3，见 [`termlet_core::recording::cast`]），
//! 与终端、文件管理的 Tab 一样可以切换、分屏、关闭；布局不保存它（工作区只保存终端与文件管理）。
//!
//! - 画面由终端引擎重放（[`TerminalEngine::replay`]）：与实时的终端同一套解析与绘制，网格是录制时的大小（录制中的
//!   大小变化照样生效），放不下时缩小字号；可以选择、复制文字（右键菜单、Ctrl+Shift+C、Ctrl+C），查找（与终端相同的
//!   快捷键）。
//! - 播放、暂停（空格），进度条拖动跳转，←、→ 前后跳 5 秒，Home、End 到开头、结尾；倍速 1× / 2× / 4×；跳过长停顿
//!   （超过上限的停顿缩短为上限：录制文件给出的 `idle_time_limit`，没有时 2 秒；文件给出了上限时默认开启）。
//! - 跳转（进度见 [`Playhead`]）：往后时把中间的输出接着送进引擎；往前时终端回不去，另建一个回放引擎从不晚于目标的
//!   最近一份关键帧（没有时从头）送到目标位置，等它解析完再换上（画面不会先闪一下空白）。往后跳得很远、中间有关键帧
//!   时同样从关键帧开始。关键帧在一路放过去时顺带存下，内存有上限（见 [`keyframes`]；大文件往前跳的耗时见
//!   `seek_back_benchmark`）。拖动进度条时重放还没好就只记下最新的目标，好了再跳，不堆积。
//! - 配色：录制文件头里有配色（asciicast 的 `theme`，Termlet 录制时写进去）时默认用它，Tab 上可切换成当前的终端
//!   配色（工具栏的“录制时的配色”、Tab 菜单）；没有时用当前的。
//! - 打开：命令面板“打开录制文件…”、录制完成的提示中“回放”、把 `.cast` 文件拖进窗口（见 `workspace/replay_tabs.rs`）。
//!   文件在后台读入，读不了时说明原因。

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input as text_input;
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, KeyBinding, OperatingSystem,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, TestSupportExt as _, Window, actions, div, prelude::FluentBuilder as _,
};
use termlet_core::recording::cast::{self, Cast, CastError, CastTheme};
use termlet_core::recording::player::{DEFAULT_IDLE_LIMIT, Playhead, SPEEDS, Seek, Timeline};
use termlet_terminal::{
    EngineOptions, GridSize, Palette, ReplayFeed, Rgb, RightClick, TerminalEngine, TerminalStyle,
    TerminalView,
};

use self::keyframes::{Keyframes, Pending as PendingKeyframe, feed_events, output_bytes};
use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;
use crate::terminal_colors::{self, SchemeLevels};
use crate::terminal_panel::clock_text;
use crate::terminal_search::{OpenSearch, SearchBar, SearchBarEvent};

mod keyframes;

/// 面板在持久化布局中的名称，发布后不能改。
const PANEL_NAME: &str = "ReplayPanel";
pub(crate) const KEY_CONTEXT: &str = "ReplayPanel";
/// 回放的快捷键只在回放的终端获得焦点时生效：焦点在查找栏里时空格照常输入。
fn terminal_context() -> String {
    format!(
        "{} && {}",
        termlet_terminal::view::KEY_CONTEXT,
        termlet_terminal::view::REPLAY
    )
}

/// ←、→ 跳多少秒。
const SEEK_STEP: f64 = 5.;
/// 播放中最多隔这么久更新一次进度与时间；下一个事件更早到期时提前醒。
const TICK: Duration = Duration::from_millis(50);
/// 事件很密时两次推进之间至少隔这么久（同一段时间到期的事件一次送进引擎）。
const MIN_TICK: Duration = Duration::from_millis(4);

actions!(
    replay,
    [
        /// 播放 / 暂停
        TogglePlay,
        /// 后退 5 秒
        SeekBackward,
        /// 前进 5 秒
        SeekForward,
        /// 跳到开头
        SeekStart,
        /// 跳到结尾
        SeekEnd,
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys(key_bindings(cx.platform_info().os));
}

/// 回放固定的快捷键（各系统相同）。
pub(crate) fn key_bindings(_: OperatingSystem) -> Vec<KeyBinding> {
    let context = terminal_context();
    let context = Some(context.as_str());
    vec![
        KeyBinding::new("space", TogglePlay, context),
        KeyBinding::new("left", SeekBackward, context),
        KeyBinding::new("right", SeekForward, context),
        KeyBinding::new("home", SeekStart, context),
        KeyBinding::new("end", SeekEnd, context),
    ]
}

/// 回放 Tab 请工作区做的事。
pub enum ReplayPanelEvent {
    /// 关闭这个 Tab
    Close,
}

/// 读不了录制文件的原因（给用户看的）。
pub fn describe_error(error: &CastError) -> SharedString {
    match error {
        CastError::Io(e) => t!("replay.error.read", error = e.to_string()),
        CastError::NotCast => t!("replay.error.not_cast"),
        CastError::Version(version) => t!("replay.error.version", version = version),
        CastError::TooLarge => t!("replay.error.too_large", size = cast::MAX_READ_BYTES >> 20),
    }
}

/// 在后台读入录制文件。
pub fn load(path: PathBuf, cx: &App) -> Task<Result<Cast, CastError>> {
    cx.background_executor()
        .spawn(async move { cast::read_file(&path) })
}

/// 跳回前面时正在从头重放的引擎：解析完换上。
struct Pending {
    seq: u64,
    engine: TerminalEngine,
    feed: ReplayFeed,
}

pub struct ReplayPanel {
    path: PathBuf,
    cast: Arc<Cast>,
    /// 录制时的配色（文件头里有时）
    recorded: Option<Palette>,
    /// 用录制时的配色（否则用当前的终端配色）
    use_recorded: bool,
    /// 往前跳用的关键帧
    keyframes: Keyframes,
    /// 最近一次重放从第几个事件开始（测试看用没用关键帧）
    #[cfg(test)]
    rebuilt_from: Option<usize>,
    view: Entity<TerminalView>,
    /// 视图中正在显示的引擎的输入端
    feed: ReplayFeed,
    pending: Option<Pending>,
    /// 重放还没好时又要跳转（拖动进度条）：记下最新的目标，好了再跳
    queued: Option<f64>,
    head: Playhead,
    skip_idle: bool,
    slider: Entity<SliderState>,
    /// 用户正在拖动进度条：计时器不改进度条
    dragging: bool,
    search: Option<Entity<SearchBar>>,
    focus: FocusHandle,
    /// 上一次推进的时间
    last_tick: Option<Instant>,
    /// 重放的序号：作废还没好的重放
    seq: u64,
    _clock: Option<Task<()>>,
    _rebuild: Option<Task<()>>,
    _search_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for ReplayPanel {}
impl EventEmitter<ReplayPanelEvent> for ReplayPanel {}

impl Focusable for ReplayPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

/// 回放用的终端样式：与终端相同的字体，右键弹出菜单（复制、全选、查找）；配色为 `recorded`（录制时的），没有时
/// 用当前的终端配色。
fn replay_style(recorded: Option<&Palette>, cx: &App) -> TerminalStyle {
    let store = AppState::store(cx);
    let mut style = terminal_colors::terminal_style(
        &store,
        &store.settings(),
        cx.theme().is_dark(),
        &SchemeLevels::default(),
    );
    style.right_click = RightClick::Menu;
    if let Some(palette) = recorded {
        style.palette = palette.clone();
        // 关键词高亮用这套配色的 1–6 号色
        style.highlight_colors = None;
    }
    style
}

/// 录制文件头里的配色（`#rrggbb`）换成终端的调色板：16 色只给了 8 个时亮色与普通色相同（与 asciinema 的播放器
/// 相同）；光标用前景色，选中的背景由背景向前景混合。颜色写错、少于 8 个时为 `None`（用当前的配色）。
pub(crate) fn recorded_palette(theme: &CastTheme) -> Option<Palette> {
    let parse = |hex: &str| {
        termlet_core::schemes::Rgb::parse(hex.trim()).map(|c| Rgb {
            r: c.r,
            g: c.g,
            b: c.b,
        })
    };
    let foreground = parse(&theme.foreground)?;
    let background = parse(&theme.background)?;
    let colors: Vec<Rgb> = theme
        .palette
        .iter()
        .take(16)
        .map(|hex| parse(hex))
        .collect::<Option<_>>()?;
    if colors.len() < 8 {
        return None;
    }
    let mut ansi = [foreground; 16];
    for (i, slot) in ansi.iter_mut().enumerate() {
        *slot = colors.get(i).copied().unwrap_or(colors[i % 8]);
    }
    let to = |c: Rgb| termlet_core::schemes::Rgb::new(c.r, c.g, c.b);
    let selection = to(background).mix(to(foreground), 0.3);
    Some(Palette {
        foreground,
        background,
        cursor: foreground,
        cursor_text: None,
        selection_background: Rgb {
            r: selection.r,
            g: selection.g,
            b: selection.b,
        },
        selection_foreground: None,
        ansi,
    })
}

/// 新的回放引擎：网格为 `size`（录制开始时、关键帧时的大小）。
fn replay_engine(size: GridSize, cx: &App) -> (TerminalEngine, ReplayFeed) {
    let scrollback = AppState::store(cx).settings().terminal.scrollback as usize;
    TerminalEngine::replay(EngineOptions { scrollback, size })
}

/// 录制开始时的网格大小。
fn start_size(cast: &Cast) -> GridSize {
    GridSize::new(cast.header.width, cast.header.height)
}

fn idle_limit(cast: &Cast, skip_idle: bool) -> Option<f64> {
    skip_idle.then(|| cast.header.idle_time_limit.unwrap_or(DEFAULT_IDLE_LIMIT))
}

/// 秒数写成 `m:ss`（一小时以上 `h:mm:ss`）。
fn time_text(seconds: f64) -> SharedString {
    clock_text(Duration::from_secs_f64(seconds.max(0.)))
}

fn speed_label(speed: f64) -> SharedString {
    format!("{speed}×").into()
}

impl ReplayPanel {
    /// 打开一个读入好的录制，从头开始播放。
    pub fn new(path: PathBuf, cast: Cast, window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx: &mut Context<Self>| {
            let cast = Arc::new(cast);
            let (engine, feed) = replay_engine(start_size(&cast), cx);
            let recorded = cast.header.theme.as_ref().and_then(recorded_palette);
            let style = replay_style(recorded.as_ref(), cx);
            let view = cx.new(|cx| {
                let mut view = TerminalView::new(engine, style, window, cx);
                view.set_replay(true, cx);
                view
            });
            let skip_idle = cast.header.idle_time_limit.is_some();
            let head = Playhead::new(Timeline::new(&cast, idle_limit(&cast, skip_idle)));
            let slider = cx.new(|_| SliderState::new().min(0.).max(1.).step(0.0001));
            let subscriptions = vec![cx.subscribe_in(
                &slider,
                window,
                |this: &mut Self, _, event: &SliderEvent, window, cx| match event {
                    SliderEvent::Change(value) => {
                        this.dragging = true;
                        this.seek(value.start() as f64 * this.head.duration(), window, cx);
                    }
                    SliderEvent::Release(value) => {
                        this.dragging = false;
                        this.seek(value.start() as f64 * this.head.duration(), window, cx);
                        this.focus_terminal(window, cx);
                    }
                },
            )];
            tracing::info!(
                version = cast.version,
                events = cast.events.len(),
                bytes = cast.output_bytes(),
                skipped = cast.skipped,
                recorded_colors = recorded.is_some(),
                "打开回放"
            );
            let mut this = Self {
                path,
                cast,
                use_recorded: recorded.is_some(),
                recorded,
                keyframes: Keyframes::default(),
                #[cfg(test)]
                rebuilt_from: None,
                view,
                feed,
                pending: None,
                queued: None,
                head,
                skip_idle,
                slider,
                dragging: false,
                search: None,
                focus: cx.focus_handle(),
                last_tick: None,
                seq: 0,
                _clock: None,
                _rebuild: None,
                _search_subscription: None,
                _subscriptions: subscriptions,
            };
            // 第 0 秒的事件（录制开始时的屏幕）先显示出来，然后开始播放
            let first = this.head.seek(0.);
            this.apply(first, window, cx);
            this.play(window, cx);
            this
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Tab 上的标题（“回放 · 文件名”）。
    pub fn title_text(&self) -> SharedString {
        let name = self
            .path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        t!("replay.title", name = name)
    }

    pub(crate) fn close_tab(&mut self, cx: &mut Context<Self>) {
        cx.emit(ReplayPanelEvent::Close);
    }

    /// 设置、外观变了：换上新的终端样式。
    pub fn restyle(&mut self, cx: &mut Context<Self>) {
        let recorded = self.recorded.as_ref().filter(|_| self.use_recorded);
        let style = replay_style(recorded, cx);
        self.view.update(cx, |view, cx| view.set_style(style, cx));
    }

    /// 文件头里有录制时的配色。
    pub fn has_recorded_colors(&self) -> bool {
        self.recorded.is_some()
    }

    /// 正在用录制时的配色。
    pub fn uses_recorded_colors(&self) -> bool {
        self.use_recorded && self.recorded.is_some()
    }

    /// 切换录制时的配色与当前的终端配色。
    pub fn set_recorded_colors(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.use_recorded == on {
            return;
        }
        self.use_recorded = on;
        tracing::debug!(recorded = on, "回放切换配色");
        self.restyle(cx);
        cx.notify();
    }

    /// 现在的位置（秒）。
    pub fn position(&self) -> f64 {
        self.queued.unwrap_or(self.head.position())
    }

    fn apply(&mut self, seek: Seek, window: &mut Window, cx: &mut Context<Self>) {
        match seek {
            Seek::Forward(range) => {
                // 往后跳得很远、中间有关键帧：从关键帧开始比把中间的输出都送一遍快
                let skip = self
                    .keyframes
                    .before(range.end)
                    .filter(|k| k.index > range.start)
                    .is_some_and(|k| {
                        output_bytes(&self.cast, range.start..k.index) >= keyframes::SPACING
                    });
                if skip {
                    self.rebuild(range.end, window, cx);
                } else {
                    self.feed_range(range, cx);
                }
            }
            Seek::Restart(range) => self.rebuild(range.end, window, cx),
        }
    }

    /// 把 `range` 中的事件送进正在送的引擎（跳回前面、重放还没好时是新的那个），途中按间隔存关键帧。
    fn feed_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let feed = self.pending.as_ref().map_or(&self.feed, |p| &p.feed);
        feed_events(
            feed,
            &self.cast,
            range,
            &mut self.keyframes,
            |index, wait| Self::keep_keyframe(index, wait, cx),
        );
    }

    /// 引擎存好关键帧后收下。
    fn keep_keyframe(index: usize, wait: PendingKeyframe, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let frame = wait.await;
            let _ = this.update(cx, |this, _| {
                this.keyframes.finish(index, frame);
                tracing::trace!(
                    index,
                    frames = this.keyframes.len(),
                    bytes = this.keyframes.bytes(),
                    "回放存下关键帧"
                );
            });
        })
        .detach();
    }

    /// 另建一个引擎，从不晚于 `end` 的最近一份关键帧（没有时从头）送到第 `end` 个事件，解析完换上。
    fn rebuild(&mut self, end: usize, window: &mut Window, cx: &mut Context<Self>) {
        let start = self
            .keyframes
            .before(end)
            .map(|k| (k.index, k.dump.clone()));
        let size = start.as_ref().map_or(start_size(&self.cast), |(_, dump)| {
            GridSize::new(dump.columns, dump.rows)
        });
        let (engine, feed) = replay_engine(size, cx);
        let started = Instant::now();
        let from = start.as_ref().map_or(0, |(index, _)| *index);
        if let Some((_, dump)) = start {
            feed.output(dump.text.into_bytes());
        }
        let count = end.saturating_sub(from);
        #[cfg(test)]
        {
            self.rebuilt_from = Some(from);
        }
        self.keyframes.restart_at(from);
        feed_events(
            &feed,
            &self.cast,
            from..end,
            &mut self.keyframes,
            |index, wait| Self::keep_keyframe(index, wait, cx),
        );
        let idle = feed.idle();
        self.seq += 1;
        let seq = self.seq;
        // 换掉还没好的那个（丢弃输入端即停止）
        self.pending = Some(Pending { seq, engine, feed });
        self._rebuild = Some(cx.spawn_in(window, async move |this, cx| {
            idle.await;
            let _ = this.update_in(cx, |this, window, cx| {
                tracing::debug!(
                    from,
                    events = count,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "回放重放到目标位置"
                );
                this.finish_rebuild(seq, window, cx)
            });
        }));
    }

    fn finish_rebuild(&mut self, seq: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.as_ref().is_none_or(|p| p.seq != seq) {
            return;
        }
        let Some(pending) = self.pending.take() else {
            return;
        };
        self._rebuild = None;
        self.view
            .update(cx, |view, cx| view.set_engine(pending.engine, window, cx));
        self.feed = pending.feed;
        if let Some(position) = self.queued.take() {
            self.seek(position, window, cx);
        }
        cx.notify();
    }

    /// 跳到 `position`（秒）。
    pub fn seek(&mut self, position: f64, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            self.queued = Some(position.clamp(0., self.head.duration()));
            cx.notify();
            return;
        }
        let seek = self.head.seek(position);
        self.apply(seek, window, cx);
        // 播放中跳转：从新的位置接着计时
        self.last_tick = Some(Instant::now());
        self.sync_slider(window, cx);
        cx.notify();
    }

    fn seek_by(&mut self, delta: f64, window: &mut Window, cx: &mut Context<Self>) {
        self.seek(self.position() + delta, window, cx);
    }

    pub fn toggle_play(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.head.is_playing() {
            self.pause(cx);
        } else {
            self.play(window, cx);
        }
    }

    /// 播放（在末尾时从头开始）。
    pub fn play(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(seek) = self.head.play() {
            self.apply(seek, window, cx);
        }
        if !self.head.is_playing() {
            return;
        }
        self.last_tick = Some(Instant::now());
        self._clock = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let Ok(Some(wait)) = this.update(cx, |this, _| this.next_wait()) else {
                    break;
                };
                cx.background_executor().timer(wait).await;
                match this.update_in(cx, |this, window, cx| this.tick(window, cx)) {
                    Ok(true) => {}
                    _ => break,
                }
            }
        }));
        cx.notify();
    }

    /// Tab 移到别的窗口（见 `workspace/move_tab.rs`）：画面、位置、播放状态都不变；终端视图的焦点订阅在新窗口中重新
    /// 登记，查找栏重建（沿用查找条件）。播放的计时按面板当前所在的窗口执行，画出后自动跟过去。
    pub(crate) fn rebind_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view
            .update(cx, |view, cx| view.rebind_window(window, cx));
        if self.search.take().is_some() {
            self._search_subscription = None;
            self.open_search(&OpenSearch, window, cx);
        }
        cx.notify();
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.head.pause();
        self._clock = None;
        cx.notify();
    }

    pub fn set_speed(&mut self, speed: f64, cx: &mut Context<Self>) {
        // 先按原来的倍速结算到现在，之后按新的倍速
        if let Some(last) = self.last_tick.replace(Instant::now()) {
            let range = self.head.advance(last.elapsed());
            self.feed_range(range, cx);
        }
        self.head.set_speed(speed);
        cx.notify();
    }

    pub fn set_skip_idle(&mut self, skip: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.skip_idle = skip;
        self.head
            .set_timeline(Timeline::new(&self.cast, idle_limit(&self.cast, skip)));
        self.queued = None;
        self.sync_slider(window, cx);
        cx.notify();
    }

    /// 离下一次推进还有多久；没有在播放时为 `None`。
    fn next_wait(&self) -> Option<Duration> {
        self.head
            .is_playing()
            .then(|| self.head.until_next().unwrap_or(TICK).clamp(MIN_TICK, TICK))
    }

    /// 推进一次，返回是否还在播放。
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        let elapsed = self
            .last_tick
            .replace(now)
            .map_or(Duration::ZERO, |last| now - last);
        let range = self.head.advance(elapsed);
        self.feed_range(range, cx);
        if !self.dragging {
            self.sync_slider(window, cx);
        }
        cx.notify();
        self.head.is_playing()
    }

    fn sync_slider(&self, window: &mut Window, cx: &mut Context<Self>) {
        let duration = self.head.duration();
        let value = if duration > 0. {
            (self.position() / duration) as f32
        } else {
            0.
        };
        self.slider
            .update(cx, |slider, cx| slider.set_value(value, window, cx));
    }

    fn focus_terminal(&self, window: &mut Window, cx: &mut App) {
        self.view.focus_handle(cx).focus(window, cx);
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.view.read(cx).selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn open_search(&mut self, _: &OpenSearch, window: &mut Window, cx: &mut Context<Self>) {
        let bar = match &self.search {
            Some(bar) => bar.clone(),
            None => {
                let view = self.view.clone();
                let bar = cx.new(|cx| SearchBar::new(view, window, cx));
                self._search_subscription =
                    Some(
                        cx.subscribe_in(&bar, window, |this, _, event, window, cx| match event {
                            SearchBarEvent::Closed => {
                                this.search = None;
                                this._search_subscription = None;
                                this.focus_terminal(window, cx);
                                cx.notify();
                            }
                        }),
                    );
                self.search = Some(bar.clone());
                bar
            }
        };
        bar.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let playing = self.head.is_playing();
        let speed = self.head.speed();
        let this = cx.entity().downgrade();
        let speed_button = Button::new("replay-speed")
            .ghost()
            .xsmall()
            .dropdown_caret(true)
            .label(speed_label(speed))
            .tooltip(t!("replay.speed"))
            .dropdown_menu({
                let this = this.clone();
                move |menu, _, _| {
                    SPEEDS.into_iter().fold(menu, |menu, choice| {
                        let this = this.clone();
                        menu.item(
                            PopupMenuItem::new(speed_label(choice))
                                .checked(choice == speed)
                                .on_click(move |_, _, cx| {
                                    let _ = this.update(cx, |this, cx| this.set_speed(choice, cx));
                                }),
                        )
                    })
                }
            });
        let limit = self
            .cast
            .header
            .idle_time_limit
            .unwrap_or(DEFAULT_IDLE_LIMIT);
        h_flex()
            .id("replay-toolbar")
            .test_support()
            .flex_none()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("replay-play")
                    .ghost()
                    .small()
                    .icon(if playing {
                        IconName::Pause
                    } else {
                        IconName::Play
                    })
                    .tooltip_with_action(
                        if playing {
                            t!("replay.pause")
                        } else {
                            t!("replay.play")
                        },
                        &TogglePlay,
                        Some(terminal_context().as_str()),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_play(window, cx);
                        this.focus_terminal(window, cx);
                    })),
            )
            .child(
                div()
                    .id("replay-time")
                    .test_support()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!(
                        "replay.time",
                        position = time_text(self.position()),
                        duration = time_text(self.head.duration())
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_16()
                    .child(Slider::new(&self.slider).accessibility_label(t!("replay.progress"))),
            )
            .child(speed_button)
            .child(
                Button::new("replay-skip-idle")
                    .ghost()
                    .xsmall()
                    .icon(IconName::FastForward)
                    .label(t!("replay.skip_idle"))
                    .selected(self.skip_idle)
                    .tooltip(t!("replay.skip_idle_hint", seconds = limit))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let skip = !this.skip_idle;
                        this.set_skip_idle(skip, window, cx);
                        this.focus_terminal(window, cx);
                    })),
            )
            // 录制时的配色 / 当前的终端配色（文件头里有配色时）
            .when(self.has_recorded_colors(), |this| {
                this.child(
                    Button::new("replay-recorded-colors")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Palette)
                        .label(t!("replay.recorded_colors"))
                        .selected(self.uses_recorded_colors())
                        .tooltip(t!("replay.recorded_colors_hint"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            let on = !this.uses_recorded_colors();
                            this.set_recorded_colors(on, cx);
                            this.focus_terminal(window, cx);
                        })),
                )
            })
            .child(
                Button::new("replay-search")
                    .ghost()
                    .small()
                    .icon(IconName::Search)
                    .tooltip_with_action(
                        t!("terminal.menu.find"),
                        &OpenSearch,
                        Some(termlet_terminal::view::KEY_CONTEXT),
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.open_search(&OpenSearch, window, cx)
                        }),
                    ),
            )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let header = &self.cast.header;
        let mut parts: Vec<String> = Vec::new();
        if let Some(title) = &header.title {
            parts.push(title.clone());
        }
        parts.push(format!("{}×{}", header.width, header.height));
        if let Some(timestamp) = header.timestamp {
            let time = termlet_core::time::format_local(timestamp.saturating_mul(1000));
            if !time.is_empty() {
                parts.push(time);
            }
        }
        parts.push(format!("asciicast v{}", self.cast.version));
        h_flex()
            .id("replay-status")
            .test_support()
            .flex_none()
            .px_3()
            .py_1()
            .gap_3()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .border_t_1()
            .border_color(cx.theme().border)
            .child(div().min_w_0().truncate().child(parts.join(" · ")))
            .when(self.cast.skipped > 0, |this| {
                this.child(
                    div()
                        .text_color(cx.theme().warning)
                        .child(prose(t!("replay.skipped_lines", count = self.cast.skipped))),
                )
            })
    }
}

impl BasePanel for ReplayPanel {
    fn panel_name(&self) -> &'static str {
        PANEL_NAME
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.focus_terminal(window, cx);
        }
    }
}

impl ReplayPanel {
    /// 竖排的 Tab 列表中这个 Tab 的一行：与横排的 Tab 上相同的图标与名称，悬停显示文件的路径；不归在哪台主机下。
    pub(crate) fn tab_face(&self, cx: &App) -> crate::workspace::TabFace {
        crate::workspace::TabFace {
            icon: Icon::new(IconName::Clapperboard)
                .xsmall()
                .text_color(cx.theme().info)
                .into_any_element(),
            rail_icon: Icon::new(IconName::Clapperboard)
                .with_size(crate::workspace::RAIL_ICON)
                .text_color(cx.theme().info)
                .into_any_element(),
            name: self.title_text(),
            detail: Some(self.path.display().to_string().into()),
            badges: Vec::new(),
            status: Vec::new(),
            host: None,
        }
    }
}

impl Panel for ReplayPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = cx.entity_id().as_u64() as usize;
        let tip: SharedString = self.path.display().to_string().into();
        h_flex()
            .gap_1()
            .child(
                h_flex()
                    .id(("replay-tab-title", id))
                    .test_support()
                    .gap_1()
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
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
                    .child(
                        Icon::new(IconName::Clapperboard)
                            .xsmall()
                            .text_color(cx.theme().info),
                    )
                    .child(self.title_text()),
            )
            .child(
                // 自己的关闭按钮（Dock 自带的不显示）
                Button::new(("replay-tab-close", id))
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tab_stop(false)
                    .tooltip(t!("panel.close"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_tab(cx);
                    })),
            )
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    /// Tab 菜单（右键 Tab 打开）：播放 / 暂停、录制时的配色（文件头里有时）、在文件夹中显示、关闭与关闭几个 Tab。
    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let this = cx.entity().downgrade();
        let act = |f: fn(&mut ReplayPanel, &mut Window, &mut Context<ReplayPanel>)| {
            let this = this.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| f(this, window, cx));
            }
        };
        let path = self.path.clone();
        let mut menu = menu.item(
            PopupMenuItem::new(if self.head.is_playing() {
                t!("replay.pause")
            } else {
                t!("replay.play")
            })
            .on_click(act(|this, window, cx| this.toggle_play(window, cx))),
        );
        if self.has_recorded_colors() {
            menu = menu.item(
                PopupMenuItem::new(t!("replay.recorded_colors_menu"))
                    .checked(self.uses_recorded_colors())
                    .on_click(act(|this, _, cx| {
                        let on = !this.uses_recorded_colors();
                        this.set_recorded_colors(on, cx);
                    })),
            );
        }
        let menu = menu
            .item(
                PopupMenuItem::new(t!("replay.reveal"))
                    .icon(IconName::FolderOpen)
                    .on_click(move |_, _, cx| cx.reveal_path(&path)),
            )
            .separator()
            .item(
                PopupMenuItem::new(t!("panel.close"))
                    .on_click(act(|this, _, cx| this.close_tab(cx))),
            );
        crate::workspace::close_tab_items(menu, cx.entity_id(), window, cx).separator()
    }
}

impl Render for ReplayPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.view.focus_handle(cx);
        let area = div()
            .id("replay-area")
            .relative()
            .flex_1()
            .min_h_0()
            .child(self.view.clone())
            .when_some(self.search.clone(), |this, bar| {
                this.child(div().absolute().top_2().right_4().child(bar))
            })
            // 只读：右键菜单只有复制、全选与查找
            .context_menu(move |menu, _, _| {
                menu.action_context(focus.clone())
                    .menu(
                        t!("terminal.menu.copy"),
                        Box::new(termlet_terminal::view::Copy),
                    )
                    .menu(
                        t!("terminal.menu.select_all"),
                        Box::new(termlet_terminal::view::SelectAll),
                    )
                    .separator()
                    .menu(t!("terminal.menu.find"), Box::new(OpenSearch))
            });
        v_flex()
            .id("replay-panel")
            .test_support()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|this, _: &TogglePlay, window, cx| this.toggle_play(window, cx)))
            .on_action(cx.listener(|this, _: &SeekBackward, window, cx| {
                this.seek_by(-SEEK_STEP, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SeekForward, window, cx| {
                    this.seek_by(SEEK_STEP, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &SeekStart, window, cx| this.seek(0., window, cx)))
            .on_action(cx.listener(|this, _: &SeekEnd, window, cx| {
                let end = this.head.duration();
                this.seek(end, window, cx)
            }))
            .on_action(cx.listener(Self::open_search))
            // gpui-kit 的 Root 把 Ctrl+C 绑给了输入框的复制：回放中就是复制选中的文字
            .on_action(cx.listener(|this, _: &text_input::Copy, _, cx| this.copy_selection(cx)))
            .child(self.render_toolbar(cx))
            .child(area)
            .child(self.render_status(cx))
    }
}

#[cfg(test)]
impl ReplayPanel {
    pub fn is_playing(&self) -> bool {
        self.head.is_playing()
    }

    pub fn duration(&self) -> f64 {
        self.head.duration()
    }

    pub fn terminal(&self) -> &Entity<TerminalView> {
        &self.view
    }

    /// 关键帧的间隔改成 `spacing` 字节（测试用小录制时）。
    pub fn set_keyframe_spacing(&mut self, spacing: usize) {
        self.keyframes = Keyframes::new(spacing, keyframes::BUDGET);
    }

    /// 存下的关键帧的位置。
    pub fn keyframe_positions(&self) -> Vec<usize> {
        self.keyframes.positions()
    }

    /// 正在从关键帧（或从头）重放、还没换上。
    pub fn is_rebuilding(&self) -> bool {
        self.pending.is_some()
    }

    /// 最近一次重放从第几个事件开始。
    pub fn rebuilt_from(&self) -> Option<usize> {
        self.rebuilt_from
    }
}

#[cfg(test)]
mod tests;
