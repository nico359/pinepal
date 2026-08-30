// SPDX-License-Identifier: GPL-3.0-or-later
// BLE connection manager for InfiniTime watches.
// Handles discovery, connection, characteristic I/O, and reconnection with backoff.

use anyhow::{anyhow, Context, Result};
use bluer::agent::{Agent, ReqError, RequestConfirmation, RequestPasskey};
use bluer::{Adapter, AdapterEvent, AdapterProperty, Address, Device};
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicRead, Service,
};
use chrono::{Datelike, Local, Timelike};
use futures::FutureExt;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, timeout, Duration};
use uuid::Uuid;

use crate::weather::WeatherData;

// Standard BLE UUIDs
const SRV_CURRENT_TIME: Uuid = uuid::uuid!("00001805-0000-1000-8000-00805f9b34fb");
const CHR_CURRENT_TIME: Uuid = uuid::uuid!("00002a2b-0000-1000-8000-00805f9b34fb");
const CHR_BATTERY: Uuid = uuid::uuid!("00002a19-0000-1000-8000-00805f9b34fb");
const CHR_FIRMWARE_REV: Uuid = uuid::uuid!("00002a26-0000-1000-8000-00805f9b34fb");
const CHR_HEART_RATE: Uuid = uuid::uuid!("00002a37-0000-1000-8000-00805f9b34fb");
const CHR_NEW_ALERT: Uuid = uuid::uuid!("00002a46-0000-1000-8000-00805f9b34fb");

// InfiniTime custom UUIDs
const CHR_STEP_COUNT: Uuid = uuid::uuid!("00030001-78fc-48fe-8e23-433b3a1942d0");
const CHR_SIMPLE_WEATHER: Uuid = uuid::uuid!("00050001-78fc-48fe-8e23-433b3a1942d0");
/// InfiniTime Alert Notification Service: reports the watch's response
/// (answer/reject/mute) to an incoming-call alert.
const CHR_NOTIFICATION_EVENT: Uuid = uuid::uuid!("00020001-78fc-48fe-8e23-433b3a1942d0");

/// ANS category that makes InfiniTime show the answer/reject call screen.
const ANS_CATEGORY_INCOMING_CALL: u8 = 0x03;

// Nordic Legacy DFU service UUIDs
const CHR_DFU_CONTROL_POINT: Uuid = uuid::uuid!("00001531-1212-efde-1523-785feabcd123");
const CHR_DFU_PACKET: Uuid = uuid::uuid!("00001532-1212-efde-1523-785feabcd123");

/// Minimum battery level required before starting a flash — a dead battery
/// mid-DFU can leave the watch unbootable.
const MIN_DFU_BATTERY: u8 = 30;

// InfiniTime Media Player service UUIDs
const CHR_MP_EVENTS: Uuid = uuid::uuid!("00000001-78fc-48fe-8e23-433b3a1942d0");
const CHR_MP_STATUS: Uuid = uuid::uuid!("00000002-78fc-48fe-8e23-433b3a1942d0");
const CHR_MP_ARTIST: Uuid = uuid::uuid!("00000003-78fc-48fe-8e23-433b3a1942d0");
const CHR_MP_TRACK: Uuid = uuid::uuid!("00000004-78fc-48fe-8e23-433b3a1942d0");
const CHR_MP_ALBUM: Uuid = uuid::uuid!("00000005-78fc-48fe-8e23-433b3a1942d0");
const CHR_MP_POSITION: Uuid = uuid::uuid!("00000006-78fc-48fe-8e23-433b3a1942d0");
const CHR_MP_DURATION: Uuid = uuid::uuid!("00000007-78fc-48fe-8e23-433b3a1942d0");
const CHR_MP_SPEED: Uuid = uuid::uuid!("0000000a-78fc-48fe-8e23-433b3a1942d0");
const CHR_MP_REPEAT: Uuid = uuid::uuid!("0000000b-78fc-48fe-8e23-433b3a1942d0");
const CHR_MP_SHUFFLE: Uuid = uuid::uuid!("0000000c-78fc-48fe-8e23-433b3a1942d0");

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
    emit_ready: bool,
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
                        if emit_ready {
                            let _ = tx.send(BleEvent::BluetoothReady).await;
                        }
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
                        if emit_ready {
                            let _ = tx.send(BleEvent::BluetoothReady).await;
                        }
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
    FirmwareVersion(String),
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
    MediaPlayerEvent(MediaPlayerEvent),
    /// 0..=100 progress of an in-progress firmware flash.
    FirmwareUpdateProgress(u8),
    /// Human-readable firmware update status ("flashing", "rebooting", "failed: ...").
    FirmwareUpdateStatus(String),
    /// The watch is displaying a 6-digit pairing code; the UI should prompt for it.
    PasskeyRequested,
}

