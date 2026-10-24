//! Keychain: FIDO2 security keys. "New security key" (the key is generated
//! inside the device), "Load from security key" (resident keys) and the
//! details of any key (for security keys: application, flags, device and
//! the `authorized_keys` line with the options its flags need).

use std::sync::Arc;

use gpui::{
    AppContext, ClipboardItem, Entity, IntoElement, ParentElement, SharedString, Styled, Window,
    div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::select::Select;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_core::model::{SecretUpdate, SecurityKeyInfo, SshKey, SshKeySecret, SyncMode};
use termoak_ssh::keys::KeyMaterial;
use termoak_ssh::sk::{self, SkAlgorithm, SkError, SkKeyOptions};

use crate::runtime;
use crate::security_keys::{SecurityKeys, error_text};
use crate::state::{AppModel, Item, ToastKind};
use crate::ui::{self, Choice, IconName};

/// Options of the "New security key" form.
#[derive(Clone, Copy, Default)]
struct NewFlags {
    resident: bool,
    verify: bool,
    no_touch: bool,
    device_only: bool,
    advanced: bool,
}

/// Checks the advanced fields like the core does (before touching the key).
pub fn check_options(application: &str, user: &str) -> Result<(), SharedString> {
    let app = application.trim();
    if !app.is_empty() && (!app.starts_with("ssh:") || app.len() > 128) {
        return Err(t!("keychain.sk.application_invalid"));
    }
    if user.len() > 32 {
        return Err(t!("keychain.sk.user_too_long"));
    }
    Ok(())
}

/// The security keys of this computer, or an error shown.
fn service(
    model: &Entity<AppModel>,
    window: &mut Window,
    cx: &mut gpui::App,
) -> Option<Arc<SecurityKeys>> {
    let s = model.read(cx).security_keys.clone();
    if s.is_none() {
        ui::error(window, cx, t!("keychain.sk.unavailable"));
    }
    s
}

fn sk_error(e: SkError) -> String {
    error_text(e.code.api_code(), &e.message)
}

/// "New security key": the form, then the device makes the key (touch,
/// maybe the PIN), then it is saved and its details shown.
pub fn open_new_security_key(model: Entity<AppModel>, window: &mut Window, cx: &mut gpui::App) {
    if service(&model, window, cx).is_none() {
        return;
    }
    let host = termoak_client::api::device_name();
    let label =
        cx.new(|cx| InputState::new(window, cx).placeholder(t!("keychain.sk.name_placeholder")));
    let comment = cx.new(|cx| InputState::new(window, cx).default_value(format!("termoak@{host}")));
    let application = cx.new(|cx| InputState::new(window, cx).placeholder(sk::DEFAULT_APPLICATION));
    let user = cx.new(|cx| InputState::new(window, cx).placeholder(sk::DEFAULT_USER));
    let device =
        cx.new(|cx| InputState::new(window, cx).placeholder(t!("keychain.sk.device_placeholder")));
    let kind = ui::choice_state(
        vec![
            Choice::new(t!("keychain.sk.type_ed25519"), SkAlgorithm::Ed25519),
            Choice::new(t!("keychain.sk.type_ecdsa"), SkAlgorithm::EcdsaP256),
        ],
        Some(&SkAlgorithm::Ed25519),
        window,
        cx,
    );
    ui::focus_later(&label, window, cx);
    let flags = cx.new(|_| NewFlags::default());
    let (l2, c2, a2, u2, d2, k2, f2) = (
        label.clone(),
        comment.clone(),
        application.clone(),
        user.clone(),
        device.clone(),
        kind.clone(),
        flags.clone(),
    );
    ui::open_form_dialog(
        window,
        cx,
        t!("keychain.sk.new_title"),
        t!("keychain.sk.create"),
        520.,
        move |_, cx| {
            let f = *flags.read(cx);
            let set = |flags: &Entity<NewFlags>, apply: fn(&mut NewFlags, bool)| {
                let flags = flags.clone();
                move |v: &bool, _: &mut Window, cx: &mut gpui::App| {
                    flags.update(cx, |f, cx| {
                        apply(f, *v);
                        cx.notify();
                    })
                }
            };
            let theme = cx.theme();
            v_flex()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("keychain.sk.new_intro")),
                )
                .child(ui::field(t!("common.name"), Input::new(&label), cx))
                .child(ui::field(t!("keychain.type"), Select::new(&kind), cx))
                .child(ui::field(t!("keychain.comment"), Input::new(&comment), cx))
                .child(checkbox_with_hint(
                    "sk-resident",
                    t!("keychain.sk.resident_label"),
                    t!("keychain.sk.resident_hint"),
                    f.resident,
                    set(&flags, |f, v| f.resident = v),
                    cx,
                ))
                .child(
                    Checkbox::new("sk-verify")
                        .label(t!("keychain.sk.verify_label"))
                        .checked(f.verify)
                        .on_click(set(&flags, |f, v| f.verify = v)),
                )
                .child(checkbox_with_hint(
                    "sk-no-touch",
                    t!("keychain.sk.no_touch_label"),
                    t!("keychain.sk.no_touch_hint"),
                    f.no_touch,
                    set(&flags, |f, v| f.no_touch = v),
                    cx,
                ))
                .child(
                    Checkbox::new("sk-device-only")
                        .label(t!("keychain.device_only"))
                        .checked(f.device_only)
                        .on_click(set(&flags, |f, v| f.device_only = v)),
                )
                .child(
                    h_flex().child(
                        Button::new("sk-advanced")
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(if f.advanced {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            }))
                            .label(t!("keychain.sk.advanced"))
                            .on_click({
                                let flags = flags.clone();
                                move |_, _, cx| {
                                    flags.update(cx, |f, cx| {
                                        f.advanced = !f.advanced;
                                        cx.notify();
                                    })
                                }
                            }),
                    ),
                )
                .when(f.advanced, |this| {
                    this.child(ui::field_with_hint(
                        t!("keychain.sk.application"),
                        Input::new(&application),
                        t!("keychain.sk.application_hint"),
                        cx,
                    ))
                    .child(ui::field_with_hint(
                        t!("keychain.sk.user_label"),
                        Input::new(&user),
                        t!("keychain.sk.user_hint"),
                        cx,
                    ))
                    .child(ui::field(
                        t!("keychain.sk.device_label"),
                        Input::new(&device),
                        cx,
                    ))
                })
                .into_any_element()
        },
        move |window, cx| {
            let label = l2.read(cx).value().trim().to_string();
            if label.is_empty() {
                ui::error(window, cx, t!("keychain.key_name_required"));
                return false;
            }
            let application = a2.read(cx).value().trim().to_string();
            let user = u2.read(cx).value().trim().to_string();
            if let Err(e) = check_options(&application, &user) {
                ui::error(window, cx, e);
                return false;
            }
            let f = *f2.read(cx);
            let device_label = d2.read(cx).value().trim().to_string();
            let options = SkKeyOptions {
                algorithm: ui::chosen(&k2, cx).unwrap_or(SkAlgorithm::Ed25519),
                application: Some(application).filter(|a| !a.is_empty()),
                user: Some(user).filter(|u| !u.is_empty()),
                resident: f.resident,
                verify_required: f.verify,
                no_touch_required: f.no_touch,
                device: Some(device_label).filter(|d| !d.is_empty()),
            };
            let comment = c2.read(cx).value().trim().to_string();
            create(
                model.clone(),
                label,
                options,
                comment,
                f.device_only,
                window,
                cx,
            );
            true
        },
    );
}

