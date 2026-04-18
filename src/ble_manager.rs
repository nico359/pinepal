// SPDX-License-Identifier: GPL-3.0-or-later
// BLE connection manager for InfiniTime watches.
// Handles discovery, connection, characteristic I/O, and reconnection with backoff.

use anyhow::{anyhow, Result};
use bluer::{Adapter, AdapterEvent, AdapterProperty, Address, Device};
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicRead, Service,
};
use chrono::{Datelike, Local, Timelike};
use futures::FutureExt;
use futures::StreamExt;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Duration};
use uuid::Uuid;

// Standard BLE UUIDs
const SRV_CURRENT_TIME: Uuid = uuid::uuid!("00001805-0000-1000-8000-00805f9b34fb");
const CHR_CURRENT_TIME: Uuid = uuid::uuid!("00002a2b-0000-1000-8000-00805f9b34fb");
const CHR_BATTERY: Uuid = uuid::uuid!("00002a19-0000-1000-8000-00805f9b34fb");
const CHR_FIRMWARE_REV: Uuid = uuid::uuid!("00002a26-0000-1000-8000-00805f9b34fb");
const CHR_HEART_RATE: Uuid = uuid::uuid!("00002a37-0000-1000-8000-00805f9b34fb");
const CHR_NEW_ALERT: Uuid = uuid::uuid!("00002a46-0000-1000-8000-00805f9b34fb");

// InfiniTime custom UUIDs
const CHR_STEP_COUNT: Uuid = uuid::uuid!("00030001-78fc-48fe-8e23-433b3a1942d0");

