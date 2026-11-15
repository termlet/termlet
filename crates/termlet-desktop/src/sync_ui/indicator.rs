//! 标题栏的同步图标：没有设置同步时不显示；同步中转圈，出错时带红点，有等待决定的事时带黄点，其余时候是云的图标。
//! 悬停显示状态，点击发出 [`OpenSyncSettings`]（由工作区打开设置的“同步”页）。

use gpui_kit::assets::IconName;
use gpui_kit::component::badge::Badge;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _};
use gpui_kit::{
    Context, Entity, EventEmitter, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div,
};
use termlet_core::cloudsync::SyncStatus;

use super::{SyncModel, describe, model, service_of};
use crate::t;

/// 点击了同步图标：打开设置的“同步”页。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OpenSyncSettings;

/// 标题栏的同步图标（与标题栏其他图标按钮相同：ghost、small 的图标按钮）。
pub(crate) struct SyncIndicator {
    model: Entity<SyncModel>,
    _observe: Subscription,
}

impl EventEmitter<OpenSyncSettings> for SyncIndicator {}

impl SyncIndicator {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let model = model(cx);
        let observe = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            _observe: observe,
        }
    }
}

/// 图标显示的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Look {
    Idle,
    Syncing,
    /// 有等待用户决定的事（自动同步暂停）
    Pending,
    Failed,
}

impl Look {
    /// 没有设置同步时为 `None`（不显示）。同步中优先，其次是等待决定的事、错误。
    pub(super) fn of(status: &SyncStatus) -> Option<Self> {
        status.config.as_ref()?;
        Some(if status.syncing {
            Look::Syncing
        } else if status.pending.is_some() {
            Look::Pending
        } else if status.last_error.is_some() {
            Look::Failed
        } else {
            Look::Idle
        })
    }
}

fn tooltip(look: Look, status: &SyncStatus) -> SharedString {
    match look {
        Look::Syncing => t!("sync_ui.indicator.syncing"),
        Look::Pending => t!("sync_ui.indicator.pending"),
        Look::Failed => match &status.last_error {
            Some(error) => t!(
                "sync_ui.indicator.failed",
                error = describe(error, service_of(status))
            ),
            None => t!("sync_ui.indicator.never"),
        },
        Look::Idle => match status.last_sync {
            Some(at) => t!(
                "sync_ui.indicator.last_sync",
                time = termlet_core::time::format_local(at)
            ),
            None => t!("sync_ui.indicator.never"),
        },
    }
}

impl Render for SyncIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.model.read(cx).status();
        let Some(look) = Look::of(status) else {
            // 不占位置（标题栏中按钮之间的间距不变）
            return div().hidden().into_any_element();
        };
        let tooltip = tooltip(look, status);
        let button = Button::new("sync-indicator")
            .ghost()
            .small()
            .tooltip(tooltip)
            .on_click(cx.listener(|_, _, _, cx| cx.emit(OpenSyncSettings)));
        let button = if look == Look::Syncing {
            button.icon(Spinner::new().icon(Icon::new(IconName::RefreshCw)))
        } else {
            button.icon(IconName::Cloud)
        };
        let dot = match look {
            Look::Failed => Some(cx.theme().danger),
            Look::Pending => Some(cx.theme().warning),
            Look::Idle | Look::Syncing => None,
        };
        match dot {
            Some(color) => Badge::new()
                .dot()
                .color(color)
                .child(button)
                .into_any_element(),
            None => button.into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termlet_core::cloudsync::{Pending, SyncConfig, SyncError, SyncTarget};

    fn configured() -> SyncStatus {
        SyncStatus {
            config: Some(SyncConfig {
                target: SyncTarget::Folder {
                    path: "D:/Sync".into(),
                },
                auto: true,
                interval_minutes: 10,
                device_name: "pc".into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn hidden_until_sync_is_set_up() {
        assert_eq!(Look::of(&SyncStatus::default()), None);
        assert_eq!(Look::of(&configured()), Some(Look::Idle));
    }

    #[test]
    fn syncing_then_pending_then_error() {
        let mut status = configured();
        status.last_error = Some(SyncError::AuthFailed);
        assert_eq!(Look::of(&status), Some(Look::Failed));
        status.pending = Some(Pending::ConfirmDeletes {
            local: 6,
            remote: 0,
        });
        assert_eq!(Look::of(&status), Some(Look::Pending));
        status.syncing = true;
        assert_eq!(Look::of(&status), Some(Look::Syncing));
    }
}
