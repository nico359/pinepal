// SPDX-License-Identifier: GPL-3.0-or-later
// D-Bus notification monitor — watches for desktop notifications and forwards them to the watch.

use crate::ble_manager::{BleCommand, BleHandle};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use zbus::zvariant::{Type, Value};

/// Spawn a task that monitors D-Bus for desktop notifications and
/// sends them to the BLE manager for forwarding to the watch.
pub fn spawn_notification_forwarder(
    rt: &tokio::runtime::Runtime,
    ble: BleHandle,
    enabled: Arc<AtomicBool>,
) {
    rt.spawn(async move {
        if let Err(e) = run_forwarder(ble, enabled).await {
            log::error!("Notification forwarder error: {e}");
        }
    });
}

#[allow(unused)]
#[derive(Debug, serde::Deserialize, Type)]
struct DesktopNotification<'s> {
    app_name: &'s str,
    replaces_id: u32,
    app_icon: &'s str,
    summary: &'s str,
    body: &'s str,
    actions: Vec<&'s str>,
    hints: HashMap<&'s str, Value<'s>>,
    expire_timeout: i32,
}

async fn run_forwarder(ble: BleHandle, enabled: Arc<AtomicBool>) -> anyhow::Result<()> {
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::fdo::MonitoringProxy::builder(&conn)
        .destination("org.freedesktop.DBus")?
        .path("/org/freedesktop/DBus")?
        .build()
        .await?;

    let rule = zbus::match_rule::MatchRule::builder()
        .msg_type(zbus::message::Type::MethodCall)
        .interface("org.freedesktop.Notifications")?
        .member("Notify")?
        .path("/org/freedesktop/Notifications")?
        .build();
    proxy.become_monitor(&[rule], 0).await?;

    let mut stream = zbus::MessageStream::from(&conn);
    use futures::TryStreamExt;

    while let Some(msg) = stream.try_next().await? {
        if !enabled.load(Ordering::Relaxed) {
            continue;
        }

        match msg.body().deserialize::<DesktopNotification>() {
            Ok(notif) => {
                // Skip duplicate — GNOME Shell sends each notification twice,
                // the second copy has an "x-shell-sender" hint.
                if notif.hints.contains_key("x-shell-sender") {
                    continue;
                }

                let title = if notif.app_name.is_empty() {
                    notif.summary.to_string()
                } else {
                    format!("{}: {}", notif.app_name, notif.summary)
                };

                log::debug!("Forwarding notification: {title}");
                ble.send(BleCommand::SendNotification {
                    title,
                    body: notif.body.to_string(),
                });
            }
            Err(e) => {
                log::debug!("Failed to parse notification: {e}");
            }
        }
    }

    Ok(())
}
