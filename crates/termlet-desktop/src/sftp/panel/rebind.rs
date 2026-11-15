//! 文件管理的 Tab 移到别的窗口（见 `workspace/move_tab.rs`）：会话与它的传输交给新窗口（核心的
//! [`SftpManager::reassign_owner`]：原来的窗口关闭时不再关闭、删除它们，进行中的传输不中断，结束时的汇总在新窗口中
//! 提示）；路径框（输入框绑在打开它的窗口上，离开时恢复路径靠它的失去焦点事件）在新窗口中重建，以 root 身份打开时
//! sudo 密码的询问改在新窗口中弹出。别的订阅与任务按面板当前所在的窗口执行，画出后自动跟过去。
//!
//! 正在连接时不能移：进行中的连接可能正停在原来窗口的询问上。
//!
//! [`SftpManager::reassign_owner`]: termlet_core::sftp::SftpManager::reassign_owner

use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext as _, Context, Entity, SharedString, Subscription, Task, Window};
use termlet_core::secret::SecretString;
use termlet_core::ssh::Prompter;

use super::super::file_list::FileList;
use super::super::local_pane::LocalPane;
use super::super::sudo_dialog::{self, SudoRequest};
use super::super::transfers::TransferList;
use super::{SftpPanel, Status};
use crate::t;

impl SftpPanel {
    /// 现在不能移到别的窗口的原因（正在连接）；能移时为 `None`。
    pub(crate) fn move_blocker(&self) -> Option<SharedString> {
        (self.status == Status::Connecting).then(|| t!("tab_window.blocked_connecting"))
    }

    /// 已从原来窗口的停靠区取下、放进 `window` 之前调用：会话与传输改属新窗口（`owner`），之后的连接在新窗口中询问
    /// （`prompter`），路径框重建。
    pub(crate) fn rebind_window(
        &mut self,
        owner: String,
        prompter: Prompter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(sftp = %self.name, from = %self.owner, to = %owner, "文件管理移到别的窗口");
        if let Some(session) = self.session() {
            self.manager.reassign_owner(session.id(), &owner);
        }
        self.owner = owner.clone();
        self.prompter = prompter;
        self.transfers
            .update(cx, |list, cx| list.set_owner(owner, cx));
        // 保留没提交的输入
        let text = self.path.read(cx).value();
        let path = cx.new(|cx| InputState::new(window, cx));
        path.update(cx, |input, cx| input.set_value(text, window, cx));
        self.path = path;
        self.local
            .update(cx, |local, cx| local.rebind_window(window, cx));
        self._subscriptions = Self::subscribe_children(
            &self.list,
            &self.local,
            &self.transfers,
            &self.path,
            window,
            cx,
        );
        if let Some((requests, saved)) = self.sudo_answers.clone() {
            self._sudo = Some(Self::answer_sudo(requests, saved, window, cx));
        }
        cx.notify();
    }

    /// 路径框（测试用）。
    #[cfg(test)]
    pub(crate) fn path_input(&self) -> Entity<InputState> {
        self.path.clone()
    }

    /// 面板对子元素的订阅：列表、本机栏、传输列表、路径框的事件，以及它们变了时重新渲染。
    pub(super) fn subscribe_children(
        list: &Entity<FileList>,
        local: &Entity<LocalPane>,
        transfers: &Entity<TransferList>,
        path: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        vec![
            cx.subscribe_in(list, window, Self::on_list_event),
            cx.subscribe_in(local, window, Self::on_local_event),
            cx.subscribe_in(transfers, window, Self::on_transfer_event),
            cx.subscribe_in(path, window, Self::on_path_event),
            // 本机栏换了目录（“下载”的提示）、传输列表有无记录（是否显示）时重新渲染
            cx.observe(local, |_, _, cx| cx.notify()),
            cx.observe(transfers, |_, _, cx| cx.notify()),
            // 选择变了：工具栏的按钮可用与否
            cx.observe(list, |_, _, cx| cx.notify()),
        ]
    }

    /// 以 root 身份打开时回答 sudo 的密码询问：在 `window` 中逐个弹出对话框（任务的窗口是登记时的窗口）。
    pub(super) fn answer_sudo(
        requests: async_channel::Receiver<SudoRequest>,
        saved: Option<SecretString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |_, cx| {
            sudo_dialog::answer_in_order(requests, saved, cx).await
        })
    }
}
