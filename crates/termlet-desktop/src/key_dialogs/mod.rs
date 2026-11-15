//! 密钥库的对话框：查看公钥（及附带的证书）、导入、生成、导出私钥、部署公钥、重命名、删除。

pub(crate) mod certificate;
pub(crate) mod deploy;
pub(crate) mod errors;
pub(crate) mod export;
pub(crate) mod generate;
pub(crate) mod import;
pub(crate) mod view;

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::{App, SharedString, Task, Window};
use termlet_core::id::KeyId;
use termlet_core::model::KeyEntry;

use crate::app_state::AppState;
use crate::name_dialog::{self, NameDialog};
use crate::prose::prose;
use crate::{store_errors, t};

/// 重命名密钥。
pub fn rename(id: KeyId, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    let Some(record) = store.get::<KeyEntry>(&id) else {
        return;
    };
    name_dialog::open(
        NameDialog {
            title: t!("keys.rename_title"),
            value: record.doc.name.clone(),
            placeholder: SharedString::default(),
            select_all: true,
            ok_label: t!("common.ok"),
            input_id: "key-rename",
        },
        Box::new(move |name, _, _| {
            let Some(mut record) = store.get::<KeyEntry>(&id) else {
                return Task::ready(Ok(()));
            };
            record.doc.name = name;
            Task::ready(
                store
                    .update(&id, record.doc)
                    .map_err(|e| store_errors::describe(&e)),
            )
        }),
        window,
        cx,
    );
}

/// 删除密钥（还有主机或凭据档案在用时不能删）。
pub fn confirm_delete(id: KeyId, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    let Some(record) = store.get::<KeyEntry>(&id) else {
        return;
    };
    let name = record.doc.name;
    crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
        let (store, id) = (store.clone(), id.clone());
        alert
            .confirm()
            .title(t!("keys.delete_title", name = name.clone()))
            .description(prose(t!("keys.delete_description")))
            .ok_text(t!("common.delete"))
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("common.cancel"))
            .on_ok(move |_, window, cx| {
                tracing::info!(id = %id, "删除密钥");
                if let Err(e) = store.delete::<KeyEntry>(&id) {
                    crate::notify::error(store_errors::describe(&e), window, cx);
                }
                true
            })
    });
}
