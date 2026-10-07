//! Keychain: SSH keys (generate, import, copy the public one) and identities
//! (reusable username + password and/or key).

use gpui::{
    AppContext, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, InteractiveElement,
    IntoElement, ParentElement, PathPromptOptions, Render, SharedString, Styled, Subscription,
    Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::Select;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_core::model::{Identity, IdentitySecret, SecretUpdate, SshKey, SshKeySecret, SyncMode};
use termoak_ssh::keys::{self, KeyType};

use super::OpenRequest;
use crate::runtime;
use crate::state::{AppModel, Item, ToastKind};
use crate::ui::{self, Choice, IconName};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Keys,
    Identities,
}

pub struct KeychainView {
    model: Entity<AppModel>,
    tab: Tab,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRequest> for KeychainView {}

/// Form options shared between the dialog and its button.
#[derive(Clone, Copy, Default)]
struct Flags {
    device_only: bool,
    save_passphrase: bool,
}

impl KeychainView {
    pub fn new(model: Entity<AppModel>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&model, |_, _, cx| cx.notify())];
        Self {
            model,
            tab: Tab::Keys,
            _subs: subs,
        }
    }

    // ----- Keys -----

    fn generate_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = termoak_client::api::device_name();
        let label = cx
            .new(|cx| InputState::new(window, cx).placeholder(t!("keychain.key_name_placeholder")));
        let comment =
            cx.new(|cx| InputState::new(window, cx).default_value(format!("termoak@{host}")));
        let passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("keychain.passphrase_optional"))
        });
        let kind = ui::choice_state(
            vec![
                Choice::new(t!("keychain.ed25519_recommended"), KeyType::Ed25519),
                Choice::new("RSA 4096", KeyType::Rsa4096),
                Choice::new("RSA 3072", KeyType::Rsa3072),
                Choice::new("RSA 2048", KeyType::Rsa2048),
                Choice::new("ECDSA P-256", KeyType::EcdsaP256),
                Choice::new("ECDSA P-384", KeyType::EcdsaP384),
                Choice::new("ECDSA P-521", KeyType::EcdsaP521),
            ],
            Some(&KeyType::Ed25519),
            window,
            cx,
        );
        ui::focus_later(&label, window, cx);
        let flags = cx.new(|_| Flags::default());
        let model = self.model.clone();
        let (l2, c2, p2, k2, f2) = (
            label.clone(),
            comment.clone(),
            passphrase.clone(),
            kind.clone(),
            flags.clone(),
        );
        ui::open_form_dialog(
            window,
            cx,
            t!("keychain.generate_title"),
            t!("keychain.generate"),
            480.,
            move |_, cx| {
                let f = *flags.read(cx);
                let (fa, fb) = (flags.clone(), flags.clone());
                v_flex()
                    .gap_3()
                    .child(ui::field(t!("common.name"), Input::new(&label), cx))
                    .child(ui::field(t!("keychain.type"), Select::new(&kind), cx))
                    .child(ui::field(t!("keychain.comment"), Input::new(&comment), cx))
                    .child(ui::field(
                        t!("keychain.passphrase"),
                        Input::new(&passphrase).mask_toggle(),
                        cx,
                    ))
                    .child(
                        Checkbox::new("save-pass")
                            .label(t!("keychain.save_passphrase_long"))
                            .checked(f.save_passphrase)
                            .on_click(move |v, _, cx| {
                                fa.update(cx, |f, cx| {
                                    f.save_passphrase = *v;
                                    cx.notify();
                                })
                            }),
                    )
                    .child(
                        Checkbox::new("device-only")
                            .label(t!("keychain.device_only"))
                            .checked(f.device_only)
                            .on_click(move |v, _, cx| {
                                fb.update(cx, |f, cx| {
                                    f.device_only = *v;
                                    cx.notify();
                                })
                            }),
                    )
                    .into_any_element()
            },
            move |window, cx| {
                let label = l2.read(cx).value().trim().to_string();
                if label.is_empty() {
                    ui::error(window, cx, t!("keychain.key_name_required"));
                    return false;
                }
                let comment = c2.read(cx).value().trim().to_string();
                let pass = p2.read(cx).value().to_string();
                let kind = ui::chosen(&k2, cx).unwrap_or(KeyType::Ed25519);
                let flags = *f2.read(cx);
                let model = model.clone();
                ui::notify(window, cx, ToastKind::Info, t!("keychain.generating"));
                let task = runtime::spawn(cx, async move {
                    tokio::task::spawn_blocking(move || {
                        keys::generate(
                            kind,
                            &comment,
                            Some(pass.as_str()).filter(|p| !p.is_empty()),
                        )
                        .map(|m| (m, pass))
                    })
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())
                });
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok((material, pass)) => {
                                save_material(&model, label, material, pass, flags, window, cx);
                            }
                            Err(e) => {
                                ui::error(window, cx, t!("keychain.generate_failed", error = e))
                            }
                        });
                    })
                    .detach();
                true
            },
        );
    }

    fn import_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let label = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("keychain.imported_name_placeholder"))
        });
        let pem = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(8, 14)
                .placeholder("-----BEGIN OPENSSH PRIVATE KEY-----\n…")
        });
        let passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("keychain.passphrase_if_encrypted"))
        });
        ui::focus_later(&label, window, cx);
        let flags = cx.new(|_| Flags::default());
        let model = self.model.clone();
        let (l2, pem2, p2, f2) = (
            label.clone(),
            pem.clone(),
            passphrase.clone(),
            flags.clone(),
        );
        ui::open_form_dialog(
            window,
            cx,
            t!("keychain.import_title"),
            t!("keychain.import"),
            560.,
            move |_, cx| {
                let f = *flags.read(cx);
                let (fa, fb) = (flags.clone(), flags.clone());
                let pem_pick = pem.clone();
                let label_pick = label.clone();
                v_flex()
                    .gap_3()
                    .child(ui::field(t!("common.name"), Input::new(&label), cx))
                    .child(ui::field_with_hint(
                        t!("keychain.private_key"),
                        Textarea::new(&pem),
                        t!("keychain.private_key_hint"),
                        cx,
                    ))
                    .child(
                        Button::new("pick-key-file")
                            .small()
                            .icon(ui::icon(IconName::FileKey))
                            .label(t!("keychain.choose_file"))
                            .on_click(move |_, window, cx| {
                                let rx = cx.prompt_for_paths(PathPromptOptions {
                                    files: true,
                                    directories: false,
                                    multiple: false,
                                    prompt: Some(t!("keychain.import")),
                                });
                                let pem_pick = pem_pick.clone();
                                let label_pick = label_pick.clone();
                                window
                                    .spawn(cx, async move |cx| {
                                        let Ok(Ok(Some(paths))) = rx.await else {
                                            return;
                                        };
                                        let Some(path) = paths.into_iter().next() else {
                                            return;
                                        };
                                        let content = std::fs::read_to_string(&path);
                                        let _ = cx.update(|window, cx| match content {
                                            Ok(text) => {
                                                pem_pick.update(cx, |i, cx| {
                                                    i.set_value(text, window, cx)
                                                });
                                                if label_pick.read(cx).value().is_empty()
                                                    && let Some(name) = path.file_name()
                                                {
                                                    let name = name.to_string_lossy().to_string();
                                                    label_pick.update(cx, |i, cx| {
                                                        i.set_value(name, window, cx)
                                                    });
                                                }
                                            }
                                            Err(e) => ui::error(
                                                window,
                                                cx,
                                                t!("keychain.read_file_failed", error = e),
                                            ),
                                        });
                                    })
                                    .detach();
                            }),
                    )
                    .child(ui::field(
                        t!("keychain.passphrase"),
                        Input::new(&passphrase).mask_toggle(),
                        cx,
                    ))
                    .child(
                        Checkbox::new("save-pass")
                            .label(t!("keychain.save_passphrase"))
                            .checked(f.save_passphrase)
                            .on_click(move |v, _, cx| {
                                fa.update(cx, |f, cx| {
                                    f.save_passphrase = *v;
                                    cx.notify();
                                })
                            }),
                    )
                    .child(
                        Checkbox::new("device-only")
                            .label(t!("keychain.device_only"))
                            .checked(f.device_only)
                            .on_click(move |v, _, cx| {
                                fb.update(cx, |f, cx| {
                                    f.device_only = *v;
                                    cx.notify();
                                })
                            }),
                    )
                    .into_any_element()
            },
            move |window, cx| {
                let label = l2.read(cx).value().trim().to_string();
                let pem = pem2.read(cx).value().to_string();
                if label.is_empty() || pem.trim().is_empty() {
                    ui::error(window, cx, t!("keychain.import_required"));
                    return false;
                }
                let pass = p2.read(cx).value().to_string();
                let flags = *f2.read(cx);
                match keys::import_private(&pem, Some(pass.as_str()).filter(|p| !p.is_empty())) {
                    Ok(material) => {
                        save_material(&model, label, material, pass, flags, window, cx);
                        true
                    }
                    Err(e) => {
                        ui::error(window, cx, t!("keychain.import_failed", error = e));
                        false
                    }
                }
            },
        );
    }

    fn rename_key(&mut self, rec: Item<SshKey>, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(rec.data.label.clone()));
        ui::focus_later(&input, window, cx);
        let model = self.model.clone();
        let i2 = input.clone();
        ui::open_form_dialog(
            window,
            cx,
            t!("keychain.rename_title"),
            t!("common.save"),
            420.,
            move |_, cx| ui::field(t!("common.name"), Input::new(&input), cx).into_any_element(),
            move |window, cx| {
                let label = i2.read(cx).value().trim().to_string();
                if label.is_empty() {
                    return false;
                }
                let id = rec.data.id;
                let task = model.update(cx, |m, cx| {
                    m.update_item::<SshKey>(id, move |k| k.label = label, cx)
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

    /// "Copy private key": after Touch ID / Windows Hello if the lock asks.
    fn copy_private_key(&mut self, rec: Item<SshKey>, window: &mut Window, cx: &mut Context<Self>) {
        let item = rec.item_ref();
        let model = self.model.clone();
        crate::app_lock::guard_secret(&self.model, window, cx, move |window, cx| {
            let ws = model.read(cx).ws.clone();
            let task = runtime::spawn(cx, async move {
                ws.item_secret::<SshKey>(item).await.map(|s| s.private_key)
            });
            window
                .spawn(cx, async move |cx| {
                    let res = task.await;
                    let _ = cx.update(|window, cx| match res {
                        Ok(Some(key)) => {
                            cx.write_to_clipboard(ClipboardItem::new_string(key));
                            ui::success(window, cx, t!("keychain.private_copied"));
                        }
                        Ok(None) => ui::error(window, cx, t!("keychain.no_private")),
                        Err(e) => ui::error(window, cx, e),
                    });
                })
                .detach();
        });
    }

    fn delete_key(&mut self, rec: Item<SshKey>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        ui::confirm(
            window,
            cx,
            t!("keychain.delete_key_title"),
            t!("keychain.delete_key_confirm", name = rec.data.label),
            t!("keychain.delete"),
            true,
            move |window, cx| {
                let task = model.update(cx, |m, cx| m.delete::<SshKey>(rec.data.id, cx));
                window
                    .spawn(cx, async move |cx| {
                        if let Err(e) = task.await {
                            let _ = cx.update(|window, cx| ui::error(window, cx, e));
                        }
                    })
                    .detach();
            },
        );
    }

    // ----- Identities -----

    fn edit_identity(
        &mut self,
        rec: Option<Item<Identity>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let data = rec.as_ref().map(|r| r.data.clone());
        let has_secret = rec.as_ref().is_some_and(|r| r.meta.has_secret);
        let label = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("keychain.identity_name_placeholder"))
                .default_value(data.as_ref().map(|d| d.label.clone()).unwrap_or_default())
        });
        let username = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("root")
                .default_value(
                    data.as_ref()
                        .map(|d| d.username.clone())
                        .unwrap_or_default(),
                )
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if has_secret {
                    t!("keychain.password_saved")
                } else {
                    t!("keychain.optional")
                })
        });
        // Where the identity is (or goes): its key must be of the same vault
        // or of This device; "this device only" is for This device items.
        let place = {
            let m = self.model.read(cx);
            match &rec {
                Some(r) => (r.scope, m.vault_of(r)),
                None => m.place_of_target(m.new_item_target()),
            }
        };
        let on_device = place.0 == termoak_client::Scope::Device;
        let mut key_items = vec![Choice::new(t!("keychain.no_key"), None)];
        {
            let m = self.model.read(cx);
            key_items.extend(
                m.keys
                    .iter()
                    .filter(|k| m.fits_place(k, place))
                    .map(|k| Choice::new(k.data.label.clone(), Some(k.data.id))),
            );
        }
        let key = ui::choice_state(
            key_items,
            Some(&data.as_ref().and_then(|d| d.key_id)),
            window,
            cx,
        );
        ui::focus_later(&label, window, cx);
        let flags = cx.new(|_| Flags {
            device_only: rec
                .as_ref()
                .is_some_and(|r| r.meta.sync_mode == SyncMode::DeviceOnly),
            save_passphrase: false,
        });
        let model = self.model.clone();
        let (l2, u2, p2, k2, f2) = (
            label.clone(),
            username.clone(),
            password.clone(),
            key.clone(),
            flags.clone(),
        );
        ui::open_form_dialog(
            window,
            cx,
            if data.is_some() {
                t!("keychain.edit_identity")
            } else {
                t!("keychain.new_identity")
            },
            t!("common.save"),
            460.,
            move |_, cx| {
                let f = *flags.read(cx);
                let fa = flags.clone();
                v_flex()
                    .gap_3()
                    .child(ui::field(t!("common.name"), Input::new(&label), cx))
                    .child(ui::field(
                        t!("keychain.username"),
                        Input::new(&username),
                        cx,
                    ))
                    .child(ui::field(
                        t!("keychain.password"),
                        Input::new(&password).mask_toggle(),
                        cx,
                    ))
                    .child(ui::field(t!("keychain.ssh_key"), Select::new(&key), cx))
                    .when(on_device, |this| {
                        this.child(
                            Checkbox::new("device-only")
                                .label(t!("keychain.device_only"))
                                .checked(f.device_only)
                                .on_click(move |v, _, cx| {
                                    fa.update(cx, |f, cx| {
                                        f.device_only = *v;
                                        cx.notify();
                                    })
                                }),
                        )
                    })
                    .into_any_element()
            },
            move |window, cx| {
                let label = l2.read(cx).value().trim().to_string();
                let username = u2.read(cx).value().trim().to_string();
                if label.is_empty() || username.is_empty() {
                    ui::error(window, cx, t!("keychain.identity_required"));
                    return false;
                }
                let password = p2.read(cx).value().to_string();
                let identity = Identity {
                    id: data.as_ref().map(|d| d.id).unwrap_or(Id::nil()),
                    label,
                    username,
                    key_id: ui::chosen(&k2, cx).flatten(),
                };
                let secret = if password.is_empty() {
                    SecretUpdate::Keep
                } else {
                    SecretUpdate::Set(IdentitySecret {
                        password: Some(password),
                    })
                };
                let mode = if on_device && f2.read(cx).device_only {
                    SyncMode::DeviceOnly
                } else {
                    SyncMode::Synced
                };
                let task = model.update(cx, |m, cx| m.save(identity, secret, Some(mode), cx));
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok(r) => ui::success(
                                window,
                                cx,
                                t!("keychain.identity_saved", name = r.data.label),
                            ),
                            Err(e) => ui::error(window, cx, e),
                        });
                    })
                    .detach();
                true
            },
        );
    }

    fn delete_identity(
        &mut self,
        rec: Item<Identity>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.model.clone();
        ui::confirm(
            window,
            cx,
            t!("keychain.delete_identity_title"),
            t!("keychain.delete_identity_confirm", name = rec.data.label),
            t!("keychain.delete"),
            true,
            move |window, cx| {
                let task = model.update(cx, |m, cx| m.delete::<Identity>(rec.data.id, cx));
                window
                    .spawn(cx, async move |cx| {
                        if let Err(e) = task.await {
                            let _ = cx.update(|window, cx| ui::error(window, cx, e));
                        }
                    })
                    .detach();
            },
        );
    }

    // ----- Rendering -----

    /// Vault chip, "Use only" and what can be done with an item.
    fn place<T: termoak_core::model::Entity>(
        &self,
        rec: &Item<T>,
        cx: &mut Context<Self>,
    ) -> (Option<gpui::AnyElement>, crate::accounts::Caps) {
        let m = self.model.read(cx);
        let chips =
            crate::accounts::show_vault_picker(m.vaults_in_view().len(), m.has_device_items());
        let chip = chips
            .then(|| m.vault_entry_of(rec).cloned())
            .flatten()
            .map(|v| super::vaults::chip(&v, cx).into_any_element());
        (chip, m.caps_of(rec))
    }

    fn move_button(&self, id: impl Into<gpui::ElementId>, item: termoak_client::ItemRef) -> Button {
        let model = self.model.clone();
        Button::new(id)
            .small()
            .ghost()
            .icon(ui::icon(IconName::ArrowRightLeft))
            .tooltip(t!("hosts.menu.move_to"))
            .on_click(move |_: &ClickEvent, window, cx| {
                super::transfer::open(
                    model.clone(),
                    vec![item],
                    termoak_core::transfer::TransferMode::Move,
                    None,
                    window,
                    cx,
                )
            })
    }

    fn render_keys(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let keys: Vec<Item<SshKey>> = {
            let m = self.model.read(cx);
            m.keys.iter().filter(|k| m.in_filter(k)).cloned().collect()
        };
        if keys.is_empty() {
            return ui::empty_state(
                IconName::KeyRound,
                t!("keychain.no_keys"),
                t!("keychain.no_keys_hint"),
                cx,
            )
            .into_any_element();
        }
        v_flex()
            .gap_2()
            .children(keys.into_iter().enumerate().map(|(i, rec)| {
                let (chip, caps) = self.place(&rec, cx);
                let theme = cx.theme();
                let k = rec.data.clone();
                let public = k.public_key.clone();
                let (r1, r2, r3) = (rec.clone(), rec.clone(), rec.clone());
                h_flex()
                    .id(("key", i))
                    .p_3()
                    .gap_3()
                    .items_center()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .child(
                        div()
                            .size(px(36.))
                            .rounded(theme.radius)
                            .bg(theme.accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                ui::icon(IconName::KeyRound)
                                    .size(px(18.))
                                    .text_color(theme.primary),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().font_semibold().text_sm().child(k.label.clone()))
                                    .child(ui::pill(k.algorithm.clone(), theme.primary))
                                    .when(k.has_passphrase, |this| {
                                        this.child(ui::pill(
                                            t!("keychain.encrypted"),
                                            theme.warning,
                                        ))
                                    })
                                    .when(rec.meta.sync_mode == SyncMode::DeviceOnly, |this| {
                                        this.child(ui::pill(
                                            t!("keychain.device_only_pill"),
                                            theme.muted_foreground,
                                        ))
                                    })
                                    .children(chip)
                                    .when(caps.use_only_badge, |this| {
                                        this.child(ui::pill(
                                            t!("vaults.use_only_badge"),
                                            theme.warning,
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(ui::mono_family(cx))
                                    .text_color(theme.muted_foreground)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(k.fingerprint.clone()),
                            ),
                    )
                    .child(
                        Button::new(("copy-pub", i))
                            .small()
                            .icon(ui::icon(IconName::Copy))
                            .label(t!("keychain.copy_public"))
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(public.clone()));
                                ui::notify(
                                    window,
                                    cx,
                                    ToastKind::Success,
                                    t!("keychain.public_copied"),
                                );
                            }),
                    )
                    .when(caps.reveal, |this| {
                        this.child(
                            Button::new(("copy-private", i))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::FileKey))
                                .tooltip(t!("keychain.copy_private"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.copy_private_key(r3.clone(), window, cx)
                                })),
                        )
                    })
                    .when(caps.move_out, |this| {
                        this.child(self.move_button(("move-key", i), rec.item_ref()))
                    })
                    .when(caps.edit, |this| {
                        this.child(
                            Button::new(("rename-key", i))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Pencil))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.rename_key(r1.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new(("delete-key", i))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Trash))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.delete_key(r2.clone(), window, cx)
                                })),
                        )
                    })
            }))
            .into_any_element()
    }

    fn render_identities(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let model = self.model.read(cx);
        let identities: Vec<Item<Identity>> = model
            .identities
            .iter()
            .filter(|i| model.in_filter(i))
            .cloned()
            .collect();
        let key_label = |id: Option<Id>| -> Option<String> {
            id.and_then(|id| model.keys.iter().find(|k| k.data.id == id))
                .map(|k| k.data.label.clone())
        };
        let rows: Vec<(Item<Identity>, Option<String>)> = identities
            .into_iter()
            .map(|r| {
                let kl = key_label(r.data.key_id);
                (r, kl)
            })
            .collect();
        if rows.is_empty() {
            return ui::empty_state(
                IconName::Users,
                t!("keychain.no_identities"),
                t!("keychain.no_identities_hint"),
                cx,
            )
            .into_any_element();
        }
        v_flex()
            .gap_2()
            .children(rows.into_iter().enumerate().map(|(i, (rec, key))| {
                let (chip, caps) = self.place(&rec, cx);
                let theme = cx.theme();
                let (r1, r2) = (rec.clone(), rec.clone());
                h_flex()
                    .id(("identity", i))
                    .p_3()
                    .gap_3()
                    .items_center()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .child(
                        ui::icon(IconName::CircleUser)
                            .size(px(28.))
                            .text_color(theme.primary),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .font_semibold()
                                            .text_sm()
                                            .child(rec.data.label.clone()),
                                    )
                                    .children(chip)
                                    .when(caps.use_only_badge, |this| {
                                        this.child(ui::pill(
                                            t!("vaults.use_only_badge"),
                                            theme.warning,
                                        ))
                                    }),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child({
                                let mut parts = vec![rec.data.username.clone()];
                                if rec.meta.has_secret {
                                    parts.push(t!("keychain.identity_has_password").to_string());
                                }
                                if let Some(k) = key {
                                    parts.push(t!("keychain.identity_key", name = k).to_string());
                                }
                                parts.join(" · ")
                            })),
                    )
                    .when(caps.move_out, |this| {
                        this.child(self.move_button(("move-identity", i), rec.item_ref()))
                    })
                    .when(caps.edit, |this| {
                        this.child(
                            Button::new(("edit-identity", i))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Pencil))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.edit_identity(Some(r1.clone()), window, cx)
                                })),
                        )
                        .child(
                            Button::new(("delete-identity", i))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Trash))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.delete_identity(r2.clone(), window, cx)
                                })),
                        )
                    })
            }))
            .into_any_element()
    }
}

