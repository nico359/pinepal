// SPDX-License-Identifier: GPL-3.0-or-later
// Garmin BLE connection manager.
// Handles discovery, connection, and notification forwarding for Garmin watches
// using the GadgetBridge Garmin v2 protocol over BLE.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use bluer::{Adapter, Address, Device};
use bluer::gatt::remote::Characteristic;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Duration};
use uuid::Uuid;

use crate::ble_manager::BleEvent;
use crate::cobs::CobsCodec;
use crate::gfdi;

// Garmin ML GFDI service UUID pattern: 6A4E%04X-667B-11E3-949A-0800200C9A66
// send characteristic = receive + 0x10 (e.g. 2810 recv / 2820 send)

// Gadgetbridge client ID used in handle management messages.
const CLIENT_ID: u64 = 2;
const GFDI_SERVICE_CODE: u16 = 1; // Service::GFDI

const CONNECT_TIMEOUT_SECS: u64 = 20;

// ---- Handle management request types ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ReqType {
    RegisterMlReq = 0,
    RegisterMlResp = 1,
    CloseHandleReq = 2,
    CloseHandleResp = 3,
    CloseAllReq = 5,
    CloseAllResp = 6,
}

impl ReqType {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::RegisterMlReq),
            1 => Some(Self::RegisterMlResp),
            2 => Some(Self::CloseHandleReq),
            3 => Some(Self::CloseHandleResp),
            5 => Some(Self::CloseAllReq),
            6 => Some(Self::CloseAllResp),
            _ => None,
        }
    }
}

// ---- Public API ----

// ---- Public API ----

/// Spawn the Garmin BLE manager on the given tokio runtime.
/// Returns a command sender and event receiver (using the shared BleEvent type).
pub fn spawn(
    rt: &tokio::runtime::Runtime,
) -> (GarminHandle, mpsc::Receiver<BleEvent>) {
    let (event_tx, event_rx) = mpsc::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    rt.spawn(garmin_task(event_tx, cmd_rx));
    (GarminHandle { cmd_tx }, event_rx)
}

#[derive(Clone, Debug)]
pub struct GarminHandle {
    cmd_tx: mpsc::Sender<GarminCommand>,
}

impl GarminHandle {
    pub fn send(&self, cmd: GarminCommand) {
        let _ = self.cmd_tx.try_send(cmd);
    }
}

#[derive(Debug)]
pub enum GarminCommand {
    StartScan,
    Connect(Address),
    Disconnect,
    SendNotification { title: String, body: String },
    Shutdown,
}

// ---- Main task ----

async fn garmin_task(
    tx: mpsc::Sender<BleEvent>,
    mut rx: mpsc::Receiver<GarminCommand>,
) {
    log::info!("Garmin BLE task started");

    let session = match bluer::Session::new().await {
        Ok(s) => s,
        Err(e) => {
            log::error!("Garmin: Bluetooth session init failed: {e}");
            let _ = tx.send(BleEvent::Error(format!("Bluetooth init failed: {e}"))).await;
            return;
        }
    };
    let adapter = match session.default_adapter().await {
        Ok(a) => a,
        Err(e) => {
            log::error!("Garmin: No Bluetooth adapter: {e}");
            let _ = tx.send(BleEvent::Error(format!("No Bluetooth adapter: {e}"))).await;
            return;
        }
    };

    // Wait for Bluetooth power
    if !wait_for_bt(&adapter, &tx, &mut rx).await {
        return;
    }

    let mut auto_addr: Option<Address> = None;
    let mut attempts: u32 = 0;
    let mut user_disconnected = false;

    loop {
        if let (Some(addr), false) = (auto_addr, user_disconnected) {
            if attempts > 0 {
                let delay = reconnect_delay(attempts);
                log::info!("Garmin: reconnect attempt {attempts}, waiting {delay}s");
                let _ = tx.send(BleEvent::Reconnecting { attempt: attempts, delay_secs: delay }).await;
                tokio::select! {
                    _ = sleep(Duration::from_secs(delay)) => {}
                    Some(cmd) = rx.recv() => {
                        match cmd {
                            GarminCommand::Disconnect | GarminCommand::StartScan => {
                                auto_addr = None;
                                user_disconnected = true;
                                attempts = 0;
                                let _ = tx.send(BleEvent::Disconnected { reason: "User cancelled".into() }).await;
                                continue;
                            }
                            GarminCommand::Shutdown => return,
                            GarminCommand::Connect(new_addr) => {
                                auto_addr = Some(new_addr);
                                attempts = 0;
                                user_disconnected = false;
                                continue;
                            }
                            _ => {}
                        }
                    }
                }
            }

            let _ = tx.send(BleEvent::Reconnecting { attempt: attempts + 1, delay_secs: 0 }).await;

            match connect_garmin(&adapter, addr, &tx, &mut rx).await {
                Ok(DisconnectReason::UserRequested) => {
                    auto_addr = None;
                    user_disconnected = true;
                    attempts = 0;
                }
                Ok(DisconnectReason::Shutdown) => return,
                Ok(DisconnectReason::NewDevice(new_addr)) => {
                    auto_addr = Some(new_addr);
                    attempts = 0;
                }
                Err(e) => {
                    log::warn!("Garmin connection attempt {} failed: {e}", attempts + 1);
                    attempts += 1;
                }
            }
            continue;
        }

        // Idle: wait for command
        match rx.recv().await {
            Some(GarminCommand::StartScan) => {
                let _ = tx.send(BleEvent::Scanning).await;
                if let Err(e) = scan_garmin(&adapter, &tx).await {
                    log::error!("Garmin scan error: {e}");
                    let _ = tx.send(BleEvent::Error(format!("Scan error: {e}"))).await;
                }
            }
            Some(GarminCommand::Connect(addr)) => {
                auto_addr = Some(addr);
                attempts = 0;
                user_disconnected = false;
            }
            Some(GarminCommand::Shutdown) | None => return,
            _ => {}
        }
    }
}

