// SPDX-License-Identifier: GPL-3.0-or-later

use adw::prelude::*;
use adw::subclass::prelude::*;
use futures::StreamExt;
use gettextrs::gettext;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

use crate::ble_manager::{BleCommand, BleEvent, BleHandle, MediaPlayerEvent};
use crate::dashboard_page::PinepalDashboardPage;
use crate::devices_page::PinepalDevicesPage;
use crate::mpris;
use crate::step_db::StepDb;
use crate::updater::{self, LatestRelease};
use crate::weather::{self, Location};
use bluer::Address;

/// Firmware update-check results delivered back to the glib main loop.
pub enum UpdateUiEvent {
    Available(LatestRelease),
    UpToDate,
    Error(String),
}

/// Weather results delivered from the tokio task back to the glib main loop.
pub enum WeatherUiEvent {
    Updated(String),
    Error(String),
    /// A geocode succeeded; the glib thread should (re)start the refresh loop
    /// (it owns the !Send GTK/BLE handles, so this can't be done from tokio).
    LocationResolved(Location),
}

mod imp {
    use super::*;

    #[derive(Debug, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/nico359/pinepal/window.ui")]
    pub struct PinepalWindow {
        #[template_child]
        pub header_bar: TemplateChild<adw::HeaderBar>,
        #[template_child]
        pub back_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub navigation_view: TemplateChild<adw::NavigationView>,
        #[template_child]
        pub devices_page: TemplateChild<PinepalDevicesPage>,

        pub dashboard_page: RefCell<Option<PinepalDashboardPage>>,
        pub ble_handle: RefCell<Option<BleHandle>>,
        pub step_db: RefCell<Option<Rc<StepDb>>>,
        pub tokio_rt: RefCell<Option<tokio::runtime::Handle>>,
        pub mp_event_tx: RefCell<Option<tokio::sync::mpsc::Sender<MediaPlayerEvent>>>,
        pub mpris_watcher: RefCell<Option<tokio::task::JoinHandle<()>>>,
        pub mpris_control: RefCell<Option<tokio::task::JoinHandle<()>>>,
        pub mpris_action_rx: RefCell<Option<tokio::sync::mpsc::Receiver<mpris::PlayersListEvent>>>,
        pub weather_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
        pub weather_tx: RefCell<Option<tokio::sync::mpsc::Sender<WeatherUiEvent>>>,
        pub weather_rx: RefCell<Option<tokio::sync::mpsc::Receiver<WeatherUiEvent>>>,
        pub update_tx: RefCell<Option<tokio::sync::mpsc::Sender<UpdateUiEvent>>>,
        pub update_rx: RefCell<Option<tokio::sync::mpsc::Receiver<UpdateUiEvent>>>,
    }

