// SPDX-License-Identifier: GPL-3.0-or-later

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use std::cell::Cell;
use std::rc::Rc;

mod imp {
    use super::*;

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/nico359/pinepal/preferences_dialog.ui")]
    pub struct PinepalPreferencesDialog {
        #[template_child]
        pub run_in_background_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub autostart_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub forward_notifications_row: TemplateChild<adw::SwitchRow>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PinepalPreferencesDialog {
        const NAME: &'static str = "PinepalPreferencesDialog";
        type Type = super::PinepalPreferencesDialog;
        type ParentType = adw::PreferencesDialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for PinepalPreferencesDialog {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup_settings();
        }
    }

    impl WidgetImpl for PinepalPreferencesDialog {}
    impl AdwDialogImpl for PinepalPreferencesDialog {}
    impl PreferencesDialogImpl for PinepalPreferencesDialog {}
}

glib::wrapper! {
    pub struct PinepalPreferencesDialog(ObjectSubclass<imp::PinepalPreferencesDialog>)
        @extends adw::PreferencesDialog, adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl PinepalPreferencesDialog {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    fn setup_settings(&self) {
        let imp = self.imp();
        let settings = gio::Settings::new("io.github.nico359.pinepal");

        // Bind run-in-background and forward-notifications directly to GSettings.
        settings
            .bind("run-in-background", &*imp.run_in_background_row, "active")
            .build();
        settings
            .bind("forward-notifications", &*imp.forward_notifications_row, "active")
            .build();

        // Autostart: show saved state, and call the portal when the user changes it.
        // Set the initial value before connecting the handler to avoid triggering it.
        imp.autostart_row
            .set_active(settings.boolean("autostart"));

        let dialog_ref = self.clone();
        let reverting = Rc::new(Cell::new(false));
        let reverting_clone = reverting.clone();

        imp.autostart_row.connect_active_notify(move |row| {
            if reverting_clone.get() {
                return;
            }
            let enable = row.is_active();
            let row = row.clone();
            let reverting_revert = reverting_clone.clone();
            let dialog = dialog_ref.clone();

            glib::spawn_future_local(async move {
                match crate::background_portal::request_autostart(enable).await {
                    Ok(granted) => {
                        let settings = gio::Settings::new("io.github.nico359.pinepal");
                        settings.set_boolean("autostart", granted).ok();
                        if granted != enable {
                            reverting_revert.set(true);
                            row.set_active(granted);
                            reverting_revert.set(false);
                        }
                    }
                    Err(e) => {
                        log::error!("Background portal error: {e}");
                        reverting_revert.set(true);
                        row.set_active(!enable);
                        reverting_revert.set(false);

                        let alert = adw::AlertDialog::builder()
                            .heading("Autostart Unavailable")
                            .body("The system background portal is not available. \
                                   Launch at Login cannot be configured in this environment.")
                            .build();
                        alert.add_response("ok", "OK");
                        alert.set_default_response(Some("ok"));
                        alert.set_close_response("ok");
                        alert.present(Some(&dialog));
                    }
                }
            });
        });
    }
}