fn checkbox_with_hint(
    id: &'static str,
    label: SharedString,
    hint: SharedString,
    checked: bool,
    on_click: impl Fn(&bool, &mut Window, &mut gpui::App) + 'static,
    cx: &gpui::App,
) -> impl IntoElement {
    v_flex()
        .gap_0p5()
        .child(
            Checkbox::new(id)
                .label(label)
                .checked(checked)
                .on_click(on_click),
        )
        .child(
            div()
                .pl_6()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(hint),
        )
}

/// Makes the key on the device and saves it.
fn create(
    model: Entity<AppModel>,
    label: String,
    options: SkKeyOptions,
    comment: String,
    device_only: bool,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let Some(keys) = service(&model, window, cx) else {
        return;
    };
    let task = runtime::spawn(cx, async move {
        let req = sk::make_credential_request(&options).map_err(sk_error)?;
        let (cred, device) = keys.make_credential(&req).await.map_err(sk_error)?;
        let mut m = sk::key_from_credential(&req, &cred, &comment).map_err(sk_error)?;
        if let Some(info) = m.security_key.as_mut()
            && info.device.is_none()
        {
            info.device = device;
        }
        Ok::<_, String>(m)
    });
    window
        .spawn(cx, async move |cx| {
            let res = task.await;
            let _ = cx.update(|window, cx| match res {
                Ok(material) => save_security_key(&model, label, material, device_only, window, cx),
                Err(e) => ui::error(window, cx, t!("keychain.sk.failed", error = e)),
            });
        })
        .detach();
}

