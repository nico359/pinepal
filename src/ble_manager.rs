// SPDX-License-Identifier: GPL-3.0-or-later
// BLE connection manager for InfiniTime watches.
// Handles discovery, connection, characteristic I/O, and reconnection with backoff.

use anyhow::{anyhow, Context, Result};
use bluer::{Adapter, Address, Device};
use futures::StreamExt;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Duration};
use uuid::Uuid;

// Standard BLE UUIDs
const CHR_BATTERY: Uuid = uuid::uuid!("00002a19-0000-1000-8000-00805f9b34fb");
const CHR_FIRMWARE_REV: Uuid = uuid::uuid!("00002a26-0000-1000-8000-00805f9b34fb");
const CHR_HEART_RATE: Uuid = uuid::uuid!("00002a37-0000-1000-8000-00805f9b34fb");
const CHR_NEW_ALERT: Uuid = uuid::uuid!("00002a46-0000-1000-8000-00805f9b34fb");

// InfiniTime custom UUIDs
const CHR_STEP_COUNT: Uuid = uuid::uuid!("00030001-78fc-48fe-8e23-433b3a1942d0");

// Reconnection parameters
const BASE_DELAY_SECS: u64 = 1;
const MAX_DELAY_SECS: u64 = 60;
const CONNECT_TIMEOUT_SECS: u64 = 15;

const DEVICE_NAME: &str = "InfiniTime";

/// Events sent from BLE manager to the UI.
#[derive(Debug, Clone)]
pub enum BleEvent {
    Scanning,
    DeviceFound {
        address: Address,
        name: String,
        rssi: Option<i16>,
    },
    Connected {
        address: Address,
        firmware: String,
    },
    Disconnected {
        reason: String,
    },
    BatteryLevel(u8),
    HeartRate(u8),
    StepCount(u32),
    Error(String),
    Reconnecting {
        attempt: u32,
        delay_secs: u64,
    },
}

/// Commands sent from UI to BLE manager.
#[derive(Debug)]
pub enum BleCommand {
    StartScan,
    Connect(Address),
    Disconnect,
    SendNotification { title: String, body: String },
    Shutdown,
}

/// Handle for sending commands to the BLE task from the UI (glib) thread.
#[derive(Clone, Debug)]
pub struct BleHandle {
    cmd_tx: mpsc::Sender<BleCommand>,
}

impl BleHandle {
    /// Send a command to the BLE manager. Non-blocking, drops if full.
    pub fn send(&self, cmd: BleCommand) {
        let _ = self.cmd_tx.try_send(cmd);
    }
}

/// Spawn the BLE manager on the given tokio runtime.
/// Returns a command handle and a receiver for BLE events.
pub fn spawn(rt: &tokio::runtime::Runtime) -> (BleHandle, mpsc::Receiver<BleEvent>) {
    let (event_tx, event_rx) = mpsc::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    rt.spawn(ble_task(event_tx, cmd_rx));
    (BleHandle { cmd_tx }, event_rx)
}