    impl Default for PinepalWindow {
        fn default() -> Self {
            Self {
                header_bar: Default::default(),
                back_button: Default::default(),
                navigation_view: Default::default(),
                devices_page: Default::default(),
                dashboard_page: RefCell::new(None),
                ble_handle: RefCell::new(None),
                step_db: RefCell::new(None),
                tokio_rt: RefCell::new(None),
                mp_event_tx: RefCell::new(None),
                mpris_watcher: RefCell::new(None),
                mpris_control: RefCell::new(None),
                mpris_action_rx: RefCell::new(None),
                weather_task: RefCell::new(None),
                weather_tx: RefCell::new(None),
                weather_rx: RefCell::new(None),
                update_tx: RefCell::new(None),
                update_rx: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PinepalWindow {
        const NAME: &'static str = "PinepalWindow";
        type Type = super::PinepalWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            PinepalDevicesPage::ensure_type();
            PinepalDashboardPage::ensure_type();
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for PinepalWindow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_back_button();
            obj.setup_background_mode();
            obj.setup_find_devices_action();
        }
    }
    impl WidgetImpl for PinepalWindow {}
    impl WindowImpl for PinepalWindow {}
    impl ApplicationWindowImpl for PinepalWindow {}
    impl AdwApplicationWindowImpl for PinepalWindow {}
}

glib::wrapper! {
    pub struct PinepalWindow(ObjectSubclass<imp::PinepalWindow>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl PinepalWindow {
    pub fn new<P: IsA<gtk::Application>>(application: &P) -> Self {
        glib::Object::builder()
            .property("application", application)
            .build()
    }

    pub fn set_tokio_rt(&self, rt: tokio::runtime::Handle) {
        self.imp().tokio_rt.replace(Some(rt));
    }

    pub fn init_ble(&self, ble: BleHandle, event_rx: tokio::sync::mpsc::Receiver<BleEvent>) {
        self.init_ble_inner(ble, event_rx, None);
    }

    /// Take over an existing BLE connection from the background service.
    /// If `connected_firmware` is `Some`, the watch is already connected and the
    /// dashboard is shown immediately without sending a Connect or StartScan command.
    /// If `None`, falls back to the normal auto-connect / scan logic.
    pub fn init_ble_takeover(
        &self,
        ble: BleHandle,
        event_rx: tokio::sync::mpsc::Receiver<BleEvent>,
        connected_firmware: Option<String>,
    ) {
        self.init_ble_inner(ble, event_rx, connected_firmware);
    }

    fn init_ble_inner(
        &self,
        ble: BleHandle,
        mut event_rx: tokio::sync::mpsc::Receiver<BleEvent>,
        connected_firmware: Option<String>,
    ) {
        let imp = self.imp();
        imp.ble_handle.replace(Some(ble.clone()));

        // Open step database
        let db = match StepDb::open() {
            Ok(db) => Rc::new(db),
            Err(e) => {
                log::error!("Failed to open step database: {e}");
                return;
            }
        };
        imp.step_db.replace(Some(db.clone()));

        // Setup devices page click handler
        let ble_for_devices = ble.clone();
        imp.devices_page.connect_device_activated(move |addr| {
            ble_for_devices.send(BleCommand::Connect(addr));
        });

        // Cancel reconnect button
        let ble_for_cancel = ble.clone();
        imp.devices_page.connect_cancel_reconnect(move || {
            ble_for_cancel.send(BleCommand::Disconnect);
        });

        if let Some(ref fw) = connected_firmware {
            // Taking over a connected service — show dashboard immediately,
            // then ask the BLE manager to re-send current characteristic values.
            log::info!("Taking over service connection, showing dashboard");
            self.show_dashboard(fw);
            ble.send(BleCommand::RequestUpdate);
        } else {
            // Start scanning or auto-connect to last known watch
            let settings = gio::Settings::new("io.github.nico359.pinepal");
            let saved_addr = settings.string("auto-connect-address");
            if saved_addr.is_empty() {
                ble.send(BleCommand::StartScan);
            } else {
                match saved_addr.parse::<Address>() {
                    Ok(addr) => {
                        log::info!("Auto-connecting to last known watch {addr}");
                        ble.send(BleCommand::Connect(addr));
                    }
                    Err(e) => {
                        log::warn!("Saved address '{saved_addr}' is invalid ({e}), scanning instead");
                        ble.send(BleCommand::StartScan);
                    }
                }
            }
        }

        // Poll BLE events and MPRIS actions on glib main loop
        let window = self.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            while let Ok(event) = event_rx.try_recv() {
                window.handle_ble_event(event);
            }
            // Poll MPRIS player list changes
            let imp = window.imp();
            if let Some(ref mut rx) = *imp.mpris_action_rx.borrow_mut() {
                while let Ok(action) = rx.try_recv() {
                    window.handle_mpris_action(action);
                }
            }
            // Poll weather fetch results
            let mut resolved_location = None;
            if let Some(ref mut rx) = *imp.weather_rx.borrow_mut() {
                while let Ok(event) = rx.try_recv() {
                    match event {
                        WeatherUiEvent::Updated(summary) => {
                            if let Some(ref dash) = *imp.dashboard_page.borrow() {
                                dash.set_weather(&summary);
                            }
                        }
                        WeatherUiEvent::Error(msg) => {
                            if let Some(ref dash) = *imp.dashboard_page.borrow() {
                                dash.set_weather(&format!("Error: {msg}"));
                            }
                        }
                        WeatherUiEvent::LocationResolved(loc) => resolved_location = Some(loc),
                    }
                }
            }
            if let Some(loc) = resolved_location {
                let settings = gio::Settings::new("io.github.nico359.pinepal");
                let _ = settings.set_string("weather-location", &format_saved_location(&loc));
                window.start_weather_refresh(loc);
            }
            // Poll firmware update-check results
            if let Some(ref mut rx) = *imp.update_rx.borrow_mut() {
                while let Ok(event) = rx.try_recv() {
                    window.handle_update_event(event);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    pub fn shutdown(&self) {
        self.set_hide_on_close(false);

        if let Some(ref ble) = *self.imp().ble_handle.borrow() {
            ble.send(BleCommand::Shutdown);
        }

        self.close();
    }

    fn handle_ble_event(&self, event: BleEvent) {
        let imp = self.imp();

        match event {
            BleEvent::Scanning => {
                imp.devices_page.set_scanning(true);
            }
            BleEvent::DeviceFound { address, name, .. } => {
                imp.devices_page.add_device(address, &name);
            }
            BleEvent::Connected { address, firmware } => {
                // Save address for auto-reconnect
                let settings = gio::Settings::new("io.github.nico359.pinepal");
                let _ = settings.set_string("auto-connect-address", &address.to_string());
                self.show_dashboard(&firmware);
            }
            BleEvent::FirmwareVersion(fw) => {
                if let Some(ref dash) = *imp.dashboard_page.borrow() {
                    dash.set_firmware(&fw);
                }
            }
            BleEvent::Disconnected { reason } => {
                log::info!("Disconnected: {reason}");
                // Only start a new scan for user-initiated disconnects.  For an
                // unexpected loss of connection the BLE manager's own reconnect
                // loop is already running — sending StartScan here would cancel it.
                if reason.starts_with("User") || reason.starts_with("Switching") {
                    self.show_devices();
                } else {
                    self.show_devices_no_scan();
                }
            }
            BleEvent::BatteryLevel(level) => {
                if let Some(ref dash) = *imp.dashboard_page.borrow() {
                    dash.set_battery(level);
                }
            }
            BleEvent::HeartRate(bpm) => {
                if let Some(ref dash) = *imp.dashboard_page.borrow() {
                    dash.set_heart_rate(bpm);
                }
            }
            BleEvent::StepCount(steps) => {
                if let Some(ref dash) = *imp.dashboard_page.borrow() {
                    dash.set_steps(steps);
                }
                // Save to database
                if let Some(ref db) = *imp.step_db.borrow() {
                    let today = chrono::Local::now().date_naive();
                    if let Err(e) = db.upsert_steps(&today, steps) {
                        log::error!("Failed to save steps: {e}");
                    }
                }
                // Refresh chart
                if let Some(ref dash) = *imp.dashboard_page.borrow() {
                    dash.refresh_chart();
                }
            }
            BleEvent::Error(msg) => {
                log::error!("BLE error: {msg}");
                imp.devices_page.set_error(&msg);
            }
            BleEvent::BluetoothOff => {
                log::warn!("Bluetooth is off");
                // Navigate away from the dashboard (if shown) so the user sees
                // the "Bluetooth is Off" status. Don't send StartScan — the BLE
                // manager's reconnect loop will pick up once BT comes back.
                self.show_devices_no_scan();
                imp.devices_page.set_bluetooth_off();
            }
            BleEvent::BluetoothReady => {
                log::info!("Bluetooth is on");
                self.show_devices();
                imp.devices_page.set_ready();
            }
            BleEvent::Reconnecting { attempt, delay_secs } => {
                imp.devices_page.set_reconnecting(attempt, delay_secs);
            }
            BleEvent::MediaPlayerEvent(event) => {
                if let Some(ref tx) = *imp.mp_event_tx.borrow() {
                    let _ = tx.try_send(event);
                }
            }
            BleEvent::FirmwareUpdateProgress(pct) => {
                if let Some(ref dash) = *imp.dashboard_page.borrow() {
                    dash.set_update_button_label(&format!("Updating… {pct}%"));
                }
            }
            BleEvent::FirmwareUpdateStatus(status) => {
                if let Some(ref dash) = *imp.dashboard_page.borrow() {
                    let label = match status.as_str() {
                        "flashing" => "Updating… 0%".to_string(),
                        "rebooting" => "Rebooting…".to_string(),
                        s if s.starts_with("failed") => format!("Update failed ({s})"),
                        s => s.to_string(),
                    };
                    dash.set_update_button_label(&label);
                    dash.set_update_button_sensitive(status.starts_with("failed"));
                }
            }
            BleEvent::PasskeyRequested => {
                self.show_passkey_dialog();
            }
        }
    }

    /// Prompt for the 6-digit pairing code displayed on the watch.
    fn show_passkey_dialog(&self) {
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Pair with Watch"))
            .body(gettext("Enter the 6-digit code shown on your watch."))
            .build();
        let entry = gtk::Entry::builder()
            .input_purpose(gtk::InputPurpose::Digits)
            .max_length(6)
            .build();
        dialog.set_extra_child(Some(&entry));
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("pair", &gettext("Pair"));
        dialog.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("pair"));

        let ble = self.imp().ble_handle.borrow().clone();
        dialog.connect_response(None, move |_, response| {
            let Some(ref ble) = ble else { return };
            if response == "pair" {
                if let Ok(code) = entry.text().parse::<u32>() {
                    ble.provide_passkey(code);
                    return;
                }
            }
            ble.cancel_passkey();
        });
        dialog.present(Some(self));
    }

    fn show_dashboard(&self, firmware: &str) {
        let imp = self.imp();
        let dashboard = PinepalDashboardPage::new();
        dashboard.set_firmware(firmware);

        // Set step DB
        if let Some(ref db) = *imp.step_db.borrow() {
            dashboard.set_step_db(db.clone());
            dashboard.refresh_chart();
        }

        // Disconnect button
        let ble_for_disc = imp.ble_handle.borrow().clone();
        dashboard.connect_disconnect(move || {
            if let Some(ref ble) = ble_for_disc {
                ble.send(BleCommand::Disconnect);
            }
        });

        // ---- MPRIS media player setup ----

        // Channel for forwarding watch media events to MPRIS control session
        let (mp_event_tx, _mp_event_rx) = tokio::sync::mpsc::channel::<MediaPlayerEvent>(32);
        imp.mp_event_tx.replace(Some(mp_event_tx));

        // Channel for MPRIS player add/remove events (tokio -> glib)
        let (mpris_tx, mpris_rx) = tokio::sync::mpsc::channel::<mpris::PlayersListEvent>(32);
        imp.mpris_action_rx.replace(Some(mpris_rx));

        // Spawn MPRIS player watcher on tokio
        if let Some(ref rt) = *imp.tokio_rt.borrow() {
            let watcher = rt.spawn(async move {
                let conn = match zbus::Connection::session().await {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!("MPRIS: failed to connect to session bus: {e}");
                        return;
                    }
                };
                let stream = match mpris::get_players_stream(&conn).await {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!("MPRIS: failed to get player stream: {e}");
                        return;
                    }
                };
                tokio::pin!(stream);
                while let Some(event) = stream.next().await {
                    if mpris_tx.send(event).await.is_err() {
                        break; // channel closed
                    }
                }
            });
            imp.mpris_watcher.replace(Some(watcher));
        }

        // When user selects a player in the dropdown, start a control session
        let ble_for_mp = imp.ble_handle.borrow().clone();
        let window_for_mp = self.clone();
        dashboard.connect_media_player_selected(move |index| {
            let rt_handle = window_for_mp.imp().tokio_rt.borrow().clone();
            if let (Some(ref rt), Some(ble)) = (rt_handle, ble_for_mp.clone()) {
                let player_names = window_for_mp.imp()
                    .dashboard_page.borrow().as_ref()
                    .and_then(|d| {
                        let names = d.imp().player_names.borrow();
                        names.string(index as u32).map(|s| s.to_string())
                    });

                // Cancel previous control session
                window_for_mp.abort_control_session();

                if let Some(player_name) = player_names {
                    // Create a new channel for this control session
                    let (new_tx, new_rx) = tokio::sync::mpsc::channel::<MediaPlayerEvent>(32);
                    window_for_mp.imp().mp_event_tx.replace(Some(new_tx));

                    let task = rt.spawn(async move {
                        let conn = match zbus::Connection::session().await {
                            Ok(c) => c,
                            Err(e) => {
                                log::error!("MPRIS control: session bus error: {e}");
                                return;
                            }
                        };
                        if let Err(e) = mpris::run_control_session(&conn, &player_name, ble, new_rx).await {
                            log::error!("MPRIS control session error: {e}");
                        }
                    });
                    window_for_mp.imp().mpris_control.replace(Some(task));
                }
            }
        });

        // ---- Firmware update setup ----
        let (update_tx, update_rx) = tokio::sync::mpsc::channel(4);
        imp.update_tx.replace(Some(update_tx));
        imp.update_rx.replace(Some(update_rx));

        let window_for_update = self.clone();
        dashboard.connect_check_update(move || {
            window_for_update.check_for_update();
        });

        // ---- Weather setup ----
        let (weather_tx, weather_rx) = tokio::sync::mpsc::channel(4);
        imp.weather_tx.replace(Some(weather_tx));
        imp.weather_rx.replace(Some(weather_rx));

        let window_for_weather = self.clone();
        dashboard.connect_weather_activated(move || {
            window_for_weather.show_weather_dialog();
        });

        // Resume pushing weather for the last saved location, if any.
        let settings = gio::Settings::new("io.github.nico359.pinepal");
        let saved = settings.string("weather-location");
        if let Some(loc) = parse_saved_location(&saved) {
            dashboard.set_weather(&format!("Loading weather for {}…", loc.name));
            self.start_weather_refresh(loc);
        }

        imp.dashboard_page.replace(Some(dashboard.clone()));

        let nav_page = adw::NavigationPage::builder()
            .title("Dashboard")
            .tag("dashboard")
            .child(&dashboard)
            .build();
        imp.navigation_view.push(&nav_page);
        imp.back_button.set_visible(true);
    }

    fn abort_control_session(&self) {
        if let Some(handle) = self.imp().mpris_control.borrow_mut().take() {
            handle.abort();
            log::debug!("MPRIS control session aborted");
        }
    }

    fn handle_mpris_action(&self, action: mpris::PlayersListEvent) {
        if let Some(ref dash) = *self.imp().dashboard_page.borrow() {
            match action {
                mpris::PlayersListEvent::PlayerAdded(name) => dash.add_media_player(&name),
                mpris::PlayersListEvent::PlayerRemoved(name) => dash.remove_media_player(&name),
            }
        }
    }

    /// Check GitHub for a newer InfiniTime release than the connected watch's.
    fn check_for_update(&self) {
        let imp = self.imp();
        let Some(rt) = imp.tokio_rt.borrow().clone() else { return };
        let Some(tx) = imp.update_tx.borrow().clone() else { return };
        let Some(current) = imp.dashboard_page.borrow().as_ref().map(|d| d.firmware_version()) else { return };
        if let Some(ref dash) = *imp.dashboard_page.borrow() {
            dash.set_update_button_sensitive(false);
            dash.set_update_button_label("Checking…");
        }
        rt.spawn(async move {
            match updater::fetch_latest().await {
                Ok(release) => {
                    if updater::is_newer(&release.version, &current) {
                        let _ = tx.send(UpdateUiEvent::Available(release)).await;
                    } else {
                        let _ = tx.send(UpdateUiEvent::UpToDate).await;
                    }
                }
                Err(e) => {
                    log::warn!("Firmware check failed: {e:#}");
                    let _ = tx.send(UpdateUiEvent::Error(e.to_string())).await;
                }
            }
        });
    }

    fn handle_update_event(&self, event: UpdateUiEvent) {
        let imp = self.imp();
        match event {
            UpdateUiEvent::Available(release) => {
                if let Some(ref dash) = *imp.dashboard_page.borrow() {
                    dash.set_update_button_label("Check for Update");
                    dash.set_update_button_sensitive(true);
                }
                self.show_update_dialog(release);
            }
            UpdateUiEvent::UpToDate => {
                if let Some(ref dash) = *imp.dashboard_page.borrow() {
                    dash.set_update_button_label("Up to Date");
                    dash.set_update_button_sensitive(true);
                }
            }
            UpdateUiEvent::Error(msg) => {
                log::warn!("Firmware check error: {msg}");
                if let Some(ref dash) = *imp.dashboard_page.borrow() {
                    dash.set_update_button_label("Check Failed");
                    dash.set_update_button_sensitive(true);
                }
            }
        }
    }

    fn show_update_dialog(&self, release: LatestRelease) {
        let dialog = adw::AlertDialog::builder()
            .heading("Firmware Update Available")
            .body(format!(
                "InfiniTime {} is available. Keep the watch nearby and charged during the update.",
                release.version
            ))
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("install", "Install");
        dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("install"));
        dialog.set_close_response("cancel");

        let window = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "install" {
                window.start_firmware_update(release.clone());
            }
        });
        dialog.present(Some(self));
    }