/// The key record of a security key's material.
pub fn key_of(label: String, material: &KeyMaterial) -> (SshKey, SshKeySecret) {
    (
        SshKey {
            id: Id::nil(),
            label,
            algorithm: material.algorithm.clone(),
            public_key: material.public_openssh.clone(),
            fingerprint: material.fingerprint.clone(),
            comment: material.comment.clone(),
            has_passphrase: material.encrypted,
            certificate: None,
            security_key: material.security_key.clone(),
        },
        SshKeySecret {
            private_key: Some(material.private_openssh.clone()),
            passphrase: None,
        },
    )
}

/// Saves a new security key and shows its details (the authorized_keys
/// line to copy).
fn save_security_key(
    model: &Entity<AppModel>,
    label: String,
    material: KeyMaterial,
    device_only: bool,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let (key, secret) = key_of(label, &material);
    let mode = if device_only {
        SyncMode::DeviceOnly
    } else {
        SyncMode::Synced
    };
    let task = model.update(cx, |m, cx| {
        m.save(key, SecretUpdate::Set(secret), Some(mode), cx)
    });
    let model = model.clone();
    window
        .spawn(cx, async move |cx| {
            let res = task.await;
            let _ = cx.update(|window, cx| match res {
                Ok(r) => {
                    ui::success(window, cx, t!("keychain.sk.created", name = r.data.label));
                    open_key_details(model, r, true, window, cx);
                }
                Err(e) => ui::error(window, cx, t!("keychain.save_failed", error = e)),
            });
        })
        .detach();
}

/// "Load from security key": the resident keys of the device that are not
/// here yet.
pub fn open_load_resident(model: Entity<AppModel>, window: &mut Window, cx: &mut gpui::App) {
    let Some(keys) = service(&model, window, cx) else {
        return;
    };
    if !keys.can_list_resident() {
        ui::notify(
            window,
            cx,
            ToastKind::Warning,
            t!("keychain.sk.load_unsupported"),
        );
        return;
    }
    let device =
        cx.new(|cx| InputState::new(window, cx).placeholder(t!("keychain.sk.device_placeholder")));
    let verify = cx.new(|_| false);
    let (d2, v2) = (device.clone(), verify.clone());
    ui::open_form_dialog(
        window,
        cx,
        t!("keychain.sk.load_title"),
        t!("keychain.sk.load"),
        500.,
        move |_, cx| {
            let checked = *verify.read(cx);
            let v = verify.clone();
            v_flex()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("keychain.sk.load_intro")),
                )
                .child(checkbox_with_hint(
                    "sk-load-verify",
                    t!("keychain.sk.load_verify"),
                    t!("keychain.sk.load_verify_hint"),
                    checked,
                    move |on, _, cx| {
                        v.update(cx, |v, cx| {
                            *v = *on;
                            cx.notify();
                        })
                    },
                    cx,
                ))
                .child(ui::field(
                    t!("keychain.sk.device_label"),
                    Input::new(&device),
                    cx,
                ))
                .into_any_element()
        },
        move |window, cx| {
            let verify = *v2.read(cx);
            let label = d2.read(cx).value().trim().to_string();
            load_resident(model.clone(), keys.clone(), verify, label, window, cx);
            true
        },
    );
}