/// Main BLE task — runs on tokio, manages state machine.
async fn ble_task(tx: mpsc::Sender<BleEvent>, mut rx: mpsc::Receiver<BleCommand>) {
    let session = match bluer::Session::new().await {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(BleEvent::Error(format!("Bluetooth init failed: {e}"))).await;
            return;
        }
    };
    let adapter = match session.default_adapter().await {
        Ok(a) => a,
        Err(e) => {
            let _ = tx.send(BleEvent::Error(format!("No Bluetooth adapter: {e}"))).await;
            return;
        }
    };

    let mut auto_addr: Option<Address> = None;
    let mut attempts: u32 = 0;
    let mut user_disconnected = false;

    loop {
        // If we should auto-reconnect, do so after backoff
        if let (Some(addr), false) = (auto_addr, user_disconnected) {
            if attempts > 0 {
                let delay = reconnect_delay(attempts);
                let _ = tx.send(BleEvent::Reconnecting { attempt: attempts, delay_secs: delay }).await;
                // Wait for delay OR a user command
                tokio::select! {
                    _ = sleep(Duration::from_secs(delay)) => {}
                    Some(cmd) = rx.recv() => {
                        match cmd {
                            BleCommand::Disconnect => {
                                auto_addr = None;
                                user_disconnected = true;
                                attempts = 0;
                                let _ = tx.send(BleEvent::Disconnected { reason: "User cancelled".into() }).await;
                                continue;
                            }
                            BleCommand::Shutdown => return,
                            BleCommand::Connect(new_addr) => {
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

            // Attempt connection
            match do_connect(&adapter, addr, &tx, &mut rx).await {
                Ok(DisconnectReason::UserRequested) => {
                    auto_addr = None;
                    user_disconnected = true;
                    attempts = 0;
                }
                Ok(DisconnectReason::Shutdown) => return,
                Ok(DisconnectReason::NewDevice(new_addr)) => {
                    auto_addr = Some(new_addr);
                    attempts = 0;
                    user_disconnected = false;
                }
                Err(_) => {
                    attempts += 1;
                }
            }
            continue;
        }

        // Idle — wait for a command
        match rx.recv().await {
            Some(BleCommand::StartScan) => {
                let _ = tx.send(BleEvent::Scanning).await;
                if let Err(e) = do_scan(&adapter, &tx).await {
                    let _ = tx.send(BleEvent::Error(format!("Scan error: {e}"))).await;
                }
            }
            Some(BleCommand::Connect(addr)) => {
                auto_addr = Some(addr);
                attempts = 0;
                user_disconnected = false;
            }
            Some(BleCommand::Shutdown) => return,
            Some(_) => {}
            None => return,
        }
    }
}

fn reconnect_delay(attempts: u32) -> u64 {
    BASE_DELAY_SECS
        .saturating_mul(1u64.checked_shl(attempts.saturating_sub(1).min(6)).unwrap_or(MAX_DELAY_SECS))
        .min(MAX_DELAY_SECS)
}

enum DisconnectReason {
    UserRequested,
    Shutdown,
    NewDevice(Address),
}

async fn do_scan(adapter: &Adapter, tx: &mpsc::Sender<BleEvent>) -> Result<()> {
    let filter = bluer::DiscoveryFilter {
        transport: bluer::DiscoveryTransport::Le,
        ..Default::default()
    };
    adapter.set_discovery_filter(filter).await?;

    let discover = adapter.discover_devices().await?;
    tokio::pin!(discover);

    let scan_end = tokio::time::Instant::now() + Duration::from_secs(10);

    loop {
        let remaining = scan_end.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, discover.next()).await {
            Ok(Some(bluer::AdapterEvent::DeviceAdded(addr))) => {
                if let Ok(device) = adapter.device(addr) {
                    let name = device.name().await.ok().flatten().unwrap_or_default();
                    if name == DEVICE_NAME {
                        let rssi = device.rssi().await.ok().flatten();
                        let _ = tx.send(BleEvent::DeviceFound { address: addr, name, rssi }).await;
                    }
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    Ok(())
}

async fn do_connect(
    adapter: &Adapter,
    addr: Address,
    tx: &mpsc::Sender<BleEvent>,
    rx: &mut mpsc::Receiver<BleCommand>,
) -> Result<DisconnectReason> {
    let device = adapter.device(addr)?;

    // Connect with timeout
    timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), device.connect())
        .await
        .context("Connection timed out")?
        .context("Connection failed")?;

    // Discover characteristics
    let chars = discover_characteristics(&device).await?;

    // Read firmware version
    let firmware = match chars.get(&CHR_FIRMWARE_REV) {
        Some(chr) => {
            let data = chr.read().await.unwrap_or_default();
            String::from_utf8(data).unwrap_or_else(|_| "Unknown".into())
        }
        None => "Unknown".into(),
    };

    let _ = tx.send(BleEvent::Connected { address: addr, firmware }).await;

    // Initial reads
    if let Some(chr) = chars.get(&CHR_BATTERY) {
        if let Ok(data) = chr.read().await {
            if let Some(&val) = data.first() {
                let _ = tx.send(BleEvent::BatteryLevel(val)).await;
            }
        }
    }
    if let Some(chr) = chars.get(&CHR_HEART_RATE) {
        if let Ok(data) = chr.read().await {
            if let Some(&val) = data.get(1) {
                let _ = tx.send(BleEvent::HeartRate(val)).await;
            }
        }
    }
    if let Some(chr) = chars.get(&CHR_STEP_COUNT) {
        if let Ok(data) = chr.read().await {
            if let Ok(bytes) = <[u8; 4]>::try_from(data.as_slice()) {
                let _ = tx.send(BleEvent::StepCount(u32::from_le_bytes(bytes))).await;
            }
        }
    }

    // Start notify streams (boxed since bluer streams aren't Unpin)
    let mut battery_stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>> =
        if let Some(chr) = chars.get(&CHR_BATTERY) {
            chr.notify().await.ok().map(|s| Box::pin(s) as _)
        } else { None };
    let mut hr_stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>> =
        if let Some(chr) = chars.get(&CHR_HEART_RATE) {
            chr.notify().await.ok().map(|s| Box::pin(s) as _)
        } else { None };
    let mut step_stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>> =
        if let Some(chr) = chars.get(&CHR_STEP_COUNT) {
            chr.notify().await.ok().map(|s| Box::pin(s) as _)
        } else { None };

    let alert_chr = chars.get(&CHR_NEW_ALERT).cloned();

    // Monitor device for disconnect
    let mut prop_stream = device.events().await?;

    // Connected event loop
    loop {
        tokio::select! {
            val = next_or_pending(&mut battery_stream) => {
                if let Some(&v) = val.first() {
                    let _ = tx.send(BleEvent::BatteryLevel(v)).await;
                }
            }
            val = next_or_pending(&mut hr_stream) => {
                if let Some(&v) = val.get(1) {
                    let _ = tx.send(BleEvent::HeartRate(v)).await;
                }
            }
            val = next_or_pending(&mut step_stream) => {
                if let Ok(bytes) = <[u8; 4]>::try_from(val.as_slice()) {
                    let _ = tx.send(BleEvent::StepCount(u32::from_le_bytes(bytes))).await;
                }
            }
            evt = prop_stream.next() => {
                match evt {
                    Some(bluer::DeviceEvent::PropertyChanged(
                        bluer::DeviceProperty::Connected(false)
                    )) => {
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
            Some(cmd) = rx.recv() => {
                match cmd {
                    BleCommand::Disconnect => {
                        let _ = device.disconnect().await;
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "User disconnected".into(),
                        }).await;
                        return Ok(DisconnectReason::UserRequested);
                    }
                    BleCommand::Connect(new_addr) => {
                        let _ = device.disconnect().await;
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "Switching device".into(),
                        }).await;
                        return Ok(DisconnectReason::NewDevice(new_addr));
                    }
                    BleCommand::SendNotification { title, body } => {
                        if let Some(ref chr) = alert_chr {
                            let msg = build_alert_message(0x00, &title, &body);
                            let _ = chr.write(&msg).await;
                        }
                    }
                    BleCommand::Shutdown => {
                        let _ = device.disconnect().await;
                        return Ok(DisconnectReason::Shutdown);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Helper: await next item from an Option<Pin<Box<Stream>>>, or pend forever if None.
async fn next_or_pending(
    stream: &mut Option<std::pin::Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>>,
) -> Vec<u8> {
    match stream.as_mut() {
        Some(s) => match s.next().await {
            Some(v) => v,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

async fn discover_characteristics(
    device: &Device,
) -> Result<HashMap<Uuid, bluer::gatt::remote::Characteristic>> {
    // Wait for services to be resolved
    for _ in 0..50 {
        if device.is_services_resolved().await? {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let mut map = HashMap::new();
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

fn build_alert_message(category: u8, title: &str, body: &str) -> Vec<u8> {
    let mut msg = vec![category, 1];
    msg.extend_from_slice(title.as_bytes());
    msg.push(0x00);
    msg.extend_from_slice(body.as_bytes());
    msg
}
