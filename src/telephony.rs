// SPDX-License-Identifier: GPL-3.0-or-later
//! Bridges incoming phone calls (oFono) to the watch and routes the watch's
//! answer/reject back to the call.
//!
//! Watches the modem's `VoiceCallManager` on the system bus for incoming calls,
//! shows them on the watch, and turns the watch's Notification Event response
//! into an Answer/Hangup on the call. If oFono is absent (e.g. on the desktop)
//! the task just exits and the rest of the app runs unaffected.

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::sync::mpsc;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::ble_manager::{BleCommand, BleHandle, CallAction};

/// Spawn the telephony bridge on the given tokio runtime.
pub fn spawn_telephony(
    rt: &tokio::runtime::Runtime,
    ble: BleHandle,
    action_rx: mpsc::Receiver<CallAction>,
) {
    rt.spawn(async move {
        if let Err(e) = run(ble, action_rx).await {
            log::info!("Telephony bridge inactive: {e:#}");
        }
    });
}

#[zbus::proxy(
    interface = "org.ofono.Manager",
    default_service = "org.ofono",
    default_path = "/"
)]
trait OfonoManager {
    fn get_modems(&self) -> zbus::Result<Vec<(OwnedObjectPath, HashMap<String, OwnedValue>)>>;
}

#[zbus::proxy(interface = "org.ofono.VoiceCallManager", default_service = "org.ofono")]
trait VoiceCallManager {
    #[zbus(signal)]
    fn call_added(
        &self,
        path: OwnedObjectPath,
        properties: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    fn call_removed(&self, path: OwnedObjectPath) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "org.ofono.VoiceCall", default_service = "org.ofono")]
trait VoiceCall {
    fn answer(&self) -> zbus::Result<()>;
    fn hangup(&self) -> zbus::Result<()>;
}

async fn run(ble: BleHandle, mut action_rx: mpsc::Receiver<CallAction>) -> Result<()> {
    let conn = zbus::Connection::system().await.context("system bus")?;
    let manager = OfonoManagerProxy::new(&conn).await.context("ofono manager")?;
    let modems = manager.get_modems().await.context("listing modems")?;
    let modem = modems
        .into_iter()
        .next()
        .map(|(path, _)| path)
        .context("no modem present")?;

    let vcm = VoiceCallManagerProxy::builder(&conn)
        .path(modem.clone())?
        .build()
        .await
        .context("voice call manager")?;
    let mut added = vcm.receive_call_added().await?;
    let mut removed = vcm.receive_call_removed().await?;
    log::info!("Watching for incoming calls on modem {}", modem.as_str());

    // The call we've told the watch about, so we can act on it.
    let mut current: Option<OwnedObjectPath> = None;

    loop {
        tokio::select! {
            Some(sig) = added.next() => {
                let Ok(args) = sig.args() else { continue };
                if str_prop(args.properties(), "State").as_deref() != Some("incoming") {
                    continue;
                }
                let number = str_prop(args.properties(), "LineIdentification").unwrap_or_default();
                let name = str_prop(args.properties(), "Name").unwrap_or_default();
                log::info!("Incoming call: {name} {number}");
                current = Some(args.path().clone());
                ble.send(BleCommand::IncomingCall { name, number });
            }
            Some(sig) = removed.next() => {
                let Ok(args) = sig.args() else { continue };
                if current.as_ref().map(|p| p.as_str()) == Some(args.path().as_str()) {
                    // InfiniTime has no BLE "cancel call"; the screen dismisses
                    // when the user acts on the watch. A call ended/missed on the
                    // phone side keeps showing until then — same as pinetime-furios.
                    current = None;
                }
            }
            action = action_rx.recv() => {
                let Some(action) = action else { return Ok(()) }; // app shutting down
                let Some(path) = current.clone() else { continue };
                if let Err(e) = act_on_call(&conn, path, action).await {
                    // Usually just a race: the call ended (e.g. missed) right as the
                    // watch button was pressed, so there's nothing left to act on.
                    log::debug!("Call action {action:?} had no effect (call already ended?): {e:#}");
                }
                // Answer/Reject resolve the call — drop our handle so a late event
                // for it is ignored. Mute leaves the call ringing.
                if !matches!(action, CallAction::Mute) {
                    current = None;
                }
            }
        }
    }
}

async fn act_on_call(
    conn: &zbus::Connection,
    path: OwnedObjectPath,
    action: CallAction,
) -> Result<()> {
    let call = VoiceCallProxy::builder(conn).path(path)?.build().await?;
    match action {
        CallAction::Answer => call.answer().await.context("answering call")?,
        CallAction::Reject => call.hangup().await.context("rejecting call")?,
        // Silencing the ringer is the phone's job, not oFono's — nothing to do.
        CallAction::Mute => log::info!("Call muted on watch (ringer is phone-side)"),
    }
    Ok(())
}

fn str_prop(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| String::try_from(v).ok())
        .filter(|s| !s.is_empty())
}
