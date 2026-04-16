// SPDX-License-Identifier: GPL-3.0-or-later

mod application;
mod ble_manager;
mod config;
mod dashboard_page;
mod devices_page;
mod log_collector;
mod log_viewer;
mod notifications;
mod step_chart;
mod step_db;
mod window;

use self::application::PinepalApplication;
use self::window::PinepalWindow;

use config::{GETTEXT_PACKAGE, LOCALEDIR, PKGDATADIR};
use gettextrs::{bind_textdomain_codeset, bindtextdomain, textdomain};
use gtk::{gio, glib};
use gtk::prelude::*;

fn main() -> glib::ExitCode {
    log_collector::init();

    bindtextdomain(GETTEXT_PACKAGE, LOCALEDIR).expect("Unable to bind the text domain");
    bind_textdomain_codeset(GETTEXT_PACKAGE, "UTF-8")
        .expect("Unable to set the text domain encoding");
    textdomain(GETTEXT_PACKAGE).expect("Unable to switch to the text domain");

    let resources = gio::Resource::load(PKGDATADIR.to_owned() + "/pinepal.gresource")
        .expect("Could not load resources");
    gio::resources_register(&resources);

    let app = PinepalApplication::new("io.github.nico359.pinepal", &gio::ApplicationFlags::empty());
    app.run()
}