fn load_resident(
    model: Entity<AppModel>,
    keys: Arc<SecurityKeys>,
    verify: bool,
    device_label: String,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let known: Vec<String> = model
        .read(cx)
        .keys
        .iter()
        .map(|k| k.data.fingerprint.clone())
        .collect();
    let task = runtime::spawn(cx, async move {
        let (list, device) = keys
            .list_resident(sk::DEFAULT_APPLICATION)
            .await
            .map_err(sk_error)?;
        let device = Some(device_label).filter(|d| !d.is_empty()).or(device);
        let mut out = Vec::new();
        for c in &list {
            let m = sk::key_from_resident(c, verify, device.clone(), "").map_err(sk_error)?;
            if known.contains(&m.fingerprint)
                || out
                    .iter()
                    .any(|(_, x): &(String, KeyMaterial)| x.fingerprint == m.fingerprint)
            {
                continue;
            }
            out.push((
                resident_label(device.as_deref(), m.security_key.as_ref()),
                m,
            ));
        }
        Ok::<_, String>(out)
    });
    window
        .spawn(cx, async move |cx| {
            let res = task.await;
            let _ = cx.update(|window, cx| match res {
                Ok(found) if found.is_empty() => {
                    ui::notify(window, cx, ToastKind::Info, t!("keychain.sk.loaded_none"))
                }
                Ok(found) => {
                    let n = found.len();
                    let tasks: Vec<_> = found
                        .into_iter()
                        .map(|(label, m)| {
                            let (key, secret) = key_of(label, &m);
                            model.update(cx, |md, cx| {
                                md.save(key, SecretUpdate::Set(secret), Some(SyncMode::Synced), cx)
                            })
                        })
                        .collect();
                    window
                        .spawn(cx, async move |cx| {
                            let mut errors = Vec::new();
                            for t in tasks {
                                if let Err(e) = t.await {
                                    errors.push(e);
                                }
                            }
                            let _ = cx.update(|window, cx| {
                                if let Some(e) = errors.first() {
                                    ui::error(window, cx, t!("keychain.save_failed", error = e));
                                } else {
                                    ui::success(window, cx, tn!("keychain.sk.loaded", n));
                                }
                            });
                        })
                        .detach();
                }
                Err(e) => ui::error(window, cx, t!("keychain.sk.load_failed", error = e)),
            });
        })
        .detach();
}

/// "YubiKey 5C (alice)": the device and the user stored with the key.
pub fn resident_label(device: Option<&str>, info: Option<&SecurityKeyInfo>) -> String {
    let base = device
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| t!("keychain.sk.badge").to_string());
    match info
        .and_then(|i| i.user.as_deref())
        .filter(|u| !u.is_empty())
    {
        Some(u) => format!("{base} ({u})"),
        None => base,
    }
}

