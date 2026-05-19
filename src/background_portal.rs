// SPDX-License-Identifier: GPL-3.0-or-later
// XDG Background portal — requests autostart permission from the desktop.

use futures::StreamExt;
use std::collections::HashMap;
use zbus::zvariant::Value;

/// Ask the XDG Background portal to enable or disable autostart.
///
/// On success returns whether autostart was actually granted by the portal.
/// On error (e.g. no portal available) returns the error so the caller can
/// revert the toggle.
pub async fn request_autostart(enable: bool) -> anyhow::Result<bool> {
    let conn = zbus::Connection::session().await?;

    // Build a unique handle token for this request.
    let token = format!(
        "pinepal_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
    );

    // Compute the expected Request object path before calling.
    // Spec: strip leading ':', replace all '.' with '_'.
    let sender = conn
        .unique_name()
        .map(|n| n.as_str().trim_start_matches(':').replace('.', "_"))
        .unwrap_or_default();
    let request_path = format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        sender, token
    );

    // Subscribe to the Response signal *before* the call to avoid a race.
    let request_proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        request_path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await?;
    let mut response_stream = request_proxy.receive_signal("Response").await?;

    // Build the options dict.
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert(
        "reason",
        Value::from("Keep PinePal running to stay connected to your PineTime"),
    );
    options.insert("autostart", Value::from(enable));
    options.insert("dbus-activatable", Value::from(true));

    let portal = zbus::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Background",
    )
    .await?;

    // Empty string = no parent window handle.
    let _req: zbus::zvariant::OwnedObjectPath =
        portal.call("RequestBackground", &("", &options)).await?;

    // Wait for the Response signal.
    match response_stream.next().await {
        Some(msg) => {
            let (code, results): (u32, HashMap<String, zbus::zvariant::OwnedValue>) =
                msg.body().deserialize()?;
            if code != 0 {
                // 1 = user cancelled, 2 = other error
                return Ok(false);
            }
            // The portal reports whether autostart was actually granted.
            if let Some(val) = results.get("autostart") {
                let granted = bool::try_from(val).unwrap_or(false);
                return Ok(granted);
            }
            Ok(enable)
        }
        None => Ok(false),
    }
}