impl Render for KeychainView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.tab;
        let (nkeys, nids) = {
            let m = self.model.read(cx);
            (m.keys.len(), m.identities.len())
        };
        let body = match tab {
            Tab::Keys => self.render_keys(cx),
            Tab::Identities => self.render_identities(cx),
        };
        let actions = match tab {
            Tab::Keys => h_flex()
                .gap_2()
                .child(
                    Button::new("import-key")
                        .icon(ui::icon(IconName::Upload))
                        .label(t!("keychain.import"))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.import_key(window, cx)
                        })),
                )
                .child(
                    Button::new("generate-key")
                        .primary()
                        .icon(ui::icon(IconName::Plus))
                        .label(t!("keychain.generate_key"))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.generate_key(window, cx)
                        })),
                ),
            Tab::Identities => h_flex().child(
                Button::new("new-identity")
                    .primary()
                    .icon(ui::icon(IconName::Plus))
                    .label(t!("keychain.new_identity"))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.edit_identity(None, window, cx)
                    })),
            ),
        };
        let tab_button = |id: &'static str, label: SharedString, t: Tab, cx: &mut Context<Self>| {
            Button::new(id)
                .small()
                .label(label)
                .map(|b| if tab == t { b.primary() } else { b.ghost() })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.tab = t;
                    cx.notify();
                }))
        };
        v_flex()
            .size_full()
            .child(ui::section_header(
                t!("keychain.title"),
                t!("keychain.subtitle"),
                actions,
                cx,
            ))
            .child(
                h_flex()
                    .px_6()
                    .py_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(tab_button(
                        "tab-keys",
                        t!("keychain.tab_keys", count = nkeys),
                        Tab::Keys,
                        cx,
                    ))
                    .child(tab_button(
                        "tab-identities",
                        t!("keychain.tab_identities", count = nids),
                        Tab::Identities,
                        cx,
                    )),
            )
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(div().p_6().child(body)),
                ),
            )
    }
}

