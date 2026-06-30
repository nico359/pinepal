// SPDX-License-Identifier: GPL-3.0-or-later

use gettextrs::gettext;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use std::cell::{OnceCell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::ble_manager::{self, BleCommand, BleEvent, BleHandle};
use crate::config::VERSION;
use crate::notifications;
use crate::PinepalWindow;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct PinepalApplication {
        pub tokio_rt: OnceCell<tokio::runtime::Runtime>,
        /// BLE handle when running as a headless D-Bus service (autostart mode).
        pub service_ble: RefCell<Option<BleHandle>>,
        /// Event receiver for the service BLE — drained by a timer in service mode,
        /// then taken and handed to the window when the GUI is opened.
        pub service_event_rx: RefCell<Option<tokio::sync::mpsc::Receiver<BleEvent>>>,
        /// Firmware of the last watch connected in service mode, cleared on disconnect.
        pub service_connected_fw: RefCell<Option<String>>,
        /// Hold guard that keeps the app alive while running as a background service.
        pub service_hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PinepalApplication {
        const NAME: &'static str = "PinepalApplication";
        type Type = super::PinepalApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for PinepalApplication {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_gactions();
            obj.set_accels_for_action("app.quit", &["<control>q"]);
        }
    }

    impl ApplicationImpl for PinepalApplication {
        fn startup(&self) {
            self.parent_startup();

            // Register bundled app icons so GTK can find e.g. heart-symbolic.
            if let Some(display) = gtk::gdk::Display::default() {
                gtk::IconTheme::for_display(&display)
                    .add_resource_path("/io/github/nico359/pinepal/icons");
            }

            // Always initialise tokio early so both startup paths can use it.
            self.tokio_rt.get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create tokio runtime")
            });

            // When started via D-Bus activation (autostart at login with
            // --gapplication-service), `activate` is never called.  Spin up
            // BLE silently so the watch stays connected without showing a window.
            if self.obj().flags().contains(gio::ApplicationFlags::IS_SERVICE) {
                log::info!("Starting in background service mode");
                *self.service_hold.borrow_mut() = Some(self.obj().hold());
                self.obj().start_background_service();
            }
        }

        fn activate(&self) {
            let application = self.obj();
            let window = application.active_window().unwrap_or_else(|| {
                let rt = application.imp().tokio_rt.get()
                    .expect("tokio runtime initialised in startup");

                let imp = application.imp();

                // Try to take over the existing service connection seamlessly.
                // If the service BLE and its event channel are both available, reuse
                // them so the watch stays connected while the window opens.
                let service_ble = imp.service_ble.borrow_mut().take();
                let service_rx  = imp.service_event_rx.borrow_mut().take();
                let service_fw  = imp.service_connected_fw.borrow().clone();
                imp.service_hold.borrow_mut().take();

                let (ble_handle, event_rx, takeover_fw) =
                    if let (Some(ble), Some(rx)) = (service_ble, service_rx) {
                        log::info!("Opening window — reusing existing service BLE connection");
                        // Notification forwarder is already running from start_background_service.
                        (ble, rx, service_fw)
                    } else {
                        // No service running — spawn a fresh BLE stack.
                        let (ble, rx) = ble_manager::spawn(rt);

                        // Notification forwarding: bridge GSettings to an AtomicBool
                        let settings = gio::Settings::new("io.github.nico359.pinepal");
                        let notif_enabled = Arc::new(AtomicBool::new(
                            settings.boolean("forward-notifications"),
                        ));
                        let notif_flag = notif_enabled.clone();
                        settings.connect_changed(Some("forward-notifications"), move |s, _| {
                            notif_flag.store(
                                s.boolean("forward-notifications"),
                                Ordering::Relaxed,
                            );
                        });
                        notifications::spawn_notification_forwarder(
                            rt,
                            ble.clone(),
                            notif_enabled,
                        );
                        (ble, rx, None)
                    };

                let window = PinepalWindow::new(&*application);
                window.set_tokio_rt(rt.handle().clone());
                window.init_ble_takeover(ble_handle, event_rx, takeover_fw);
                window.upcast()
            });

            window.present();
        }

        fn shutdown(&self) {
            // Tokio runtime drops automatically
            self.parent_shutdown();
        }
    }

    impl GtkApplicationImpl for PinepalApplication {}
    impl AdwApplicationImpl for PinepalApplication {}
}

