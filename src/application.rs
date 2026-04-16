// SPDX-License-Identifier: GPL-3.0-or-later

use gettextrs::gettext;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use std::cell::OnceCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ble_manager;
use crate::config::VERSION;
use crate::notifications;
use crate::PinepalWindow;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct PinepalApplication {
        pub tokio_rt: OnceCell<tokio::runtime::Runtime>,
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
        fn activate(&self) {
            let application = self.obj();
            let window = application.active_window().unwrap_or_else(|| {
                let rt = self.tokio_rt.get_or_init(|| {
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime")
                });

                let (ble_handle, event_rx) = ble_manager::spawn(rt);

                // Notification forwarding: bridge GSettings to an AtomicBool
                let settings = gio::Settings::new("io.github.nico359.pinepal");
                let notif_enabled = Arc::new(AtomicBool::new(settings.boolean("forward-notifications")));
                let notif_flag = notif_enabled.clone();
                settings.connect_changed(Some("forward-notifications"), move |s, _| {
                    notif_flag.store(s.boolean("forward-notifications"), Ordering::Relaxed);
                });
                notifications::spawn_notification_forwarder(rt, ble_handle.clone(), notif_enabled);

                let window = PinepalWindow::new(&*application);
                window.init_ble(ble_handle, event_rx);
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
            .activate(move |app: &Self, _, _| app.quit())
            .build();
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        let logs_action = gio::ActionEntry::builder("show-logs")
            .activate(move |app: &Self, _, _| app.show_logs())
            .build();
        self.add_action_entries([quit_action, about_action, logs_action]);
    }

    fn show_logs(&self) {
        let window = self.active_window().unwrap();
        crate::log_viewer::show_log_viewer(&window);
    }

    fn show_about(&self) {
        let window = self.active_window().unwrap();
        let about = adw::AboutDialog::builder()
            .application_name("PinePal")
            .application_icon("io.github.nico359.pinepal")
            .developer_name("nico359")
            .version(VERSION)
            .developers(vec!["nico359", "GitHub Copilot CLI (Claude)"])
            .comments("Companion app for PineTime smartwatches running InfiniTime.\n\nBuilt with the assistance of AI (GitHub Copilot CLI, powered by Claude).")
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
