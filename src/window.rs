// SPDX-License-Identifier: GPL-3.0-or-later

use adw::prelude::*;
use adw::subclass::prelude::*;
use futures::StreamExt;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

use crate::ble_manager::{BleCommand, BleEvent, BleHandle, MediaPlayerEvent};
use crate::dashboard_page::PinepalDashboardPage;
use crate::devices_page::PinepalDevicesPage;
use crate::garmin_ble::{GarminCommand, GarminHandle};
use crate::mpris;
use crate::step_db::StepDb;
use bluer::Address;
use std::collections::HashSet;

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
        pub garmin_handle: RefCell<Option<GarminHandle>>,
        pub garmin_addrs: RefCell<HashSet<Address>>,
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
                garmin_handle: RefCell::new(None),
                garmin_addrs: RefCell::new(HashSet::new()),
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

    pub fn set_garmin_handle(&self, handle: GarminHandle) {
        self.imp().garmin_handle.replace(Some(handle));
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

        // Setup devices page click handler — routes to correct BLE manager
        let ble_for_devices = ble.clone();
        let garmin_for_devices = imp.garmin_handle.borrow().clone();
        let addrs = imp.garmin_addrs.clone();
        imp.devices_page.connect_device_activated(move |addr| {
            if addrs.borrow().contains(&addr) {
                if let Some(ref garmin) = garmin_for_devices {
                    garmin.send(GarminCommand::Connect(addr));
                }
            } else {
                ble_for_devices.send(BleCommand::Connect(addr));
            }
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
            glib::ControlFlow::Continue
        });
    }

    pub fn shutdown(&self) {
        self.set_hide_on_close(false);

        if let Some(ref ble) = *self.imp().ble_handle.borrow() {
            ble.send(BleCommand::Shutdown);
        }
        if let Some(ref garmin) = *self.imp().garmin_handle.borrow() {
            garmin.send(GarminCommand::Shutdown);
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
                // Track suspected Garmin addresses for routing Connect commands
                if name != "InfiniTime" {
                    imp.garmin_addrs.borrow_mut().insert(address);
                }
            }
            BleEvent::Connected { address, firmware } => {
                // Save address for auto-reconnect
                let settings = gio::Settings::new("io.github.nico359.pinepal");
                if firmware.starts_with("Garmin") {
                    let _ = settings.set_string("auto-connect-garmin-address", &address.to_string());
                    // Don't show dashboard for Garmin — keep device list visible
                    log::info!("Garmin watch connected: {firmware}");
                } else {
                    let _ = settings.set_string("auto-connect-address", &address.to_string());
                    self.show_dashboard(&firmware);
                }
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
        }
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
        let imp = self.imp();
        imp.dashboard_page.replace(None);
        imp.back_button.set_visible(false);
        imp.navigation_view.pop_to_tag("devices");
        imp.devices_page.clear_devices();
        imp.garmin_addrs.borrow_mut().clear();

        // Re-scan both managers
        if let Some(ref ble) = *imp.ble_handle.borrow() {
            ble.send(BleCommand::StartScan);
        }
        if let Some(ref garmin) = *imp.garmin_handle.borrow() {
            garmin.send(GarminCommand::StartScan);
        }
    }

    /// Navigate back to the devices page WITHOUT starting a new scan.
    /// Used when the connection was lost unexpectedly — the BLE manager's own
    /// reconnect loop is already running and must not be interrupted.
    fn show_devices_no_scan(&self) {
        self.stop_mpris_tasks();
        let imp = self.imp();
        imp.dashboard_page.replace(None);
        imp.back_button.set_visible(false);
        imp.navigation_view.pop_to_tag("devices");
        imp.devices_page.clear_devices();
        imp.garmin_addrs.borrow_mut().clear();
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
                if let Some(ref garmin) = *imp.garmin_handle.borrow() {
                    garmin.send(GarminCommand::Disconnect);
                }
            } else {
                // Not connected — jump straight to device scan
                window.show_devices();
            }
        });
        self.add_action(&action);
    }
}