/// The details of a key: everything about it and, for security keys, the
/// device's flags and the `authorized_keys` line. `fresh`: just created
/// (the text says what to do next).
pub fn open_key_details(
    model: Entity<AppModel>,
    rec: Item<SshKey>,
    fresh: bool,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let caps = model.read(cx).caps_of(&rec);
    let k = rec.data.clone();
    let info = k.security_key.clone().or_else(|| {
        termoak_ssh::keys::parse_public(&k.public_key)
            .ok()
            .and_then(|p| sk::public_info(&p))
    });
    let device = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t!("keychain.sk.device_placeholder"))
            .default_value(
                info.as_ref()
                    .and_then(|i| i.device.clone())
                    .unwrap_or_default(),
            )
    });
    let d2 = device.clone();
    let editable = caps.edit && info.is_some();
    let id = k.id;
    let original_device = info
        .as_ref()
        .and_then(|i| i.device.clone())
        .unwrap_or_default();
    ui::open_form_dialog(
        window,
        cx,
        t!("keychain.details_title", name = k.label),
        if editable {
            t!("common.save")
        } else {
            t!("common.close")
        },
        600.,
        move |_, cx| details_body(&k, info.as_ref(), editable.then_some(&device), fresh, cx),
        move |window, cx| {
            if !editable {
                return true;
            }
            let label = d2.read(cx).value().trim().to_string();
            if label == original_device {
                return true;
            }
            let task = model.update(cx, |m, cx| {
                m.update_item::<SshKey>(
                    id,
                    move |k| {
                        if let Some(i) = k.security_key.as_mut() {
                            i.device = Some(label.clone()).filter(|l| !l.is_empty());
                        } else if let Some(mut i) = termoak_ssh::keys::parse_public(&k.public_key)
                            .ok()
                            .and_then(|p| sk::public_info(&p))
                        {
                            i.device = Some(label.clone()).filter(|l| !l.is_empty());
                            k.security_key = Some(i);
                        }
                    },
                    cx,
                )
            });
            window
                .spawn(cx, async move |cx| {
                    if let Err(e) = task.await {
                        let _ = cx.update(|window, cx| ui::error(window, cx, e));
                    }
                })
                .detach();
            true
        },
    );
}

fn yes_no(on: bool) -> SharedString {
    if on {
        t!("common.yes")
    } else {
        t!("common.no")
    }
}

fn detail_row(label: SharedString, value: impl IntoElement, cx: &gpui::App) -> impl IntoElement {
    h_flex()
        .gap_3()
        .items_start()
        .child(
            div()
                .w(px(150.))
                .flex_shrink_0()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(div().flex_1().min_w_0().text_sm().child(value))
}

/// A block of monospace text with a copy button.
fn copy_block(
    id: &'static str,
    text: String,
    copied: SharedString,
    cx: &gpui::App,
) -> impl IntoElement {
    let theme = cx.theme();
    let t2 = text.clone();
    h_flex()
        .gap_2()
        .items_start()
        .p_2()
        .rounded(theme.radius)
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .font_family(ui::mono_family(cx))
                .text_xs()
                .child(text),
        )
        .child(
            Button::new(id)
                .xsmall()
                .ghost()
                .icon(ui::icon(IconName::Copy))
                .tooltip(t!("common.copy"))
                .on_click(move |_, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(t2.clone()));
                    ui::success(window, cx, copied.clone());
                }),
        )
}

