// SPDX-License-Identifier: GPL-3.0-or-later
// MPRIS D-Bus integration — bridges watch media controls to desktop media players.

use anyhow::{Context, Result};
use futures::{stream, Stream, StreamExt};
use tokio::sync::mpsc;
use zbus::{zvariant, Connection, names::OwnedBusName};

use crate::ble_manager::{BleCommand, BleHandle, MediaPlayerEvent, MpInfo};

const VOLUME_STEP: f64 = 0.1;

#[derive(Debug, Clone)]
pub enum PlayersListEvent {
    PlayerAdded(String),
    PlayerRemoved(String),
}

pub async fn get_players_stream(
    conn: &Connection,
) -> Result<impl Stream<Item = PlayersListEvent>> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;

    let all_names = dbus.list_names().await?;
    let current: Vec<_> = all_names
        .into_iter()
        .filter_map(|n| {
            let full = n.as_str();
            if full == "org.freedesktop.DBus" {
                return None;
            }
            full.strip_prefix("org.mpris.MediaPlayer2.")
                .map(|short| PlayersListEvent::PlayerAdded(short.to_string()))
        })
        .collect();

    let dbus2 = zbus::fdo::DBusProxy::new(conn).await?;
    let name_events = dbus2
        .receive_name_owner_changed()
        .await?
        .filter_map(move |sig| {
            async move {
                let args = sig.args().ok()?;
                let full = args.name().as_str();
                if !full.starts_with("org.mpris.MediaPlayer2.") {
                    return None;
                }
                let short = full
                    .strip_prefix("org.mpris.MediaPlayer2.")?
                    .to_string();
                let has_old = args.old_owner().is_some();
                let has_new = args.new_owner().is_some();
                match (has_old, has_new) {
                    (true, false) => Some(PlayersListEvent::PlayerRemoved(short)),
                    (false, true) => Some(PlayersListEvent::PlayerAdded(short)),
                    _ => None,
                }
            }
        });

    Ok(stream::iter(current).chain(name_events))
}