// ---- Bluetooth power wait ----

async fn wait_for_bt(
    adapter: &Adapter,
    _tx: &mpsc::Sender<BleEvent>,
    rx: &mut mpsc::Receiver<GarminCommand>,
) -> bool {
    if adapter.is_powered().await.unwrap_or(true) {
        return true;
    }
    // Simple polling fallback (same as InfiniTime path)
    loop {
        sleep(Duration::from_secs(2)).await;
        match adapter.is_powered().await {
            Ok(true) => return true,
            Ok(false) => {}
            Err(_) => return true,
        }
        // Check for shutdown
        if let Ok(GarminCommand::Shutdown) = rx.try_recv() {
            return false;
        }
    }
}

// ---- Scan ----

async fn scan_garmin(adapter: &Adapter, tx: &mpsc::Sender<BleEvent>) -> Result<()> {
    let filter = bluer::DiscoveryFilter {
        transport: bluer::DiscoveryTransport::Le,
        ..Default::default()
    };
    adapter.set_discovery_filter(filter).await?;

    log::info!("Garmin: Discovery started (10 s window)");
    let discover = adapter.discover_devices().await?;
    tokio::pin!(discover);

    let scan_end = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut found = 0u32;

    loop {
        let remaining = scan_end.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, discover.next()).await {
            Ok(Some(bluer::AdapterEvent::DeviceAdded(addr))) => {
                if let Ok(device) = adapter.device(addr) {
                    // Check if this device has Garmin UUIDs (we can't fully scan without connecting,
                    // but we can check the device name or just show it for user selection).
                    // For MVP: show any device — the user selects which one to connect to.
                    // More precise filtering would require connecting first.
                    let name = device.name().await.ok().flatten().unwrap_or_default();
                    if is_likely_garmin(&name, adapter, addr).await {
                        log::info!("Garmin: Found likely Garmin device at {addr}: '{name}'");
                        found += 1;
                        let _ = tx.send(BleEvent::DeviceFound { address: addr, name, rssi: None }).await;
                    }
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    log::info!("Garmin: Discovery finished — found {found} device(s)");
    Ok(())
}

/// Heuristic check: match device name against known Garmin model prefixes.
async fn is_likely_garmin(name: &str, _adapter: &Adapter, _addr: Address) -> bool {
    // Common Garmin watch prefixes
    let garmin_prefixes = [
        "fenix", "Forerunner", "vivo", "Venu", "Instinct",
        "Edge", "MARQ", "tactix", "Descent", "Enduro", "Lily",
        "Approach", "quatix", "D2", "epix",
    ];
    for prefix in &garmin_prefixes {
        if name.starts_with(prefix) || name.to_lowercase().contains(&prefix.to_lowercase()) {
            return true;
        }
    }
    // Fallback: any device with a non-empty name; the user can manually connect.
    // This ensures we don't miss devices during discovery.
    !name.is_empty()
}

// ---- Connection ----

enum DisconnectReason {
    UserRequested,
    Shutdown,
    NewDevice(Address),
}

async fn connect_garmin(
    adapter: &Adapter,
    addr: Address,
    tx: &mpsc::Sender<BleEvent>,
    rx: &mut mpsc::Receiver<GarminCommand>,
) -> Result<DisconnectReason> {
    use std::pin::Pin;

    let device = adapter.device(addr)?;
    let name = device.name().await.ok().flatten().unwrap_or_else(|| "Garmin".into());

    log::info!("Garmin: Connecting to {addr} ({name})");

    // Connect with timeout
    let connect_fut = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), device.connect());
    tokio::pin!(connect_fut);
    let connect_result = loop {
        tokio::select! {
            result = &mut connect_fut => break result,
            Some(cmd) = rx.recv() => {
                match cmd {
                    GarminCommand::Disconnect | GarminCommand::StartScan => {
                        let _ = tx.send(BleEvent::Disconnected { reason: "User cancelled".into() }).await;
                        return Ok(DisconnectReason::UserRequested);
                    }
                    GarminCommand::Shutdown => return Ok(DisconnectReason::Shutdown),
                    _ => {}
                }
            }
        }
    };
    match connect_result {
        Err(_) => return Err(anyhow!("Connection timed out")),
        Ok(Err(e)) => return Err(anyhow!("Connection failed: {e}")),
        Ok(Ok(())) => log::info!("Garmin: TCP/ACL link established"),
    }

    // Discover characteristics
    let chars = discover_characteristics(&device).await?;
    log::info!("Garmin: Discovered {} characteristics", chars.len());

    // Find Garmin ML send/receive pair
    let (recv_uuid, send_uuid) = find_garmin_characteristics(&chars)?;
    log::info!("Garmin: Using recv={recv_uuid}, send={send_uuid}");

    let recv_chr = chars.get(&recv_uuid).ok_or_else(|| anyhow!("Receive characteristic not found"))?;
    let send_chr = chars.get(&send_uuid).ok_or_else(|| anyhow!("Send characteristic not found"))?;

    // Enable notifications on receive characteristic
    let mut recv_stream: Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>> = Box::pin(recv_chr.notify().await?);
    log::info!("Garmin: Notifications enabled on receive characteristic");

    // Emit connected event
    let _ = tx.send(BleEvent::Connected {
        address: addr,
        firmware: format!("Garmin ({name})"),
    }).await;

    // ---- Initialize: CLOSE_ALL handshake ----
    let close_all = build_close_all_message();
    log::info!("Garmin: Sending CLOSE_ALL_REQ");
    send_chr.write(&close_all).await?;

    // Wait briefly for CLOSE_ALL_RESP
    let init_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut registration_done = false;
    loop {
        let remaining = init_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            log::debug!("Garmin: Init handshake timed out — proceeding");
            break;
        }
        tokio::select! {
            val = recv_stream.next() => {
                match val {
                    Some(data) if !data.is_empty() && data[0] == 0 && data.len() >= 2 => {
                        match ReqType::from_u8(data[1]) {
                            Some(ReqType::CloseAllResp) => {
                                log::info!("Garmin: CLOSE_ALL_RESP received — registering GFDI");
                                let reg_msg = build_register_gfdi_message();
                                if let Err(e) = send_chr.write(&reg_msg).await {
                                    log::warn!("Garmin: GFDI registration write failed: {e}");
                                }
                            }
                            Some(ReqType::RegisterMlResp) => {
                                log::info!("Garmin: REGISTER_ML_RESP received — ready");
                                registration_done = true;
                            }
                            _ => {}
                        }
                    }
                    None => break,
                    _ => {}
                }
            }
            _ = sleep(remaining) => break,
            Some(cmd) = rx.recv() => {
                match cmd {
                    GarminCommand::Disconnect | GarminCommand::Shutdown => break,
                    _ => {}
                }
            }
        }
        if registration_done {
            break;
        }
    }
    log::info!("Garmin: Init complete (registered={registration_done})");

    // ---- Connected event loop ----
    let mut prop_stream = device.events().await?;
    let mut cobs_codec = CobsCodec::new();
    let mut notification_id: i32 = 100;

    loop {
        tokio::select! {
            // Incoming data from watch
            val = recv_stream.next() => {
                match val {
                    Some(data) => {
                        log::debug!("Garmin recv: {} bytes", data.len());
                        // Handle management messages (handle 0) or GFDI data
                        if !data.is_empty() && data[0] == 0 {
                            // Handle management message — just log for MVP
                            log::debug!("Garmin: handle mgmt message: {:02x?}", &data[..data.len().min(16)]);
                            // Check if it's CLOSE_ALL_RESP
                            if data.len() >= 2 && data[1] == ReqType::CloseAllResp as u8 {
                                log::info!("Garmin: Received CLOSE_ALL_RESP during connected state");
                            }
                        } else {
                            // COBS-encoded GFDI data
                            let messages = gfdi::decode_from_ble(&mut cobs_codec, &data);
                            for (msg_id, decoded) in messages {
                                log::debug!("Garmin: decoded msg_id={msg_id}, {} bytes", decoded.len());
                                // Handle notification control (dismiss)
                                if msg_id == gfdi::MSG_NOTIFICATION_CONTROL && decoded.len() >= 11 {
                                    let command = decoded[4];
                                    let notif_id = i32::from_le_bytes([
                                        decoded[5], decoded[6], decoded[7], decoded[8],
                                    ]);
                                    log::info!("Garmin: Notification control: id={notif_id}, cmd={command}");
                                    // Dismiss: send ACK
                                    if command == 2 || command == 128 {
                                        let ack = gfdi::build_response(
                                            gfdi::MSG_NOTIFICATION_CONTROL,
                                            gfdi::Status::Ack,
                                        );
                                        let encoded = gfdi::encode_for_ble(&ack);
                                        if let Err(e) = send_chr.write(&encoded).await {
                                            log::warn!("Garmin: ACK write failed: {e}");
                                        }
                                        // Also send removal update
                                        let remove = gfdi::build_notification_update(
                                            gfdi::NotificationUpdateKind::Remove,
                                            gfdi::NotificationType::Generic,
                                            notif_id,
                                            0,
                                        );
                                        let encoded = gfdi::encode_for_ble(&remove);
                                        if let Err(e) = send_chr.write(&encoded).await {
                                            log::warn!("Garmin: Remove write failed: {e}");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        log::warn!("Garmin: Notification stream ended");
                        let _ = tx.send(BleEvent::Disconnected { reason: "Connection lost".into() }).await;
                        return Err(anyhow!("Notification stream ended"));
                    }
                }
            }
            // Device property changes
            evt = prop_stream.next() => {
                match evt {
                    Some(bluer::DeviceEvent::PropertyChanged(
                        bluer::DeviceProperty::Connected(false)
                    )) => {
                        log::warn!("Garmin: Device disconnected");
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "Watch disconnected".into(),
                        }).await;
                        return Err(anyhow!("Device disconnected"));
                    }
                    None => {
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "Connection lost".into(),
                        }).await;
                        return Err(anyhow!("Property stream ended"));
                    }
                    _ => {}
                }
            }
            // Commands from UI
            Some(cmd) = rx.recv() => {
                match cmd {
                    GarminCommand::Disconnect => {
                        log::info!("Garmin: User requested disconnect");
                        let _ = device.disconnect().await;
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "User disconnected".into(),
                        }).await;
                        return Ok(DisconnectReason::UserRequested);
                    }
                    GarminCommand::Connect(new_addr) => {
                        log::info!("Garmin: Switching to {new_addr}");
                        let _ = device.disconnect().await;
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "Switching device".into(),
                        }).await;
                        return Ok(DisconnectReason::NewDevice(new_addr));
                    }
                    GarminCommand::SendNotification { title, body } => {
                        log::info!("Garmin: Sending notification: '{title}'");
                        let update = gfdi::build_notification_update(
                            gfdi::NotificationUpdateKind::Add,
                            classify_notification(&title, &body),
                            notification_id,
                            1,
                        );
                        let encoded = gfdi::encode_for_ble(&update);
                        if let Err(e) = send_chr.write(&encoded).await {
                            log::warn!("Garmin: Notification write failed: {e}");
                        } else {
                            log::info!("Garmin: Notification sent (id={notification_id})");
                            notification_id = notification_id.wrapping_add(1);
                        }
                    }
                    GarminCommand::Shutdown => {
                        let _ = device.disconnect().await;
                        return Ok(DisconnectReason::Shutdown);
                    }
                    _ => {}
                }
            }
        }
    }
}