    /// Download the DFU package and hand it to the BLE manager to flash.
    fn start_firmware_update(&self, release: LatestRelease) {
        let imp = self.imp();
        let Some(rt) = imp.tokio_rt.borrow().clone() else { return };
        let Some(ble) = imp.ble_handle.borrow().clone() else { return };
        let Some(tx) = imp.update_tx.borrow().clone() else { return };
        if let Some(ref dash) = *imp.dashboard_page.borrow() {
            dash.set_update_button_sensitive(false);
            dash.set_update_button_label("Downloading…");
        }
        rt.spawn(async move {
            match updater::download_package(&release).await {
                Ok(package) => ble.send(BleCommand::InstallFirmware(package)),
                Err(e) => {
                    log::warn!("Firmware download failed: {e:#}");
                    let _ = tx.send(UpdateUiEvent::Error(e.to_string())).await;
                }
            }
        });
    }

    fn stop_mpris_tasks(&self) {
        self.abort_control_session();
        if let Some(handle) = self.imp().mpris_watcher.borrow_mut().take() {
            handle.abort();
            log::debug!("MPRIS player watcher stopped");
        }
        self.imp().mp_event_tx.replace(None);
        self.imp().mpris_action_rx.replace(None);
    }

    fn show_devices(&self) {
        self.stop_mpris_tasks();
        self.stop_weather_task();
        let imp = self.imp();
        imp.dashboard_page.replace(None);
        imp.weather_tx.replace(None);
        imp.weather_rx.replace(None);
        imp.update_tx.replace(None);
        imp.update_rx.replace(None);
        imp.back_button.set_visible(false);
        imp.navigation_view.pop_to_tag("devices");
        imp.devices_page.clear_devices();

        // Re-scan
        if let Some(ref ble) = *imp.ble_handle.borrow() {
            ble.send(BleCommand::StartScan);
        }
    }

