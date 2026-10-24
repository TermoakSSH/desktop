//! Teams security in a terminal (0.7 phase 4c): a host that needs an
//! approved access window shows "Request access" (or the request waiting)
//! instead of an error, and connects by itself when the window opens; the
//! toolbar shows the window's countdown and "Recorded"; the tab shows the
//! time left.

use gpui::{
    AnyElement, ClickEvent, Context, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, StyledExt, WindowExt, h_flex, v_flex};
use termoak_core::Id;

use super::{TermKind, TermState, TerminalView};
use crate::access::{self, AccessBook, HostWindow};
use crate::ui::{self, IconName};

impl TerminalView {
    /// The account and host whose access applies to this terminal.
    pub(super) fn access_target(&self, cx: &gpui::App) -> Option<(Id, Id)> {
        let app = self.app.read(cx);
        match &self.kind {
            TermKind::Local { host_id } => app.account_of_host(*host_id).map(|a| (a, *host_id)),
            TermKind::Server {
                host_id: Some(h),
                account,
                link: None,
                ..
            } => account.or_else(|| app.account_of_host(*h)).map(|a| (a, *h)),
            _ => None,
        }
    }

    /// Follows the access state of the host (when its vault has access
    /// rules or records sessions).
    pub(super) fn watch_access(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((account, host)) = self.access_target(cx) else {
            return;
        };
        let (gates, api) = {
            let app = self.app.read(cx);
            (app.host_gates(host), app.api_of(Some(account)))
        };
        if !gates.access_rules && !gates.recorded {
            return;
        }
        let book = AccessBook::global(cx);
        if let Some(api) = api
            && gates.access_rules
        {
            book.update(cx, |b, cx| b.ensure_host(account, host, api, true, cx));
        }
        if self.access_sub.is_none() {
            self.access_sub = Some(cx.observe_in(&book, window, |this, _, window, cx| {
                this.on_access_book(window, cx)
            }));
        }
        if self.access_tick.is_none() {
            self.access_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    let every = this
                        .update(cx, |v, cx| {
                            let left = v.window_end(cx).map(|end| {
                                access::remaining_secs(end, termoak_core::time::now_ms())
                            });
                            if left.is_some() {
                                cx.notify();
                            }
                            access::tick_every(left.unwrap_or(i64::MAX))
                        })
                        .ok();
                    let Some(every) = every else {
                        break;
                    };
                    cx.background_executor().timer(every).await;
                }
            }));
        }
    }

    /// End of this host's open window (ms), if any.
    pub fn window_end(&self, cx: &gpui::App) -> Option<i64> {
        let (account, host) = self.access_target(cx)?;
        let book = AccessBook::try_global(cx)?;
        let a = book.read(cx).host(account, host)?;
        a.grant.as_ref().and_then(|g| g.ends_at)
    }

    /// The time left of the window, for the tab ("42 min").
    pub fn access_left(&self, cx: &gpui::App) -> Option<String> {
        let end = self.window_end(cx)?;
        let left = access::remaining_secs(end, termoak_core::time::now_ms());
        (left > 0).then(|| access::short_remaining(left))
    }

    /// The access book changed: when this terminal waits for a window that
    /// is open now, it connects.
    fn on_access_book(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.access_need.is_some()
            && let Some((account, host)) = self.access_target(cx)
        {
            // Forgotten (a policy changed): ask again.
            let known = AccessBook::try_global(cx)
                .is_some_and(|b| b.read(cx).host(account, host).is_some());
            if !known {
                let api = self.app.read(cx).api_of(Some(account));
                if let (Some(api), Some(book)) = (api, AccessBook::try_global(cx)) {
                    book.update(cx, |b, cx| b.ensure_host(account, host, api, false, cx));
                }
            }
            let now = termoak_core::time::now_ms();
            let open = AccessBook::try_global(cx).is_some_and(|b| {
                matches!(
                    b.read(cx).window(account, host, now),
                    Some(HostWindow::Open(_))
                )
            });
            if open {
                self.connect(window, cx);
                return;
            }
            // A request made elsewhere (the Access section, another window).
            let pending = AccessBook::try_global(cx).and_then(|b| {
                b.read(cx)
                    .host(account, host)
                    .and_then(|h| h.pending.as_ref().map(|p| p.id))
            });
            if let Some(need) = self.access_need.as_mut() {
                need.pending = pending;
            }
        }
        cx.notify();
    }

    /// Toolbar pills: the access window and "Recorded".
    pub(super) fn access_badges(&self, cx: &gpui::App) -> Vec<(String, gpui::Hsla, SharedString)> {
        let mut out = Vec::new();
        if !matches!(self.state, TermState::Running | TermState::Connecting(_)) {
            return out;
        }
        let Some((account, host)) = self.access_target(cx) else {
            return out;
        };
        let theme = cx.theme();
        let now = termoak_core::time::now_ms();
        let book = AccessBook::try_global(cx);
        let answer = book
            .as_ref()
            .and_then(|b| b.read(cx).host(account, host).cloned());
        if let Some(end) = answer
            .as_ref()
            .and_then(|a| a.grant.as_ref())
            .and_then(|g| g.ends_at)
        {
            let left = access::remaining_secs(end, now);
            if left > 0 {
                out.push((
                    access::window_label(end, now),
                    if access::ending_soon(left) {
                        theme.warning
                    } else {
                        theme.success
                    },
                    t!("access.bar.tooltip"),
                ));
            } else {
                out.push((
                    t!("access.bar.ended").to_string(),
                    theme.danger,
                    t!("access.bar.ended_tooltip"),
                ));
            }
        }
        let recorded = self.app.read(cx).host_gates(host).recorded
            || answer.as_ref().is_some_and(|a| a.record_required);
        if recorded {
            out.push((
                format!("● {}", t!("access.recorded")),
                theme.danger,
                t!("access.recorded_tip"),
            ));
        }
        out
    }

    /// Instead of the error: what is needed and "Request access".
    pub(super) fn render_access_card(&self, _text: String, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let need = self.access_need.clone();
        let pending = need.as_ref().and_then(|n| n.pending).is_some();
        let max = need.as_ref().map(|n| n.max_minutes).unwrap_or(60);
        v_flex()
            .absolute()
            .inset_0()
            .items_center()
            .justify_center()
            .child(
                ui::card(cx)
                    .p_6()
                    .gap_3()
                    .max_w(px(560.))
                    .items_center()
                    .child(
                        ui::icon(if pending {
                            IconName::Hourglass
                        } else {
                            IconName::LockKeyhole
                        })
                        .size(px(32.))
                        .text_color(theme.warning),
                    )
                    .child(div().text_lg().font_semibold().child(if pending {
                        t!("access.card.pending_title")
                    } else {
                        t!("access.card.title")
                    }))
                    .child(
                        div()
                            .text_sm()
                            .text_center()
                            .text_color(theme.muted_foreground)
                            .child(if pending {
                                t!("access.card.pending_text").to_string()
                            } else {
                                t!("access.card.text", max = access::duration_label(max))
                                    .to_string()
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("access-request")
                                    .primary()
                                    .icon(ui::icon(if pending {
                                        IconName::X
                                    } else {
                                        IconName::Send
                                    }))
                                    .label(if pending {
                                        t!("access.request.cancel_request")
                                    } else {
                                        t!("access.request.button")
                                    })
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.request_access(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("access-retry")
                                    .icon(ui::icon(IconName::RefreshCw))
                                    .label(t!("common.retry"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.connect(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("access.card.auto")),
                    ),
            )
            .into_any_element()
    }

    /// "Request access" (or cancel the waiting request).
    fn request_access(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(need), Some((account, host))) =
            (self.access_need.clone(), self.access_target(cx))
        else {
            return;
        };
        let mut need = need;
        if need.host_id.is_none() {
            need.host_id = Some(host);
        }
        let label = self.app.read(cx).host_label(host);
        crate::views::access_requests::open_request_dialog(
            self.app.clone(),
            account,
            label,
            need,
            window,
            cx,
        );
    }
}

/// What the connection details dialog shows of a certificate.
pub fn certificate_rows(
    d: &termoak_ssh::ca::CertificateDetails,
    now_secs: u64,
) -> Vec<(SharedString, String)> {
    let when = |s: u64| crate::ui::format_secs(s as i64);
    let mut rows = vec![
        (t!("conn_details.cert.key_id"), d.key_id.clone()),
        (t!("conn_details.cert.serial"), d.serial.to_string()),
        (t!("conn_details.cert.principals"), d.principals.join(", ")),
        (t!("conn_details.cert.valid_after"), when(d.valid_after)),
        (
            t!("conn_details.cert.valid_before"),
            if d.valid_before > now_secs {
                format!(
                    "{} ({})",
                    when(d.valid_before),
                    t!(
                        "conn_details.cert.left",
                        left = access::short_remaining((d.valid_before - now_secs) as i64)
                    )
                )
            } else {
                format!(
                    "{} ({})",
                    when(d.valid_before),
                    t!("conn_details.cert.expired")
                )
            },
        ),
        (t!("conn_details.cert.extensions"), d.extensions.join(", ")),
    ];
    if !d.critical_options.is_empty() {
        rows.push((
            t!("conn_details.cert.options"),
            d.critical_options.join(", "),
        ));
    }
    rows.push((t!("conn_details.cert.ca"), d.ca_fingerprint.clone()));
    rows.push((
        t!("conn_details.cert.signature"),
        if d.signature_valid {
            t!("conn_details.cert.signature_ok").to_string()
        } else {
            t!("conn_details.cert.signature_bad").to_string()
        },
    ));
    rows
}

impl TerminalView {
    /// The connection details dialog: where it is connected and, for team
    /// certificates, the certificate (`parse_certificate`).
    pub(super) fn open_connection_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let TermKind::Local { host_id } = self.kind else {
            return;
        };
        let info = self
            .local
            .as_ref()
            .and_then(|t| t.connection().map(|c| c.info().clone()));
        let (ws, scope, team_cert, can_resolve, api) = {
            let app = self.app.read(cx);
            let rec = app.host_record(host_id);
            let team_cert = rec.is_some_and(|h| {
                app.host_auth(h)
                    .auth
                    .is_some_and(|a| a.is_team_certificate())
            });
            // Resolving again reuses the cached certificate; Use-only
            // members of normal vaults would fetch just-in-time credentials
            // again, so they read the server's list instead.
            let can_resolve = rec.is_some_and(|h| h.access != termoak_client::ItemAccess::UseOnly);
            (
                app.ws.clone(),
                rec.map(|h| h.scope),
                team_cert,
                can_resolve,
                app.account_of_host(host_id)
                    .and_then(|a| app.api_of(Some(a))),
            )
        };
        let mut rows: Vec<(SharedString, String)> = Vec::new();
        if let Some(i) = &info {
            rows.push((
                t!("conn_details.host"),
                format!("{}@{}:{}", i.username, i.address, i.port),
            ));
            if !i.via.is_empty() {
                rows.push((t!("conn_details.via"), i.via.join(" → ")));
            }
            if let (Some(kind), Some(fp)) = (&i.server_key_type, &i.server_fingerprint) {
                rows.push((t!("conn_details.host_key"), format!("{kind} {fp}")));
            }
        }
        let title = t!("conn_details.title", host = self.host_label.clone());
        let fetch = match (team_cert, scope) {
            (true, Some(scope)) => Some((ws, scope)),
            _ => None,
        };
        let task = crate::runtime::spawn(cx, async move {
            let Some((ws, scope)) = fetch else {
                return Ok::<_, String>(None);
            };
            if can_resolve {
                let mut resolved = ws
                    .resolve_item(termoak_client::ItemRef { scope, id: host_id })
                    .await
                    .map_err(crate::state::api_error)?;
                let cert = resolved.key.as_ref().and_then(|k| k.certificate.clone());
                resolved.zeroize_secrets();
                return Ok(cert
                    .and_then(|c| termoak_ssh::ca::parse_certificate(&c).ok())
                    .map(CertView::Parsed));
            }
            // Use-only: the server's list of your certificates.
            let Some(api) = api else {
                return Ok(None);
            };
            let list: Vec<termoak_core::access::IssuedCertificate> = api
                .get("/api/v1/certificates")
                .await
                .map_err(crate::state::api_error)?;
            let now = termoak_core::time::now_ms();
            Ok(list
                .into_iter()
                .filter(|c| {
                    c.host_id == Some(host_id) && c.valid_before > now && c.revoked_at.is_none()
                })
                .max_by_key(|c| c.valid_before)
                .map(CertView::Issued))
        });
        cx.spawn_in(window, async move |_, cx| {
            let cert = task.await;
            let _ = cx.update(|window, cx| {
                let now_secs = (termoak_core::time::now_ms() / 1000) as u64;
                let mut all = rows.clone();
                let mut note = None;
                match cert {
                    Ok(Some(CertView::Parsed(d))) => {
                        all.push((
                            t!("conn_details.auth"),
                            t!("conn_details.auth_cert").to_string(),
                        ));
                        all.extend(certificate_rows(&d, now_secs));
                    }
                    Ok(Some(CertView::Issued(c))) => {
                        all.push((
                            t!("conn_details.auth"),
                            t!("conn_details.auth_cert").to_string(),
                        ));
                        all.push((t!("conn_details.cert.key_id"), c.key_id.clone()));
                        all.push((t!("conn_details.cert.serial"), c.serial.to_string()));
                        all.push((t!("conn_details.cert.principals"), c.principals.join(", ")));
                        all.push((
                            t!("conn_details.cert.valid_before"),
                            crate::ui::format_ms(c.valid_before),
                        ));
                    }
                    Ok(None) if team_cert => note = Some(t!("conn_details.no_cert")),
                    Ok(None) => {}
                    Err(e) => note = Some(e.into()),
                }
                let rows = all;
                window.open_dialog(cx, move |d, _, cx| {
                    let muted = cx.theme().muted_foreground;
                    d.title(title.clone()).w(px(600.)).child(
                        v_flex()
                            .gap_1()
                            .children(rows.iter().map(|(k, v)| {
                                h_flex()
                                    .gap_3()
                                    .items_start()
                                    .text_sm()
                                    .child(
                                        div()
                                            .w(px(170.))
                                            .flex_shrink_0()
                                            .text_color(muted)
                                            .child(k.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .font_family(crate::ui::mono_family(cx))
                                            .text_xs()
                                            .child(v.clone()),
                                    )
                            }))
                            .when_some(note.clone(), |this, n| {
                                this.child(div().pt_2().text_xs().text_color(muted).child(n))
                            }),
                    )
                });
            });
        })
        .detach();
    }
}

/// A certificate as the dialog can show it.
enum CertView {
    /// The certificate itself (parsed here).
    Parsed(termoak_ssh::ca::CertificateDetails),
    /// The server's record (Use-only members).
    Issued(termoak_core::access::IssuedCertificate),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_rows_show_validity() {
        rust_i18n::set_locale("en");
        let d = termoak_ssh::ca::CertificateDetails {
            key_id: "termoak:ana@example.com:u:direct:42".into(),
            serial: 42,
            cert_type: "user".into(),
            principals: vec!["deploy".into()],
            valid_after: 1_000,
            valid_before: 1_000 + 3_600,
            extensions: vec!["permit-pty".into()],
            critical_options: vec![],
            key_fingerprint: "SHA256:k".into(),
            ca_fingerprint: "SHA256:ca".into(),
            signature_valid: true,
        };
        let rows = certificate_rows(&d, 1_000 + 600);
        let text: Vec<String> = rows.iter().map(|(_, v)| v.clone()).collect();
        assert!(text.iter().any(|v| v == "deploy"));
        assert!(text.iter().any(|v| v.contains("50 min")));
        assert!(text.iter().any(|v| v == "SHA256:ca"));
        let expired = certificate_rows(&d, 10_000);
        assert!(expired.iter().any(|(_, v)| v.contains("expired")));
    }
}
