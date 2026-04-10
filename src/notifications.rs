// SPDX-License-Identifier: GPL-3.0-or-later
// D-Bus notification monitor — watches for desktop notifications and forwards them to the watch.

use crate::ble_manager::{BleCommand, BleHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use zbus::Connection;

/// Spawn a task that monitors D-Bus for desktop notifications and
/// sends them to the BLE manager for forwarding to the watch.
/// The `enabled` flag is toggled from the UI thread via GSettings binding.
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

async fn run_forwarder(ble: BleHandle, enabled: Arc<AtomicBool>) -> anyhow::Result<()> {
    let conn = Connection::session().await?;

    let rule = "type='method_call',interface='org.freedesktop.Notifications',member='Notify'";

    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus.Monitoring"),
        "BecomeMonitor",
        &(vec![rule], 0u32),
    )
    .await?;

    let mut stream = zbus::MessageStream::from(&conn);
    use futures::StreamExt;

    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => continue,
        };

        if !enabled.load(Ordering::Relaxed) {
            continue;
        }

        // Parse Notify call: (app_name, replaces_id, icon, summary, body, actions, hints, timeout)
        let body = msg.body();
        let parsed: Result<(String, u32, String, String, String), _> =
            body.deserialize();
        if let Ok((app_name, _replaces, _icon, summary, body_text)) = parsed {
            let title = if app_name.is_empty() {
                summary.clone()
            } else {
                format!("{app_name}: {summary}")
            };
            ble.send(BleCommand::SendNotification {
                title,
                body: body_text,
            });
        }
    }

    Ok(())
}