pub async fn run_control_session(
    conn: &Connection,
    player_name: &str,
    ble: BleHandle,
    mut watch_events: mpsc::Receiver<MediaPlayerEvent>,
) -> Result<()> {
    let dest: OwnedBusName = format!("org.mpris.MediaPlayer2.{}", player_name)
        .try_into()
        .context("Invalid MPRIS player name")?;

    let proxy = zbus::Proxy::new(
        conn,
        &dest,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .await?;

    let identity_proxy = zbus::Proxy::new(
        conn,
        &dest,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2",
    )
    .await?;

    let identity: String = identity_proxy
        .get_property("Identity")
        .await
        .unwrap_or_else(|_| player_name.to_string());
    log::info!("Media Player Control session started for: {identity}");

    // Read initial capabilities
    let mut can_go_next = proxy.get_property::<bool>("CanGoNext").await.unwrap_or(false);
    let mut can_go_previous = proxy.get_property::<bool>("CanGoPrevious").await.unwrap_or(false);
    let mut can_play = proxy.get_property::<bool>("CanPlay").await.unwrap_or(false);
    let mut can_pause = proxy.get_property::<bool>("CanPause").await.unwrap_or(false);

    // Send initial state to watch
    send_full_player_state(&proxy, &ble).await;

    // Subscribe to property change streams
    let mut playback_status_stream = proxy.receive_property_changed::<String>("PlaybackStatus").await;
    let mut loop_status_stream = proxy.receive_property_changed::<String>("LoopStatus").await;
    let mut shuffle_stream = proxy.receive_property_changed::<bool>("Shuffle").await;
    let mut position_stream = proxy.receive_property_changed::<i64>("Position").await;
    let mut rate_stream = proxy.receive_property_changed::<f64>("Rate").await;
    let mut metadata_stream = proxy.receive_property_changed::<zvariant::OwnedValue>("Metadata").await;
    let mut can_go_next_stream = proxy.receive_property_changed::<bool>("CanGoNext").await;
    let mut can_go_previous_stream = proxy.receive_property_changed::<bool>("CanGoPrevious").await;
    let mut can_play_stream = proxy.receive_property_changed::<bool>("CanPlay").await;
    let mut can_pause_stream = proxy.receive_property_changed::<bool>("CanPause").await;

    loop {
        tokio::select! {
            Some(event) = watch_events.recv() => {
                let _result = handle_watch_event(&proxy, event, can_play, can_pause, can_go_next, can_go_previous).await;
            }
            Some(change) = playback_status_stream.next() => {
                if let Ok(status) = change.get().await {
                    let playing = status == "Playing";
                    log::debug!("Playback status: {status}");
                    ble.send(BleCommand::SendMpInfo(MpInfo { playing: Some(playing), ..Default::default() }));
                }
            }
            Some(change) = loop_status_stream.next() => {
                if let Ok(status) = change.get().await {
                    let repeat = status == "Track";
                    log::debug!("Loop status: {status}");
                    ble.send(BleCommand::SendMpInfo(MpInfo { repeat: Some(repeat), ..Default::default() }));
                }
            }
            Some(change) = shuffle_stream.next() => {
                if let Ok(shuffle) = change.get().await {
                    log::debug!("Shuffle: {shuffle}");
                    ble.send(BleCommand::SendMpInfo(MpInfo { shuffle: Some(shuffle), ..Default::default() }));
                }
            }
            Some(change) = position_stream.next() => {
                if let Ok(pos_us) = change.get().await {
                    let position = (pos_us / 1_000_000) as u32;
                    log::debug!("Position: {position}s");
                    ble.send(BleCommand::SendMpInfo(MpInfo { position: Some(position), ..Default::default() }));
                }
            }
            Some(change) = rate_stream.next() => {
                if let Ok(rate) = change.get().await {
                    log::debug!("Rate: {rate}");
                    ble.send(BleCommand::SendMpInfo(MpInfo { speed: Some(rate as f32), ..Default::default() }));
                }
            }
            Some(change) = metadata_stream.next() => {
                if let Ok(val) = change.get().await {
                    let info = parse_metadata(&val);
                    ble.send(BleCommand::SendMpInfo(info));
                }
            }
            Some(change) = can_go_next_stream.next() => {
                if let Ok(v) = change.get().await { can_go_next = v; }
            }
            Some(change) = can_go_previous_stream.next() => {
                if let Ok(v) = change.get().await { can_go_previous = v; }
            }
            Some(change) = can_play_stream.next() => {
                if let Ok(v) = change.get().await { can_play = v; }
            }
            Some(change) = can_pause_stream.next() => {
                if let Ok(v) = change.get().await { can_pause = v; }
            }
            else => break,
        }
    }

    log::info!("Media Player Control session ended for: {identity}");
    Ok(())
}

async fn send_full_player_state(proxy: &zbus::Proxy<'_>, ble: &BleHandle) {
    let mut info = MpInfo::default();

    if let Ok(status) = proxy.get_property::<String>("PlaybackStatus").await {
        info.playing = Some(status == "Playing");
    }
    if let Ok(repeat) = proxy.get_property::<String>("LoopStatus").await {
        info.repeat = Some(repeat == "Track");
    }
    if let Ok(shuffle) = proxy.get_property::<bool>("Shuffle").await {
        info.shuffle = Some(shuffle);
    }
    if let Ok(pos) = proxy.get_property::<i64>("Position").await {
        info.position = Some((pos / 1_000_000) as u32);
    }
    if let Ok(rate) = proxy.get_property::<f64>("Rate").await {
        info.speed = Some(rate as f32);
    }
    if let Ok(meta) = proxy.get_property::<zvariant::OwnedValue>("Metadata").await {
        let parsed = parse_metadata(&meta);
        info.artist = parsed.artist;
        info.album = parsed.album;
        info.track = parsed.track;
        info.duration = parsed.duration;
    }

    ble.send(BleCommand::SendMpInfo(info));
}

fn parse_metadata(value: &zvariant::OwnedValue) -> MpInfo {
    let mut info = MpInfo::default();

    let Ok(dict) = value.downcast_ref::<zvariant::Dict>() else {
        return info;
    };

    for (k, v) in dict.iter() {
        let Ok(k_str) = k.downcast_ref::<zvariant::Str>() else {
            continue;
        };
        match k_str.as_str() {
            "xesam:artist" => {
                if let Ok(arr) = v.downcast_ref::<zvariant::Array>() {
                    if let Ok(Some(first)) = arr.get::<String>(0) {
                        info.artist = Some(first);
                    }
                }
            }
            "xesam:album" => {
                if let Ok(s) = v.downcast_ref::<zvariant::Str>() {
                    info.album = Some(s.as_str().to_string());
                }
            }
            "xesam:title" => {
                if let Ok(s) = v.downcast_ref::<zvariant::Str>() {
                    info.track = Some(s.as_str().to_string());
                }
            }
            "mpris:length" => {
                if let Ok(us) = v.downcast_ref::<i64>() {
                    info.duration = Some((us / 1_000_000) as u32);
                }
            }
            _ => {}
        }
    }

    info
}

async fn handle_watch_event(
    proxy: &zbus::Proxy<'_>,
    event: MediaPlayerEvent,
    can_play: bool,
    can_pause: bool,
    can_go_next: bool,
    can_go_previous: bool,
) -> Result<()> {
    match event {
        MediaPlayerEvent::AppOpened => {}
        MediaPlayerEvent::Play => {
            if can_play {
                proxy.call_noreply("Play", &()).await?;
            }
        }
        MediaPlayerEvent::Pause => {
            if can_pause {
                proxy.call_noreply("Pause", &()).await?;
            }
        }
        MediaPlayerEvent::Next => {
            if can_go_next {
                proxy.call_noreply("Next", &()).await?;
            }
        }
        MediaPlayerEvent::Previous => {
            if can_go_previous {
                proxy.call_noreply("Previous", &()).await?;
            }
        }
        MediaPlayerEvent::VolumeUp => {
            if let Ok(vol) = proxy.get_property::<f64>("Volume").await {
                let new_vol = (vol + VOLUME_STEP).min(1.0);
                let _ = proxy.set_property("Volume", new_vol).await;
            }
        }
        MediaPlayerEvent::VolumeDown => {
            if let Ok(vol) = proxy.get_property::<f64>("Volume").await {
                let new_vol = (vol - VOLUME_STEP).max(0.0);
                let _ = proxy.set_property("Volume", new_vol).await;
            }
        }
    }
    Ok(())
}