/// Saves a generated or imported key.
fn save_material(
    model: &Entity<AppModel>,
    label: String,
    material: keys::KeyMaterial,
    passphrase: String,
    flags: Flags,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let key = SshKey {
        id: Id::nil(),
        label,
        algorithm: material.algorithm.clone(),
        public_key: material.public_openssh.clone(),
        fingerprint: material.fingerprint.clone(),
        comment: material.comment.clone(),
        has_passphrase: material.encrypted,
        certificate: None,
    };
    let secret = SshKeySecret {
        private_key: Some(material.private_openssh.clone()),
        passphrase: (flags.save_passphrase && !passphrase.is_empty()).then_some(passphrase),
    };
    let mode = if flags.device_only {
        SyncMode::DeviceOnly
    } else {
        SyncMode::Synced
    };
    let task = model.update(cx, |m, cx| {
        m.save(key, SecretUpdate::Set(secret), Some(mode), cx)
    });
    window
        .spawn(cx, async move |cx| {
            let res = task.await;
            let _ = cx.update(|window, cx| match res {
                Ok(r) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(r.data.public_key.clone()));
                    ui::success(window, cx, t!("keychain.key_saved", name = r.data.label));
                }
                Err(e) => ui::error(window, cx, t!("keychain.save_failed", error = e)),
            });
        })
        .detach();
}