/// Media player button event from the watch.
#[derive(Debug, Clone)]
pub enum MediaPlayerEvent {
    AppOpened,
    Play,
    Pause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
}

impl MediaPlayerEvent {
    pub fn from_raw(v: u8) -> Option<Self> {
        match v {
            0xe0 => Some(MediaPlayerEvent::AppOpened),
            0x00 => Some(MediaPlayerEvent::Play),
            0x01 => Some(MediaPlayerEvent::Pause),
            0x03 => Some(MediaPlayerEvent::Next),
            0x04 => Some(MediaPlayerEvent::Previous),
            0x05 => Some(MediaPlayerEvent::VolumeUp),
            0x06 => Some(MediaPlayerEvent::VolumeDown),
            _ => None,
        }
    }
}

/// A response to an incoming call, pressed on the watch.
#[derive(Clone, Copy, Debug)]
pub enum CallAction {
    Answer,
    Reject,
    Mute,
}

impl CallAction {
    /// Map an InfiniTime Notification Event byte to a call action.
    pub fn from_event(byte: u8) -> Option<CallAction> {
        match byte {
            0x00 => Some(CallAction::Reject),
            0x01 => Some(CallAction::Answer),
            0x02 => Some(CallAction::Mute),
            _ => None,
        }
    }
}

/// Media track info sent to the watch's media player service.
#[derive(Debug, Default, Clone)]
pub struct MpInfo {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<String>,
    pub playing: Option<bool>,
    pub position: Option<u32>,
    pub duration: Option<u32>,
    pub speed: Option<f32>,
    pub repeat: Option<bool>,
    pub shuffle: Option<bool>,
}

/// Commands sent from UI to BLE manager.
#[derive(Debug)]
pub enum BleCommand {
    StartScan,
    Connect(Address),
    Disconnect,
    SendNotification { title: String, body: String },
    /// Re-read all characteristic values and re-emit the corresponding events.
    /// Used when the GUI opens and takes over an already-connected service session.
    RequestUpdate,
    Shutdown,
    /// Send media player info to the watch.
    SendMpInfo(MpInfo),
    /// Push weather data to the watch's Simple Weather Service.
    SendWeather(WeatherData),
    /// Flash a downloaded DFU package to the watch.
    InstallFirmware(crate::updater::DfuPackage),
    /// Show an incoming call on the watch (answer/reject screen).
    IncomingCall { name: String, number: String },
}

/// Shared slot resolving an in-flight pairing passkey request. Accessed
/// directly (not via BleCommand) because the BLE task is blocked inside
/// `device.pair()` while waiting for the code — a queued command would deadlock.
type PasskeySlot = Arc<Mutex<Option<oneshot::Sender<u32>>>>;

/// Handle for sending commands to the BLE task from the UI (glib) thread.
#[derive(Clone, Debug)]
pub struct BleHandle {
    cmd_tx: mpsc::Sender<BleCommand>,
    passkey_slot: PasskeySlot,
}

impl BleHandle {
    /// Send a command to the BLE manager. Non-blocking, drops if full.
    pub fn send(&self, cmd: BleCommand) {
        let _ = self.cmd_tx.try_send(cmd);
    }

    /// Provide the passkey shown on the watch to an in-flight pairing request.
    pub fn provide_passkey(&self, code: u32) {
        if let Some(tx) = self.passkey_slot.lock().unwrap().take() {
            let _ = tx.send(code);
        }
    }

    /// Cancel an in-flight passkey request (user dismissed the dialog).
    pub fn cancel_passkey(&self) {
        // Dropping the sender makes the agent answer Canceled to BlueZ.
        let _ = self.passkey_slot.lock().unwrap().take();
    }
}

