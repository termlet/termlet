//! 查看公钥：类型、指纹、authorized_keys 格式的一行（可复制），以及怎样使用的提示；
//! 下面是附带的 OpenSSH 证书（见 [`super::certificate`]）。导出公钥、附带证书的结果写在底部的提示行里
//! （[`crate::notify::show`]：弹出的通知会被对话框盖住）。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::id::KeyId;
use termlet_core::model::{KeyEntry, Kind};
use termlet_core::store::StoreEvent;

use crate::app_state::AppState;
use crate::notify::NoticeLine;
use crate::prose::prose;
use crate::t;
use crate::ui_scale::DialogTitleExt as _;

pub fn open(id: KeyId, window: &mut Window, cx: &mut App) {
    let Some(record) = AppState::store(cx).get::<KeyEntry>(&id) else {
        return;
    };
    let key = record.doc;
    let view = cx.new(|cx| PublicKeyView::new(id, &key, window, cx));
    let title: SharedString = key.name.clone().into();
    let width = rems(36.).to_pixels(window.rem_size());
    let notice = view.read(cx).notice.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.titled(title.clone()).w(width).child(view.clone())
    });
    crate::notify::attach(&notice, window, cx);
}

struct PublicKeyView {
    id: KeyId,
    algorithm: SharedString,
    fingerprint: SharedString,
    public_key: String,
    /// 附带的证书（附带、替换、去掉后随存储更新）
    certificate: String,
    text: Entity<TextareaState>,
    copied: bool,
    /// 导出公钥、附带证书等的结果
    notice: Entity<NoticeLine>,
    _subscription: Subscription,
}

impl PublicKeyView {
    fn new(id: KeyId, key: &KeyEntry, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let public_key = key.public_key.trim().to_string();
        let text = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(4)
                .default_value(public_key.clone())
        });
        let store = AppState::global(cx).store.clone();
        let subscription = cx.subscribe(&store, |this: &mut Self, _, event: &StoreEvent, cx| {
            if matches!(event, StoreEvent::Changed(Kind::Key))
                && let Some(record) = AppState::store(cx).get::<KeyEntry>(&this.id)
            {
                this.certificate = record.doc.certificate;
                cx.notify();
            }
        });
        Self {
            id,
            algorithm: key.algorithm.clone().into(),
            fingerprint: key.fingerprint.clone().into(),
            public_key,
            certificate: key.certificate.clone(),
            text,
            copied: false,
            notice: NoticeLine::new(cx),
            _subscription: subscription,
        }
    }
}

impl Render for PublicKeyView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mono = theme.mono_font_family.clone();
        let line = |label: SharedString, value: SharedString, is_mono: bool| {
            h_flex()
                .gap_2()
                .items_start()
                .text_sm()
                .child(div().flex_none().w(rems(4.)).text_color(muted).child(label))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(is_mono, |this| {
                            this.font_family(theme.mono_font_family.clone())
                        })
                        .child(value),
                )
        };
        v_flex()
            .gap_3()
            .child(line(t!("keys.type"), self.algorithm.clone(), false))
            .child(line(t!("keys.fingerprint"), self.fingerprint.clone(), true))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(t!("keys.public_key")),
                    )
                    .child(Textarea::new(&self.text).readonly(true).font_family(mono))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!("keys.public_key_hint"))),
                    ),
            )
            .child(
                div()
                    .pt_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(super::certificate::section(&self.id, &self.certificate, cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .child(self.notice.clone()),
                    )
                    .child(
                        Button::new("key-copy-public")
                            .outline()
                            .icon(IconName::Copy)
                            .label(if self.copied {
                                t!("keys.copied")
                            } else {
                                t!("keys.copy")
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    this.public_key.clone(),
                                ));
                                this.copied = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("key-export-public")
                            .outline()
                            .icon(IconName::FileDown)
                            .label(t!("keys.export_public"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                super::export::export_public(this.id.clone(), window, cx)
                            })),
                    )
                    .child(
                        Button::new("key-view-close")
                            .primary()
                            .label(t!("common.close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    }
}