    /// Navigate back to the devices page WITHOUT starting a new scan.
    /// Used when the connection was lost unexpectedly — the BLE manager's own
    /// reconnect loop is already running and must not be interrupted.
    fn show_devices_no_scan(&self) {
        self.stop_mpris_tasks();
        self.stop_weather_task();
        let imp = self.imp();
        imp.dashboard_page.replace(None);
        imp.weather_tx.replace(None);
        imp.weather_rx.replace(None);
        imp.update_tx.replace(None);
        imp.update_rx.replace(None);
        imp.back_button.set_visible(false);
        imp.navigation_view.pop_to_tag("devices");
        imp.devices_page.clear_devices();
        // No BleCommand::StartScan — let the reconnect loop do its job.
    }

    fn setup_back_button(&self) {
        let window = self.clone();
        self.imp().back_button.connect_clicked(move |_| {
            if let Some(ref ble) = *window.imp().ble_handle.borrow() {
                ble.send(BleCommand::Disconnect);
            }
        });
    }

    fn setup_background_mode(&self) {
        let settings = gio::Settings::new("io.github.nico359.pinepal");
        self.set_hide_on_close(settings.boolean("run-in-background"));
        let window = self.clone();
        settings.connect_changed(Some("run-in-background"), move |s, _| {
            window.set_hide_on_close(s.boolean("run-in-background"));
        });
    }