/// Spawn the BLE manager on the given tokio runtime.
/// Returns a command handle and a receiver for BLE events.
/// Watch-pressed call actions (answer/reject/mute) are forwarded to `call_action_tx`.
pub fn spawn(
    rt: &tokio::runtime::Runtime,
    call_action_tx: mpsc::Sender<CallAction>,
) -> (BleHandle, mpsc::Receiver<BleEvent>) {
    let (event_tx, event_rx) = mpsc::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let passkey_slot = PasskeySlot::default();
    rt.spawn(ble_task(event_tx, cmd_rx, passkey_slot.clone(), call_action_tx));
    (BleHandle { cmd_tx, passkey_slot }, event_rx)
}

/// Main BLE task — runs on tokio, manages state machine.
async fn ble_task(
    tx: mpsc::Sender<BleEvent>,
    mut rx: mpsc::Receiver<BleCommand>,
    passkey_slot: PasskeySlot,
    call_action_tx: mpsc::Sender<CallAction>,
) {
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
    if !wait_for_bluetooth_on(&adapter, &tx, &mut rx, true).await {
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

    // KeyboardDisplay pairing agent: InfiniTime shows a 6-digit code, the user
    // types it into the app. Without this the system agent negotiates Just-Works
    // (NoInputNoOutput), whose bonds reconnect unreliably on InfiniTime.
    let _agent = match session
        .register_agent(make_agent(passkey_slot, tx.clone()))
        .await
    {
        Ok(h) => Some(h),
        Err(e) => {
            log::warn!("Could not register pairing agent: {e} — relying on system agent");
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
                            BleCommand::StartScan => {
                                log::info!("User requested device scan, cancelling reconnect");
                                auto_addr = None;
                                user_disconnected = true;
                                attempts = 0;
                                needs_rescan = false;
                                let _ = tx.send(BleEvent::Disconnected { reason: "User requested scan".into() }).await;
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
            // Don't emit BluetoothReady — the Reconnecting event below handles the UI.
            if !wait_for_bluetooth_on(&adapter, &tx, &mut rx, false).await {
                return;
            }

            log::info!("Connecting to {addr} (attempt {})", attempts + 1);

            // Tell the UI we're now actively attempting a connection.
            let _ = tx.send(BleEvent::Reconnecting { attempt: attempts + 1, delay_secs: 0 }).await;

            // Attempt connection
            match do_connect(&adapter, addr, &tx, &mut rx, &call_action_tx).await {
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
                    if e.to_string().contains("pairing failed") {
                        // Pairing failed or was cancelled — don't silently fall
                        // through to an unbonded connection (unreliable on
                        // InfiniTime, and looks like pairing succeeded). Stop
                        // auto-reconnecting; the next explicit user Connect
                        // prompts again.
                        auto_addr = None;
                        user_disconnected = true;
                        attempts = 0;
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "Pairing failed or cancelled".into(),
                        }).await;
                    } else {
                        attempts += 1;
                    }
                    if e.to_string().contains("not present or removed") {
                        needs_rescan = true;
                    }
                    // ponytail: BlueZ GATT cache poisoning — remove stale device object
                    // so the next scan+connect gets a fresh service discovery.
                    if e.to_string().contains("No characteristics found") {
                        log::info!("Removing stale device object for {addr} to clear GATT cache");
                        let _ = adapter.remove_device(addr).await;
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
    call_action_tx: &mpsc::Sender<CallAction>,
) -> Result<DisconnectReason> {
    let device = adapter.device(addr)?;

    // Subscribe to adapter events early so we detect BT being turned off.
    let mut adapter_events = adapter.events().await?;

    // InfiniTime needs a bonded (encrypted) link to reconnect reliably; an
    // unbonded GATT connection drops out. Pair first if we have no bond —
    // this drives the passkey prompt via our agent.
    if !device.is_paired().await.unwrap_or(false) {
        log::info!("Not bonded to {addr} — starting secure pairing");
        device.pair().await.context("pairing failed")?;
        let _ = device.set_trusted(true).await;
        log::info!("Bonded with {addr}");
    }

    log::info!("Initiating connection to {addr} (timeout {}s)", CONNECT_TIMEOUT_SECS);

    // Connect with timeout, cancellable by Disconnect/Shutdown commands.
    let connect_fut = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), device.connect());
    tokio::pin!(connect_fut);
    let connect_result = loop {
        tokio::select! {
            result = &mut connect_fut => break result,
            Some(cmd) = rx.recv() => {
                match cmd {
                    BleCommand::Disconnect => {
                        log::info!("User cancelled connection attempt to {addr}");
                        let _ = tx.send(BleEvent::Disconnected { reason: "User cancelled".into() }).await;
                        return Ok(DisconnectReason::UserRequested);
                    }
                    BleCommand::StartScan => {
                        log::info!("User requested scan, aborting connection attempt to {addr}");
                        let _ = tx.send(BleEvent::Disconnected { reason: "User requested scan".into() }).await;
                        return Ok(DisconnectReason::UserRequested);
                    }
                    BleCommand::Shutdown => {
                        log::info!("Shutdown during connection attempt to {addr}");
                        return Ok(DisconnectReason::Shutdown);
                    }
                    _ => {} // other commands are ignored while connecting
                }
            }
        }
    };
    match connect_result {
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

    // Show the dashboard immediately with a placeholder — real data arrives below.
    let _ = tx.send(BleEvent::Connected { address: addr, firmware: "…".into() }).await;

    // Read firmware, battery, heart rate and step count in parallel.
    let fw_chr    = chars.get(&CHR_FIRMWARE_REV).cloned();
    let bat_chr   = chars.get(&CHR_BATTERY).cloned();
    let hr_chr    = chars.get(&CHR_HEART_RATE).cloned();
    let steps_chr = chars.get(&CHR_STEP_COUNT).cloned();

    let (firmware, battery, hr, steps) = tokio::join!(
        async move {
            let data = fw_chr?.read().await.ok()?;
            String::from_utf8(data).ok().filter(|s| !s.is_empty())
        },
        async move {
            let data = bat_chr?.read().await.ok()?;
            data.first().copied()
        },
        async move {
            let data = hr_chr?.read().await.ok()?;
            data.get(1).copied()
        },
        async move {
            let data = steps_chr?.read().await.ok()?;
            <[u8; 4]>::try_from(data.as_slice()).ok().map(u32::from_le_bytes)
        },
    );

    let fw = firmware.unwrap_or_else(|| "Unknown".into());
    log::info!("Firmware version: {fw}");
    let _ = tx.send(BleEvent::FirmwareVersion(fw)).await;
    // Tracked locally (not just forwarded) so InstallFirmware can gate on it.
    let mut current_battery = battery;
    if let Some(level) = battery {
        log::debug!("Initial battery level: {level}%");
        let _ = tx.send(BleEvent::BatteryLevel(level)).await;
    }
    if let Some(bpm) = hr {
        log::debug!("Initial heart rate: {bpm} bpm");
        let _ = tx.send(BleEvent::HeartRate(bpm)).await;
    }
    if let Some(count) = steps {
        log::debug!("Initial step count: {count}");
        let _ = tx.send(BleEvent::StepCount(count)).await;
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

    // Media player event stream (button presses from the watch)
    let mut mp_event_stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>> =
        if let Some(chr) = chars.get(&CHR_MP_EVENTS) {
            match chr.notify().await {
                Ok(s) => { log::debug!("Media player events subscribed"); Some(Box::pin(s) as _) }
                Err(e) => { log::debug!("Media player events not available: {e}"); None }
            }
        } else { None };

    // Incoming-call response stream (answer/reject/mute pressed on the watch)
    let mut call_event_stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>> =
        if let Some(chr) = chars.get(&CHR_NOTIFICATION_EVENT) {
            match chr.notify().await {
                Ok(s) => { log::debug!("Call event notify subscribed"); Some(Box::pin(s) as _) }
                Err(e) => { log::debug!("Call event notify not available: {e}"); None }
            }
        } else { None };

    let alert_chr = chars.get(&CHR_NEW_ALERT).cloned();

    log::debug!("Subscribing to device property events on {addr}");
    // Monitor device for disconnect
    let mut prop_stream = device.events().await?;

    // Poll the link directly too — BlueZ property-change events aren't always
    // delivered, and missing a disconnect would wedge us "connected" (writes
    // start failing with Not connected while the dashboard looks fine).
    let mut health = tokio::time::interval(Duration::from_secs(3));
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    log::info!("Connected and streaming data from {addr}");

    // Connected event loop
    loop {
        tokio::select! {
            _ = health.tick() => {
                if !device.is_connected().await.unwrap_or(false) {
                    log::warn!("Device {addr} no longer connected (health poll)");
                    let _ = tx.send(BleEvent::Disconnected {
                        reason: "Watch disconnected".into(),
                    }).await;
                    return Err(anyhow!("Device disconnected"));
                }
            }
            val = next_or_pending(&mut battery_stream) => {
                if let Some(&v) = val.first() {
                    log::debug!("Battery update: {v}%");
                    current_battery = Some(v);
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
            val = next_or_pending(&mut mp_event_stream) => {
                if let Some(&v) = val.first() {
                    if let Some(evt) = MediaPlayerEvent::from_raw(v) {
                        log::debug!("Media player event: {:?}", evt);
                        let _ = tx.send(BleEvent::MediaPlayerEvent(evt)).await;
                    }
                }
            }
            val = next_or_pending(&mut call_event_stream) => {
                if let Some(&v) = val.first() {
                    if let Some(action) = CallAction::from_event(v) {
                        log::info!("Call action from watch: {:?}", action);
                        let _ = call_action_tx.try_send(action);
                    }
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
                    BleCommand::SendMpInfo(info) => {
                        write_mp_info(&chars, &info).await;
                    }
                    BleCommand::SendWeather(data) => {
                        if let Err(e) = write_weather(&chars, &data).await {
                            log::warn!("Weather write failed: {e}");
                        }
                    }
                    BleCommand::IncomingCall { name, number } => {
                        // Category "Call" makes InfiniTime show the answer/reject screen.
                        let (title, body) = if name.is_empty() {
                            (number.as_str(), "")
                        } else {
                            (name.as_str(), number.as_str())
                        };
                        if let Some(ref chr) = alert_chr {
                            let msg = build_alert_message(ANS_CATEGORY_INCOMING_CALL, title, body);
                            if let Err(e) = chr.write(&msg).await {
                                log::warn!("Call alert write failed: {e}");
                            }
                        }
                    }
                    BleCommand::InstallFirmware(package) => {
                        // ponytail: blocks this event loop for the ~1min transfer,
                        // same tradeoff pinetime-furios makes — a DFU-in-progress
                        // watch shouldn't be juggling other commands anyway.
                        install_firmware(&chars, &tx, current_battery, package).await;
                    }
                    BleCommand::RequestUpdate => {
                        log::info!("RequestUpdate: re-reading all characteristics for {addr}");
                        let fw_chr    = chars.get(&CHR_FIRMWARE_REV).cloned();
                        let bat_chr   = chars.get(&CHR_BATTERY).cloned();
                        let hr_chr    = chars.get(&CHR_HEART_RATE).cloned();
                        let steps_chr = chars.get(&CHR_STEP_COUNT).cloned();
                        let tx2 = tx.clone();
                        tokio::spawn(async move {
                            if let Some(chr) = fw_chr {
                                if let Ok(data) = chr.read().await {
                                    if let Ok(fw) = String::from_utf8(data) {
                                        let _ = tx2.send(BleEvent::FirmwareVersion(fw)).await;
                                    }
                                }
                            }
                            if let Some(chr) = bat_chr {
                                if let Ok(data) = chr.read().await {
                                    if let Some(&v) = data.first() {
                                        let _ = tx2.send(BleEvent::BatteryLevel(v)).await;
                                    }
                                }
                            }
                            if let Some(chr) = hr_chr {
                                if let Ok(data) = chr.read().await {
                                    if let Some(&v) = data.get(1) {
                                        let _ = tx2.send(BleEvent::HeartRate(v)).await;
                                    }
                                }
                            }
                            if let Some(chr) = steps_chr {
                                if let Ok(data) = chr.read().await {
                                    if let Ok(bytes) = <[u8; 4]>::try_from(data.as_slice()) {
                                        let _ = tx2.send(BleEvent::StepCount(u32::from_le_bytes(bytes))).await;
                                    }
                                }
                            }
                        });
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

/// Build a `KeyboardDisplay` pairing agent: `request_passkey` asks the UI for
/// the code the watch displays; `request_confirmation` auto-accepts the
/// numeric-comparison fallback. InfiniTime's secure pairing needs this — a
/// Just-Works bond (NoInputNoOutput) reconnects unreliably.
fn make_agent(passkey_slot: PasskeySlot, tx: mpsc::Sender<BleEvent>) -> Agent {
    Agent {
        request_default: true,
        request_passkey: Some(Box::new(move |req: RequestPasskey| {
            let passkey_slot = passkey_slot.clone();
            let tx = tx.clone();
            Box::pin(async move {
                log::info!(
                    "Watch {} is displaying a pairing code — awaiting entry from UI",
                    req.device
                );
                let (code_tx, code_rx) = oneshot::channel();
                *passkey_slot.lock().unwrap() = Some(code_tx);
                if tx.send(BleEvent::PasskeyRequested).await.is_err() {
                    return Err(ReqError::Canceled);
                }
                code_rx.await.map_err(|_| ReqError::Canceled)
            })
        })),
        request_confirmation: Some(Box::new(|req: RequestConfirmation| {
            Box::pin(async move {
                log::info!(
                    "Auto-confirming pairing code {} for {}",
                    req.passkey,
                    req.device
                );
                Ok(())
            })
        })),
        ..Default::default()
    }
}

fn build_alert_message(category: u8, title: &str, body: &str) -> Vec<u8> {
    let mut msg = vec![category, 1];
    msg.extend_from_slice(title.as_bytes());
    msg.push(0x00);
    msg.extend_from_slice(body.as_bytes());
    msg
}

/// Flash a firmware package to the watch, reporting progress/status via `tx`.
async fn install_firmware(
    chars: &HashMap<Uuid, bluer::gatt::remote::Characteristic>,
    tx: &mpsc::Sender<BleEvent>,
    battery: Option<u8>,
    package: crate::updater::DfuPackage,
) {
    let (Some(cp), Some(pkt)) = (chars.get(&CHR_DFU_CONTROL_POINT), chars.get(&CHR_DFU_PACKET)) else {
        let _ = tx.send(BleEvent::FirmwareUpdateStatus("failed: watch has no DFU service".into())).await;
        return;
    };
    if let Some(level) = battery {
        if level < MIN_DFU_BATTERY {
            let _ = tx
                .send(BleEvent::FirmwareUpdateStatus(format!(
                    "failed: battery {level}% too low (need {MIN_DFU_BATTERY}%)"
                )))
                .await;
            return;
        }
    }

    log::info!("Starting firmware flash to version {}", package.version);
    let _ = tx.send(BleEvent::FirmwareUpdateStatus("flashing".into())).await;

    let result = crate::dfu::run_dfu(cp, pkt, &package.bin, &package.dat, |p| {
        let _ = tx.try_send(BleEvent::FirmwareUpdateProgress(p));
    })
    .await;

    match result {
        Ok(()) => {
            log::info!("Firmware flashed; watch is rebooting");
            let _ = tx.send(BleEvent::FirmwareUpdateStatus("rebooting".into())).await;
        }
        Err(e) => {
            log::warn!("Firmware flash failed: {e:#}");
            let _ = tx.send(BleEvent::FirmwareUpdateStatus(format!("failed: {e}"))).await;
        }
    }
}

/// Encode and write weather to InfiniTime's Simple Weather Service: a Current
/// message (type 0) and, if available, a Forecast message (type 1). Layout
/// matches the firmware's `SimpleWeatherService` parser (temps int16 0.01°C LE).
async fn write_weather(
    chars: &HashMap<Uuid, bluer::gatt::remote::Characteristic>,
    w: &WeatherData,
) -> Result<()> {
    let Some(c) = chars.get(&CHR_SIMPLE_WEATHER) else {
        return Ok(());
    };

    // Current: [0]=type, [1]=version, [2..10]=timestamp, [10..12]=temp,
    // [12..14]=min, [14..16]=max, [16..48]=city(32B), [48]=icon.
    let mut current = Vec::with_capacity(49);
    current.push(0); // CurrentWeather
    current.push(0); // version 0 (no sunrise/sunset)
    current.extend_from_slice(&w.timestamp.to_le_bytes());
    current.extend_from_slice(&w.current_temp.to_le_bytes());
    current.extend_from_slice(&w.today_min.to_le_bytes());
    current.extend_from_slice(&w.today_max.to_le_bytes());
    let mut city = [0u8; 32];
    let name = w.location.as_bytes();
    let n = name.len().min(32);
    city[..n].copy_from_slice(&name[..n]);
    current.extend_from_slice(&city);
    current.push(w.current_icon);
    c.write(&current).await.context("writing current weather")?;

    // Forecast: [0]=type, [1]=version, [2..10]=timestamp, [10]=nbDays,
    // then 5 bytes/day: min(i16), max(i16), icon.
    if !w.forecast.is_empty() {
        let days = &w.forecast[..w.forecast.len().min(5)];
        let mut forecast = Vec::with_capacity(11 + days.len() * 5);
        forecast.push(1); // Forecast
        forecast.push(0); // version 0
        forecast.extend_from_slice(&w.timestamp.to_le_bytes());
        forecast.push(days.len() as u8);
        for day in days {
            forecast.extend_from_slice(&day.min.to_le_bytes());
            forecast.extend_from_slice(&day.max.to_le_bytes());
            forecast.push(day.icon);
        }
        c.write(&forecast).await.context("writing forecast")?;
    }
    Ok(())
}

async fn write_mp_info(
    chars: &HashMap<Uuid, bluer::gatt::remote::Characteristic>,
    info: &MpInfo,
) {
    if let (Some(ref chr), Some(ref val)) = (chars.get(&CHR_MP_ARTIST), &info.artist) {
        if let Err(e) = chr.write(val.as_bytes()).await { log::warn!("MP artist write failed: {e}"); }
    }
    if let (Some(ref chr), Some(ref val)) = (chars.get(&CHR_MP_ALBUM), &info.album) {
        if let Err(e) = chr.write(val.as_bytes()).await { log::warn!("MP album write failed: {e}"); }
    }
    if let (Some(ref chr), Some(ref val)) = (chars.get(&CHR_MP_TRACK), &info.track) {
        if let Err(e) = chr.write(val.as_bytes()).await { log::warn!("MP track write failed: {e}"); }
    }
    if let (Some(ref chr), Some(playing)) = (chars.get(&CHR_MP_STATUS), info.playing) {
        let val = [u8::from(playing)];
        if let Err(e) = chr.write(&val).await { log::warn!("MP status write failed: {e}"); }
    }
    if let (Some(ref chr), Some(pos)) = (chars.get(&CHR_MP_POSITION), info.position) {
        let val = pos.to_be_bytes();
        if let Err(e) = chr.write(&val).await { log::warn!("MP position write failed: {e}"); }
    }
    if let (Some(ref chr), Some(dur)) = (chars.get(&CHR_MP_DURATION), info.duration) {
        let val = dur.to_be_bytes();
        if let Err(e) = chr.write(&val).await { log::warn!("MP duration write failed: {e}"); }
    }
    if let (Some(ref chr), Some(speed)) = (chars.get(&CHR_MP_SPEED), info.speed) {
        let val = ((speed * 100.0) as u32).to_be_bytes();
        if let Err(e) = chr.write(&val).await { log::warn!("MP speed write failed: {e}"); }
    }
    if let (Some(ref chr), Some(repeat)) = (chars.get(&CHR_MP_REPEAT), info.repeat) {
        let val = [u8::from(repeat)];
        if let Err(e) = chr.write(&val).await { log::warn!("MP repeat write failed: {e}"); }
    }
    if let (Some(ref chr), Some(shuffle)) = (chars.get(&CHR_MP_SHUFFLE), info.shuffle) {
        let val = [u8::from(shuffle)];
        if let Err(e) = chr.write(&val).await { log::warn!("MP shuffle write failed: {e}"); }
    }
}
