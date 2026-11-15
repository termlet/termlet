//! 侧边栏：顶部是页签（主机、快捷命令、密钥、转发），下面是当前页的内容。“密钥”页里再分密钥与凭据档案。
//! 宽度与显示由工作区管理；页签按当前语言的文案放不放得下见 [`Sidebar::tabs_overflow`]（[`crate::tab_fit`]）。

pub(crate) mod forwards;
pub(crate) mod host_tree;
pub(crate) mod keys;
pub(crate) mod profiles;
pub(crate) mod rows;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tab::TabBar;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Pixels, Render,
    ScrollHandle, Styled as _, Window, div,
};

use termlet_core::id::HostId;

use crate::snippets::list::SnippetList;
use crate::t;
use forwards::ForwardList;
use host_tree::HostTree;
use keys::KeyList;
use profiles::ProfileList;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Hosts,
    Snippets,
    Keys,
    Forwards,
}

/// 顶部页签的顺序。
const PAGES: [Page; 4] = [Page::Hosts, Page::Snippets, Page::Keys, Page::Forwards];

/// “密钥”页里的两个子页。
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeysTab {
    Keys,
    Profiles,
}

pub struct Sidebar {
    page: Page,
    keys_tab: KeysTab,
    hosts: Entity<HostTree>,
    snippets: Entity<SnippetList>,
    keys: Entity<KeyList>,
    profiles: Entity<ProfileList>,
    forwards: Entity<ForwardList>,
    /// 顶部页签的滚动状态：按它量页签实际排开的宽度
    tabs_scroll: ScrollHandle,
}

impl Sidebar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            page: Page::Hosts,
            keys_tab: KeysTab::Keys,
            hosts: cx.new(|cx| HostTree::new(window, cx)),
            snippets: cx.new(|cx| SnippetList::new(window, cx)),
            keys: cx.new(KeyList::new),
            profiles: cx.new(ProfileList::new),
            forwards: cx.new(|cx| ForwardList::new(window, cx)),
            tabs_scroll: ScrollHandle::new(),
        }
    }

    /// 顶部的页签按当前语言的文案排开，比它们的可见区域宽出多少（[`crate::tab_fit::overflow`]）；工作区据此放宽
    /// 侧边栏。
    pub fn tabs_overflow(&self) -> Option<Pixels> {
        crate::tab_fit::overflow(&self.tabs_scroll, PAGES.len())
    }

    pub fn host_tree(&self) -> &Entity<HostTree> {
        &self.hosts
    }

    pub fn snippet_list(&self) -> &Entity<SnippetList> {
        &self.snippets
    }

    /// 切到主机页，让主机或分组看得到（[`HostTree::reveal`]）。
    pub fn reveal(&mut self, key: rows::RowKey, cx: &mut Context<Self>) {
        self.page = Page::Hosts;
        self.hosts.update(cx, |tree, cx| tree.reveal(key, cx));
        cx.notify();
    }

    /// 切到主机页并显示这台主机（见 [`HostTree::show_host`]）。
    pub fn show_host(&mut self, host: HostId, window: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Hosts;
        self.hosts
            .update(cx, |tree, cx| tree.show_host(host, window, cx));
        cx.notify();
    }

    /// 切到某一页（状态栏的“N 个转发”、从终端添加转发后）。
    pub fn show_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        cx.notify();
    }

    fn render_keys_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, label, which: KeysTab, current: KeysTab| {
            let button = Button::new(id).small().label(label);
            if which == current {
                button.with_variant(gpui_kit::component::button::ButtonVariant::Secondary)
            } else {
                button.ghost()
            }
        };
        let current = self.keys_tab;
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .pt_1()
                    .gap_1()
                    .child(
                        tab("keys-tab-keys", t!("keys.tab_keys"), KeysTab::Keys, current).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.keys_tab = KeysTab::Keys;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        tab(
                            "keys-tab-profiles",
                            t!("sidebar.page.profiles"),
                            KeysTab::Profiles,
                            current,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.keys_tab = KeysTab::Profiles;
                            cx.notify();
                        })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(match current {
                KeysTab::Keys => self.keys.clone().into_any_element(),
                KeysTab::Profiles => self.profiles.clone().into_any_element(),
            }))
    }
}

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = PAGES.iter().position(|p| *p == self.page).unwrap_or(0);
        let this = cx.entity().downgrade();
        let content = match self.page {
            Page::Hosts => self.hosts.clone().into_any_element(),
            Page::Snippets => self.snippets.clone().into_any_element(),
            Page::Keys => self.render_keys_page(cx).into_any_element(),
            Page::Forwards => self.forwards.clone().into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .child(
                div().px_2().pt_2().child(
                    TabBar::new("sidebar-pages")
                        .segmented()
                        .track_scroll(&self.tabs_scroll)
                        .selected_index(selected)
                        .children([
                            t!("sidebar.page.hosts"),
                            t!("sidebar.page.snippets"),
                            t!("sidebar.page.keys"),
                            t!("sidebar.page.forwards"),
                        ])
                        .on_click(move |ix, _, cx| {
                            let _ = this.update(cx, |this, cx| {
                                this.page = PAGES[*ix];
                                cx.notify();
                            });
                        }),
                ),
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}