// ---- Message builders ----

fn build_close_all_message() -> Vec<u8> {
    let mut buf = Vec::with_capacity(13);
    buf.push(0); // handle 0 (management)
    buf.push(ReqType::CloseAllReq as u8);
    buf.extend_from_slice(&CLIENT_ID.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.push(0);
    buf
}

fn build_register_gfdi_message() -> Vec<u8> {
    let mut buf = Vec::with_capacity(13);
    buf.push(0); // handle 0
    buf.push(ReqType::RegisterMlReq as u8);
    buf.extend_from_slice(&CLIENT_ID.to_le_bytes());
    buf.extend_from_slice(&GFDI_SERVICE_CODE.to_le_bytes());
    buf.push(2); // reliable = true
    buf
}

// ---- Helpers ----

/// Discover all GATT characteristics into a UUID→Characteristic map.
async fn discover_characteristics(device: &Device) -> Result<HashMap<Uuid, Characteristic>> {
    for i in 0..50 {
        match device.is_services_resolved().await {
            Ok(true) => {
                log::debug!("Garmin: Services resolved after {}ms", i * 100);
                break;
            }
            Ok(false) => {}
            Err(e) => log::warn!("Garmin: is_services_resolved error: {e}"),
        }
        sleep(Duration::from_millis(100)).await;
    }

    let mut map = std::collections::HashMap::new();
    for service in device.services().await? {
        for chr in service.characteristics().await? {
            let uuid = chr.uuid().await?;
            map.insert(uuid, chr);
        }
    }
    if map.is_empty() {
        return Err(anyhow!("No characteristics found"));
    }
    Ok(map)
}

/// Find Garmin ML send/receive UUID pair.
fn find_garmin_characteristics(chars: &HashMap<Uuid, Characteristic>) -> Result<(Uuid, Uuid)> {
    for i in 0x2810u16..=0x2814 {
        let recv_str = format!("6A4E{:04X}-667B-11E3-949A-0800200C9A66", i);
        let send_str = format!("6A4E{:04X}-667B-11E3-949A-0800200C9A66", i + 0x10);
        let recv: uuid::Uuid = recv_str.parse().map_err(|_| anyhow!("Bad UUID"))?;
        let send: uuid::Uuid = send_str.parse().map_err(|_| anyhow!("Bad UUID"))?;
        if chars.contains_key(&recv) && chars.contains_key(&send) {
            return Ok((recv, send));
        }
    }
    Err(anyhow!("No Garmin ML characteristic pair found"))
}

/// Classify a notification for Garmin category assignment.
fn classify_notification(title: &str, body: &str) -> gfdi::NotificationType {
    let combined = format!("{title} {body}").to_lowercase();
    if combined.contains("incoming call") || combined.contains("phone") {
        gfdi::NotificationType::GenericPhone
    } else if combined.contains("sms") || combined.contains("message") {
        gfdi::NotificationType::GenericSms
    } else if combined.contains("mail") || combined.contains("email") {
        gfdi::NotificationType::GenericEmail
    } else {
        gfdi::NotificationType::Generic
    }
}

fn reconnect_delay(attempts: u32) -> u64 {
    let base: u64 = 1;
    base.saturating_mul(1u64.checked_shl(attempts.saturating_sub(1).min(6)).unwrap_or(60))
        .min(60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_close_all() {
        let msg = build_close_all_message();
        assert_eq!(msg.len(), 13);
        assert_eq!(msg[0], 0); // handle
        assert_eq!(msg[1], ReqType::CloseAllReq as u8);
        assert_eq!(u64::from_le_bytes(msg[2..10].try_into().unwrap()), CLIENT_ID);
    }

    #[test]
    fn test_build_register_gfdi() {
        let msg = build_register_gfdi_message();
        assert_eq!(msg.len(), 13);
        assert_eq!(msg[1], ReqType::RegisterMlReq as u8);
        assert_eq!(u16::from_le_bytes([msg[10], msg[11]]), GFDI_SERVICE_CODE);
        assert_eq!(msg[12], 2); // reliable
    }
}
