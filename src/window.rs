// SPDX-License-Identifier: GPL-3.0-or-later

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

use crate::ble_manager::{BleCommand, BleEvent, BleHandle};
use crate::dashboard_page::PinepalDashboardPage;
use crate::devices_page::PinepalDevicesPage;
use crate::step_db::StepDb;
use bluer::Address;

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

    pub fn init_ble(&self, ble: BleHandle, mut event_rx: tokio::sync::mpsc::Receiver<BleEvent>) {
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

        // Poll BLE events on glib main loop
        let window = self.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            while let Ok(event) = event_rx.try_recv() {
                window.handle_ble_event(event);
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
                self.show_devices();
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
        let ble = imp.ble_handle.borrow().clone();
        dashboard.connect_disconnect(move || {
            if let Some(ref ble) = ble {
                ble.send(BleCommand::Disconnect);
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

    fn show_devices(&self) {
        let imp = self.imp();
        imp.dashboard_page.replace(None);
        imp.back_button.set_visible(false);
        imp.navigation_view.pop_to_tag("devices");
        imp.devices_page.clear_devices();

        // Re-scan
        if let Some(ref ble) = *imp.ble_handle.borrow() {
            ble.send(BleCommand::StartScan);
        }
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
}