    fn setup_find_devices_action(&self) {
        let window = self.clone();
        let action = gio::SimpleAction::new("find-devices", None);
        action.connect_activate(move |_, _| {
            let imp = window.imp();
            if imp.dashboard_page.borrow().is_some() {
                // Disconnect from watch; Disconnected event will call show_devices()
                if let Some(ref ble) = *imp.ble_handle.borrow() {
                    ble.send(BleCommand::Disconnect);
                }
            } else {
                // Not connected — jump straight to device scan
                window.show_devices();
            }
        });
        self.add_action(&action);
    }

    /// Prompt for an address/city, then start pushing weather for it.
    fn show_weather_dialog(&self) {
        let entry = gtk::Entry::builder()
            .placeholder_text("e.g. Berlin, Germany")
            .activates_default(true)
            .build();

        let dialog = adw::AlertDialog::builder()
            .heading("Weather Location")
            .body("Location on this device isn't reliable - enter an address or city instead.")
            .extra_child(&entry)
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("set", "Set");
        dialog.set_response_appearance("set", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("set"));
        dialog.set_close_response("cancel");

        let window = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "set" {
                return;
            }
            let query = entry.text().to_string();
            if query.trim().is_empty() {
                return;
            }
            window.geocode_and_start(query);
        });
        dialog.present(Some(self));
    }

    /// Resolve `query` to coordinates; the refresh loop is (re)started once the
    /// result comes back on the glib thread (see the `LocationResolved` handler).
    fn geocode_and_start(&self, query: String) {
        let Some(rt) = self.imp().tokio_rt.borrow().clone() else { return };
        let Some(tx) = self.imp().weather_tx.borrow().clone() else { return };
        rt.spawn(async move {
            match weather::geocode(&query).await {
                Ok(loc) => {
                    let _ = tx
                        .send(WeatherUiEvent::Updated(format!("Loading weather for {}…", loc.name)))
                        .await;
                    let _ = tx.send(WeatherUiEvent::LocationResolved(loc)).await;
                }
                Err(e) => {
                    log::warn!("Geocoding failed: {e:#}");
                    let _ = tx.send(WeatherUiEvent::Error(format!("location not found: {e}"))).await;
                }
            }
        });
    }

    /// Fetch weather for `loc` immediately, push it to the watch, and repeat
    /// every 30 minutes for as long as this task runs (aborted on disconnect).
    fn start_weather_refresh(&self, loc: Location) {
        self.stop_weather_task();
        let Some(rt) = self.imp().tokio_rt.borrow().clone() else { return };
        let Some(ble) = self.imp().ble_handle.borrow().clone() else { return };
        let Some(tx) = self.imp().weather_tx.borrow().clone() else { return };

        let task = rt.spawn(async move {
            loop {
                match weather::fetch(&loc).await {
                    Ok(data) => {
                        ble.send(BleCommand::SendWeather(data.clone()));
                        let _ = tx.send(WeatherUiEvent::Updated(data.summary())).await;
                    }
                    Err(e) => {
                        log::warn!("Weather fetch failed: {e:#}");
                        let _ = tx.send(WeatherUiEvent::Error(e.to_string())).await;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;
            }
        });
        self.imp().weather_task.replace(Some(task));
    }

    fn stop_weather_task(&self) {
        if let Some(handle) = self.imp().weather_task.borrow_mut().take() {
            handle.abort();
            log::debug!("Weather refresh task stopped");
        }
    }
}

/// Format a location for storage as a single GSettings string.
fn format_saved_location(loc: &Location) -> String {
    format!("{}|{}|{}", loc.name, loc.lat, loc.lon)
}

/// Parse a location previously saved by `format_saved_location`.
fn parse_saved_location(saved: &str) -> Option<Location> {
    let mut parts = saved.splitn(3, '|');
    let name = parts.next()?.to_string();
    let lat: f64 = parts.next()?.parse().ok()?;
    let lon: f64 = parts.next()?.parse().ok()?;
    if name.is_empty() {
        return None;
    }
    Some(Location { lat, lon, name })
}