/// Starts a local GATT server advertising the Current Time Service (CTS).
/// InfiniTime reads this characteristic on connect to sync its clock.
/// The returned handle must be kept alive to keep the service registered.
async fn start_current_time_service(adapter: &Adapter) -> bluer::Result<ApplicationHandle> {
    let app = Application {
        services: vec![Service {
            uuid: SRV_CURRENT_TIME,
            primary: true,
            characteristics: vec![Characteristic {
                uuid: CHR_CURRENT_TIME,
                read: Some(CharacteristicRead {
                    read: true,
                    fun: Box::new(move |_req| {
                        async move {
                            let now = Local::now();
                            let year = (now.year() as u16).to_le_bytes();
                            let payload = vec![
                                year[0],
                                year[1],
                                now.month() as u8,
                                now.day() as u8,
                                now.hour() as u8,
                                now.minute() as u8,
                                now.second() as u8,
                                now.weekday().number_from_monday() as u8,
                                0x00, // Fractions256
                                0x00, // Adjust reason
                            ];
                            log::debug!("CTS read: {:?}", payload);
                            Ok(payload)
                        }
                        .boxed()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    adapter.serve_gatt_application(app).await
}

// Reconnection parameters
const BASE_DELAY_SECS: u64 = 1;
const MAX_DELAY_SECS: u64 = 60;
const CONNECT_TIMEOUT_SECS: u64 = 15;

const DEVICE_NAME: &str = "InfiniTime";

/// Waits until the Bluetooth adapter is powered on.
/// Sends `BleEvent::BluetoothOff` while waiting so the UI can inform the user.
/// Returns `false` if a shutdown command is received and the task should exit.
async fn wait_for_bluetooth_on(
    adapter: &Adapter,
    tx: &mpsc::Sender<BleEvent>,
    rx: &mut mpsc::Receiver<BleCommand>,
) -> bool {
    if adapter.is_powered().await.unwrap_or(true) {
        return true;
    }

    log::warn!("Bluetooth is off — waiting for user to enable it");
    let _ = tx.send(BleEvent::BluetoothOff).await;

    let events = match adapter.events().await {
        Ok(e) => e,
        Err(e) => {
            log::error!("Cannot subscribe to adapter events: {e} — falling back to polling");
            // Poll every 2 seconds as a fallback
            loop {
                sleep(Duration::from_secs(2)).await;
                match adapter.is_powered().await {
                    Ok(true) => {
                        log::info!("Bluetooth turned on");
                        let _ = tx.send(BleEvent::BluetoothReady).await;
                        return true;
                    }
                    Ok(false) => {}
                    Err(_) => return true, // can't tell, just proceed
                }
            }
        }
    };

    tokio::pin!(events);
    loop {
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(AdapterEvent::PropertyChanged(AdapterProperty::Powered(true))) => {
                        log::info!("Bluetooth turned on — resuming");
                        let _ = tx.send(BleEvent::BluetoothReady).await;
                        return true;
                    }
                    None => {
                        log::warn!("Adapter event stream ended while waiting for Bluetooth");
                        return true;
                    }
                    _ => {}
                }
            }
            cmd = rx.recv() => {
                match cmd {
                    Some(BleCommand::Shutdown) | None => return false,
                    _ => {}
                }
            }
        }
    }
}

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
    BluetoothOff,
    BluetoothReady,
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
    log::info!("BLE task started");

    let session = match bluer::Session::new().await {
        Ok(s) => s,
        Err(e) => {
            log::error!("Bluetooth session init failed: {e}");
            let _ = tx.send(BleEvent::Error(format!("Bluetooth init failed: {e}"))).await;
            return;
        }
    };
    let adapter = match session.default_adapter().await {
        Ok(a) => {
            log::info!(
                "Using Bluetooth adapter: {} (addr {})",
                a.name(),
                a.address().await.unwrap_or_default()
            );
            a
        }
        Err(e) => {
            log::error!("No Bluetooth adapter available: {e}");
            let _ = tx.send(BleEvent::Error(format!("No Bluetooth adapter: {e}"))).await;
            return;
        }
    };

    // Wait for Bluetooth to be powered on before proceeding.
    if !wait_for_bluetooth_on(&adapter, &tx, &mut rx).await {
        return; // shutdown requested while waiting
    }
    log::info!("Adapter is powered on");

    // Start the local Current Time Service so InfiniTime can sync its clock on connect.
    let _cts_handle = match start_current_time_service(&adapter).await {
        Ok(h) => {
            log::info!("Current Time Service registered (watch will sync time on connect)");
            Some(h)
        }
        Err(e) => {
            log::warn!("Failed to register Current Time Service: {e} (time sync unavailable)");
            None
        }
    };

    let mut auto_addr: Option<Address> = None;
    let mut attempts: u32 = 0;
    let mut user_disconnected = false;
    let mut needs_rescan = false;

    loop {
        // If we should auto-reconnect, do so after backoff
        if let (Some(addr), false) = (auto_addr, user_disconnected) {
            if attempts > 0 {
                let delay = reconnect_delay(attempts);
                log::info!("Reconnect attempt {attempts} — waiting {delay}s before retrying {addr}");
                let _ = tx.send(BleEvent::Reconnecting { attempt: attempts, delay_secs: delay }).await;
                // Wait for delay OR a user command
                tokio::select! {
                    _ = sleep(Duration::from_secs(delay)) => {
                        log::debug!("Reconnect backoff elapsed, proceeding");
                    }
                    Some(cmd) = rx.recv() => {
                        match cmd {
                            BleCommand::Disconnect => {
                                log::info!("User cancelled reconnect");
                                auto_addr = None;
                                user_disconnected = true;
                                attempts = 0;
                                needs_rescan = false;
                                let _ = tx.send(BleEvent::Disconnected { reason: "User cancelled".into() }).await;
                                continue;
                            }
                            BleCommand::Shutdown => {
                                log::info!("BLE task shutting down during reconnect wait");
                                return;
                            }
                            BleCommand::Connect(new_addr) => {
                                log::info!("User requested new device {new_addr} during reconnect wait");
                                auto_addr = Some(new_addr);
                                attempts = 0;
                                needs_rescan = false;
                                user_disconnected = false;
                                continue;
                            }
                            _ => {}
                        }
                    }
                }
            }

            // BlueZ evicts device D-Bus objects after repeated failures. Re-scan now (after
            // the backoff) so the object is fresh when we immediately attempt to connect.
            if needs_rescan {
                log::info!("BlueZ dropped device object for {addr} — rescanning to refresh cache");
                let _ = do_scan(&adapter, &tx).await;
                needs_rescan = false;
            }

            // If Bluetooth was turned off, wait for it to come back before connecting.
            if !wait_for_bluetooth_on(&adapter, &tx, &mut rx).await {
                return;
            }

            log::info!("Connecting to {addr} (attempt {})", attempts + 1);

            // Attempt connection
            match do_connect(&adapter, addr, &tx, &mut rx).await {
                Ok(DisconnectReason::UserRequested) => {
                    log::info!("Disconnected by user request");
                    auto_addr = None;
                    user_disconnected = true;
                    attempts = 0;
                }
                Ok(DisconnectReason::Shutdown) => {
                    log::info!("BLE task shutting down");
                    return;
                }
                Ok(DisconnectReason::NewDevice(new_addr)) => {
                    log::info!("Switching to new device {new_addr}");
                    auto_addr = Some(new_addr);
                    attempts = 0;
                    user_disconnected = false;
                }
                Err(e) => {
                    log::warn!("Connection attempt {} failed: {e}", attempts + 1);
                    if e.to_string().contains("not present or removed") {
                        needs_rescan = true;
                    }
                    attempts += 1;
                }
            }
            continue;
        }

        // Idle — wait for a command
        log::debug!("BLE task idle, waiting for command");
        match rx.recv().await {
            Some(BleCommand::StartScan) => {
                log::info!("Starting BLE scan");
                let _ = tx.send(BleEvent::Scanning).await;
                if let Err(e) = do_scan(&adapter, &tx).await {
                    log::error!("Scan error: {e}");
                    let _ = tx.send(BleEvent::Error(format!("Scan error: {e}"))).await;
                }
            }
            Some(BleCommand::Connect(addr)) => {
                log::info!("User requested connect to {addr}");
                auto_addr = Some(addr);
                attempts = 0;
                needs_rescan = false;
                user_disconnected = false;
            }
            Some(BleCommand::Shutdown) => {
                log::info!("BLE task received shutdown");
                return;
            }
            Some(_) => {}
            None => {
                log::warn!("BLE command channel closed, task exiting");
                return;
            }
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
    log::debug!("Setting LE discovery filter");
    let filter = bluer::DiscoveryFilter {
        transport: bluer::DiscoveryTransport::Le,
        ..Default::default()
    };
    adapter.set_discovery_filter(filter).await?;

    log::info!("Discovery started (10 s window)");
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
                    let name = device.name().await.ok().flatten().unwrap_or_default();
                    log::debug!("Discovered device {addr}: '{name}'");
                    if name == DEVICE_NAME {
                        let rssi = device.rssi().await.ok().flatten();
                        log::info!("Found InfiniTime at {addr} (RSSI: {rssi:?})");
                        found += 1;
                        let _ = tx.send(BleEvent::DeviceFound { address: addr, name, rssi }).await;
                    }
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    log::info!("Discovery finished — found {found} InfiniTime device(s)");
    Ok(())
}

async fn do_connect(
    adapter: &Adapter,
    addr: Address,
    tx: &mpsc::Sender<BleEvent>,
    rx: &mut mpsc::Receiver<BleCommand>,
) -> Result<DisconnectReason> {
    let device = adapter.device(addr)?;

    // Subscribe to adapter events early so we detect BT being turned off.
    let mut adapter_events = adapter.events().await?;

    log::info!("Initiating connection to {addr} (timeout {}s)", CONNECT_TIMEOUT_SECS);

    // Connect with timeout
    match timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), device.connect()).await {
        Err(_) => {
            log::warn!("Connection to {addr} timed out after {}s", CONNECT_TIMEOUT_SECS);
            return Err(anyhow!("Connection timed out"));
        }
        Ok(Err(e)) => {
            log::warn!("Connection to {addr} failed: {e}");
            return Err(anyhow!("Connection failed: {e}"));
        }
        Ok(Ok(())) => {
            log::info!("TCP/ACL link established to {addr}");
        }
    }

    log::debug!("Waiting for GATT service resolution on {addr}");
    // Discover characteristics
    let chars = discover_characteristics(&device).await?;
    log::info!(
        "Discovered {} GATT characteristics on {addr}",
        chars.len()
    );

    // Read firmware version
    let firmware = match chars.get(&CHR_FIRMWARE_REV) {
        Some(chr) => {
            let data = chr.read().await.unwrap_or_default();
            String::from_utf8(data).unwrap_or_else(|_| "Unknown".into())
        }
        None => "Unknown".into(),
    };
    log::info!("Firmware version: {firmware}");

    let _ = tx.send(BleEvent::Connected { address: addr, firmware }).await;

    // Initial reads
    if let Some(chr) = chars.get(&CHR_BATTERY) {
        match chr.read().await {
            Ok(data) => {
                if let Some(&val) = data.first() {
                    log::debug!("Initial battery level: {val}%");
                    let _ = tx.send(BleEvent::BatteryLevel(val)).await;
                }
            }
            Err(e) => log::warn!("Failed to read battery: {e}"),
        }
    }
    if let Some(chr) = chars.get(&CHR_HEART_RATE) {
        if let Ok(data) = chr.read().await {
            if let Some(&val) = data.get(1) {
                log::debug!("Initial heart rate: {val} bpm");
                let _ = tx.send(BleEvent::HeartRate(val)).await;
            }
        }
    }
    if let Some(chr) = chars.get(&CHR_STEP_COUNT) {
        if let Ok(data) = chr.read().await {
            if let Ok(bytes) = <[u8; 4]>::try_from(data.as_slice()) {
                let steps = u32::from_le_bytes(bytes);
                log::debug!("Initial step count: {steps}");
                let _ = tx.send(BleEvent::StepCount(steps)).await;
            }
        }
    }

    // Start notify streams (boxed since bluer streams aren't Unpin)
    let mut battery_stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>> =
        if let Some(chr) = chars.get(&CHR_BATTERY) {
            match chr.notify().await {
                Ok(s) => { log::debug!("Battery notify subscribed"); Some(Box::pin(s) as _) }
                Err(e) => { log::warn!("Battery notify failed: {e}"); None }
            }
        } else { None };
    let mut hr_stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>> =
        if let Some(chr) = chars.get(&CHR_HEART_RATE) {
            match chr.notify().await {
                Ok(s) => { log::debug!("Heart rate notify subscribed"); Some(Box::pin(s) as _) }
                Err(e) => { log::warn!("Heart rate notify failed: {e}"); None }
            }
        } else { None };
    let mut step_stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>> =
        if let Some(chr) = chars.get(&CHR_STEP_COUNT) {
            match chr.notify().await {
                Ok(s) => { log::debug!("Step count notify subscribed"); Some(Box::pin(s) as _) }
                Err(e) => { log::warn!("Step count notify failed: {e}"); None }
            }
        } else { None };

    let alert_chr = chars.get(&CHR_NEW_ALERT).cloned();

    log::debug!("Subscribing to device property events on {addr}");
    // Monitor device for disconnect
    let mut prop_stream = device.events().await?;

    log::info!("Connected and streaming data from {addr}");

    // Connected event loop
    loop {
        tokio::select! {
            val = next_or_pending(&mut battery_stream) => {
                if let Some(&v) = val.first() {
                    log::debug!("Battery update: {v}%");
                    let _ = tx.send(BleEvent::BatteryLevel(v)).await;
                }
            }
            val = next_or_pending(&mut hr_stream) => {
                if let Some(&v) = val.get(1) {
                    log::debug!("Heart rate update: {v} bpm");
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
                        log::warn!("Device {addr} reported Connected=false via property stream");
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "Watch disconnected".into(),
                        }).await;
                        return Err(anyhow!("Device disconnected"));
                    }
                    Some(bluer::DeviceEvent::PropertyChanged(prop)) => {
                        log::debug!("Device property changed: {prop:?}");
                    }
                    None => {
                        log::warn!("Property event stream for {addr} ended unexpectedly");
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "Connection lost".into(),
                        }).await;
                        return Err(anyhow!("Property stream ended"));
                    }
                }
            }
            Some(cmd) = rx.recv() => {
                match cmd {
                    BleCommand::Disconnect => {
                        log::info!("User requested disconnect from {addr}");
                        let _ = device.disconnect().await;
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "User disconnected".into(),
                        }).await;
                        return Ok(DisconnectReason::UserRequested);
                    }
                    BleCommand::Connect(new_addr) => {
                        log::info!("Switching device from {addr} to {new_addr}");
                        let _ = device.disconnect().await;
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "Switching device".into(),
                        }).await;
                        return Ok(DisconnectReason::NewDevice(new_addr));
                    }
                    BleCommand::SendNotification { title, body } => {
                        log::debug!("Sending alert: '{title}'");
                        if let Some(ref chr) = alert_chr {
                            let msg = build_alert_message(0x00, &title, &body);
                            if let Err(e) = chr.write(&msg).await {
                                log::warn!("Alert write failed: {e}");
                            }
                        }
                    }
                    BleCommand::Shutdown => {
                        log::info!("Shutdown requested while connected to {addr}");
                        let _ = device.disconnect().await;
                        return Ok(DisconnectReason::Shutdown);
                    }
                    _ => {}
                }
            }
            evt = adapter_events.next() => {
                match evt {
                    Some(AdapterEvent::PropertyChanged(AdapterProperty::Powered(false))) => {
                        log::warn!("Bluetooth turned off while connected to {addr}");
                        let _ = tx.send(BleEvent::BluetoothOff).await;
                        return Err(anyhow!("Bluetooth adapter powered off"));
                    }
                    None => {
                        log::warn!("Adapter event stream ended while connected to {addr}");
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "Bluetooth unavailable".into(),
                        }).await;
                        return Err(anyhow!("Adapter event stream ended"));
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
    // Wait for services to be resolved (up to 5 s total, 100 ms steps).
    let mut resolved = false;
    for i in 0..50 {
        match device.is_services_resolved().await {
            Ok(true) => {
                log::debug!("GATT services resolved after {}ms", i * 100);
                resolved = true;
                break;
            }
            Ok(false) => {}
            Err(e) => log::warn!("is_services_resolved error: {e}"),
        }
        sleep(Duration::from_millis(100)).await;
    }
    if !resolved {
        log::warn!("Services not resolved after 5s — proceeding anyway");
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