glib::wrapper! {
    pub struct PinepalApplication(ObjectSubclass<imp::PinepalApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl PinepalApplication {
    pub fn new(application_id: &str, flags: &gio::ApplicationFlags) -> Self {
        glib::Object::builder()
            .property("application-id", application_id)
            .property("flags", flags)
            .property("resource-base-path", "/io/github/nico359/pinepal")
            .build()
    }

    fn setup_gactions(&self) {
        let quit_action = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| app.full_quit())
            .build();
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        let logs_action = gio::ActionEntry::builder("show-logs")
            .activate(move |app: &Self, _, _| app.show_logs())
            .build();
        let prefs_action = gio::ActionEntry::builder("preferences")
            .activate(move |app: &Self, _, _| app.show_preferences())
            .build();
        self.add_action_entries([quit_action, about_action, logs_action, prefs_action]);
    }

    /// Start the BLE stack in headless mode (no window).  Called when the app
    /// is D-Bus activated at login via the XDG autostart mechanism.
    fn start_background_service(&self) {
        let imp = self.imp();
        let rt = imp.tokio_rt.get().expect("tokio runtime initialised in startup");

        let (ble_handle, event_rx) = ble_manager::spawn(rt);

        // Notification forwarding
        let settings = gio::Settings::new("io.github.nico359.pinepal");
        let notif_enabled = Arc::new(AtomicBool::new(settings.boolean("forward-notifications")));
        let notif_flag = notif_enabled.clone();
        settings.connect_changed(Some("forward-notifications"), move |s, _| {
            notif_flag.store(s.boolean("forward-notifications"), Ordering::Relaxed);
        });
        notifications::spawn_notification_forwarder(rt, ble_handle.clone(), notif_enabled);

        // Auto-connect to last known watch
        let saved_addr = settings.string("auto-connect-address");
        if !saved_addr.is_empty() {
            if let Ok(addr) = saved_addr.parse::<bluer::Address>() {
                log::info!("Background service: auto-connecting to {addr}");
                ble_handle.send(BleCommand::Connect(addr));
            }
        }

        // Store handle and event channel for seamless handover to window.
        *imp.service_ble.borrow_mut() = Some(ble_handle);
        *imp.service_event_rx.borrow_mut() = Some(event_rx);

        // Drain BLE events in the background, tracking connected state so
        // the window can show the dashboard immediately on takeover.
        // The timer stops itself once the event_rx is taken by the window.
        let app_weak = self.downgrade();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let Some(app) = app_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let imp = app.imp();
            let mut rx_opt = imp.service_event_rx.borrow_mut();
            match *rx_opt {
                None => glib::ControlFlow::Break, // taken by window — stop timer
                Some(ref mut rx) => {
                    while let Ok(event) = rx.try_recv() {
                        match event {
                            BleEvent::Connected { ref firmware, .. } => {
                                *imp.service_connected_fw.borrow_mut() = Some(firmware.clone());
                            }
                            BleEvent::Disconnected { .. } => {
                                *imp.service_connected_fw.borrow_mut() = None;
                            }
                            _ => {}
                        }
                    }
                    glib::ControlFlow::Continue
                }
            }
        });
    }

    fn full_quit(&self) {
        if let Some(window) = self
            .active_window()
            .and_then(|window| window.downcast::<PinepalWindow>().ok())
        {
            window.shutdown();
        }

        // Also shut down any background-service BLE
        if let Some(ble) = self.imp().service_ble.borrow_mut().take() {
            ble.send(BleCommand::Shutdown);
        }
        self.imp().service_event_rx.borrow_mut().take();
        self.imp().service_connected_fw.borrow_mut().take();

        self.quit();
    }

    fn show_logs(&self) {
        let window = self.active_window().unwrap();
        crate::log_viewer::show_log_viewer(&window);
    }

    fn show_preferences(&self) {
        let window = self.active_window().unwrap();
        crate::preferences_dialog::PinepalPreferencesDialog::new().present(Some(&window));
    }

    fn show_about(&self) {
        let window = self.active_window().unwrap();
        let about = adw::AboutDialog::builder()
            .application_name("PinePal")
            .application_icon("io.github.nico359.pinepal")
            .developer_name("nico359")
            .version(VERSION)
            .developers(vec!["nico359", "GitHub Copilot CLI (Claude, Deepseek)"])
            .comments("Companion app for PineTime smartwatches running InfiniTime.\n\nBuilt with the assistance of AI (GitHub Copilot CLI, powered by Claude and Deepseek).")
            .website("https://github.com/nico359/pinepal")
            .issue_url("https://github.com/nico359/pinepal/issues")
            .license_type(gtk::License::Gpl30)
            .translator_credits(&gettext("translator-credits"))
            .copyright("© 2026 nico359")
            .build();

        about.add_credit_section(
            Some(&gettext("Based on")),
            &["Watchmate by Andrii Zymohliad https://github.com/azymohliad/watchmate"],
        );

        about.present(Some(&window));
    }
}