fn details_body(
    k: &SshKey,
    info: Option<&SecurityKeyInfo>,
    device: Option<&Entity<InputState>>,
    fresh: bool,
    cx: &gpui::App,
) -> gpui::AnyElement {
    let theme = cx.theme();
    let mut body = v_flex()
        .gap_2()
        .child(detail_row(
            t!("keychain.details.algorithm"),
            k.algorithm.clone(),
            cx,
        ))
        .child(detail_row(
            t!("keychain.details.fingerprint"),
            div()
                .font_family(ui::mono_family(cx))
                .text_xs()
                .child(k.fingerprint.clone()),
            cx,
        ))
        .when(!k.comment.is_empty(), |this| {
            this.child(detail_row(t!("keychain.comment"), k.comment.clone(), cx))
        });
    let Some(info) = info else {
        return body
            .child(
                div()
                    .text_xs()
                    .font_medium()
                    .child(t!("keychain.details.public_key")),
            )
            .child(copy_block(
                "copy-public-details",
                k.public_key.clone(),
                t!("keychain.public_copied"),
                cx,
            ))
            .into_any_element();
    };
    body = body
        .child(
            h_flex()
                .pt_2()
                .gap_2()
                .items_center()
                .child(
                    ui::icon(IconName::Usb)
                        .size(px(15.))
                        .text_color(theme.primary),
                )
                .child(
                    div()
                        .font_semibold()
                        .text_sm()
                        .child(t!("keychain.sk.badge")),
                ),
        )
        .child(detail_row(
            t!("keychain.sk.application"),
            info.application.clone(),
            cx,
        ))
        .child(detail_row(
            t!("keychain.sk.touch"),
            if info.touch_required {
                t!("keychain.sk.touch_required")
            } else {
                t!("keychain.sk.touch_not_required")
            },
            cx,
        ))
        .child(detail_row(
            t!("keychain.sk.pin"),
            if info.verify_required {
                t!("keychain.sk.pin_required")
            } else {
                t!("keychain.sk.pin_not_required")
            },
            cx,
        ))
        .child(detail_row(
            t!("keychain.sk.resident"),
            yes_no(info.resident),
            cx,
        ))
        .when_some(info.user.clone().filter(|u| !u.is_empty()), |this, u| {
            this.child(detail_row(t!("keychain.sk.user_label"), u, cx))
        });
    body = match device {
        Some(input) => body.child(detail_row(
            t!("keychain.sk.device_label"),
            Input::new(input).small(),
            cx,
        )),
        None => body.child(detail_row(
            t!("keychain.sk.device_label"),
            info.device.clone().unwrap_or_else(|| "—".into()),
            cx,
        )),
    };
    if !info.touch_required {
        body = body.child(
            div()
                .text_xs()
                .text_color(theme.warning)
                .child(t!("keychain.sk.no_touch_warning")),
        );
    }
    let line = sk::authorized_keys_line(&k.public_key, Some(info));
    body.child(
        div()
            .pt_2()
            .text_xs()
            .font_medium()
            .child(t!("keychain.sk.authorized_keys")),
    )
    .child(copy_block(
        "copy-authorized-line",
        line,
        t!("keychain.sk.line_copied"),
        cx,
    ))
    .child(
        div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(if fresh {
                t!("keychain.sk.authorized_hint_new")
            } else {
                t!("keychain.sk.authorized_hint")
            }),
    )
    .child(
        div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(t!("keychain.sk.handle_note")),
    )
    .into_any_element()
}

/// Whether a stored key is a security key (with or without details).
pub fn is_security_key(k: &SshKey) -> bool {
    k.security_key.is_some() || k.algorithm.starts_with("sk-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advanced_options_are_checked_like_the_core() {
        assert!(check_options("", "").is_ok());
        assert!(check_options("ssh:work", "alice").is_ok());
        assert!(check_options("https://x", "").is_err());
        assert!(check_options(&format!("ssh:{}", "a".repeat(130)), "").is_err());
        assert!(check_options("ssh:", &"u".repeat(33)).is_err());
    }

    #[test]
    fn resident_keys_are_named_after_device_and_user() {
        let info = SecurityKeyInfo {
            application: "ssh:".into(),
            user: Some("alice".into()),
            touch_required: true,
            verify_required: false,
            resident: true,
            device: None,
        };
        assert_eq!(
            resident_label(Some("YubiKey 5C"), Some(&info)),
            "YubiKey 5C (alice)"
        );
        assert_eq!(
            resident_label(None, None),
            t!("keychain.sk.badge").to_string()
        );
    }

    #[test]
    fn security_keys_are_recognized_without_details() {
        let mut k = SshKey {
            id: Id::nil(),
            label: "old".into(),
            algorithm: "sk-ssh-ed25519@openssh.com".into(),
            public_key: String::new(),
            fingerprint: String::new(),
            comment: String::new(),
            has_passphrase: false,
            certificate: None,
            security_key: None,
        };
        assert!(is_security_key(&k));
        k.algorithm = "ssh-ed25519".into();
        assert!(!is_security_key(&k));
    }
}
