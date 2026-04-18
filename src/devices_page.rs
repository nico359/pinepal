// SPDX-License-Identifier: GPL-3.0-or-later

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use std::cell::RefCell;

mod imp {
    use super::*;

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/nico359/pinepal/devices_page.ui")]
    pub struct PinepalDevicesPage {
        #[template_child]
        pub status_page: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub scan_spinner: TemplateChild<gtk::Spinner>,
        #[template_child]
        pub device_list: TemplateChild<gtk::ListBox>,

        pub devices: RefCell<Vec<(bluer::Address, String)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PinepalDevicesPage {
        const NAME: &'static str = "PinepalDevicesPage";
        type Type = super::PinepalDevicesPage;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for PinepalDevicesPage {}
    impl WidgetImpl for PinepalDevicesPage {}
    impl BoxImpl for PinepalDevicesPage {}
}

glib::wrapper! {
    pub struct PinepalDevicesPage(ObjectSubclass<imp::PinepalDevicesPage>)
        @extends gtk::Widget, gtk::Box;
}

impl PinepalDevicesPage {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    pub fn set_scanning(&self, scanning: bool) {
        let imp = self.imp();
        imp.scan_spinner.set_spinning(scanning);
        if scanning {
            imp.status_page.set_title("Looking for InfiniTime…");
            imp.status_page.set_description(Some(
                "Make sure your PineTime is nearby and Bluetooth is enabled.",
            ));
        }
    }

    pub fn set_reconnecting(&self, attempt: u32, delay_secs: u64) {
        let imp = self.imp();
        imp.scan_spinner.set_spinning(true);
        imp.status_page.set_title("Reconnecting…");
        imp.status_page.set_description(Some(&format!(
            "Attempt {attempt}, retrying in {delay_secs}s"
        )));
        imp.device_list.set_visible(false);
    }

    pub fn set_bluetooth_off(&self) {
        let imp = self.imp();
        imp.scan_spinner.set_spinning(false);
        imp.status_page.set_title("Bluetooth is Off");
        imp.status_page
            .set_description(Some("Turn on Bluetooth to connect to your PineTime."));
        imp.status_page.set_icon_name(Some("bluetooth-disabled-symbolic"));
    }

    pub fn set_error(&self, message: &str) {
        let imp = self.imp();
        imp.scan_spinner.set_spinning(false);
        imp.status_page.set_title("Connection Error");
        imp.status_page.set_description(Some(message));
        imp.status_page.set_icon_name(Some("dialog-error-symbolic"));
    }

    /// Add a discovered device to the list. Returns true if it's new.
    pub fn add_device(&self, address: bluer::Address, name: &str) -> bool {
        let imp = self.imp();
        let mut devices = imp.devices.borrow_mut();

        // Check for duplicate
        if devices.iter().any(|(a, _)| *a == address) {
            return false;
        }
        devices.push((address, name.to_string()));

        let row = adw::ActionRow::builder()
            .title(name)
            .subtitle(&address.to_string())
            .activatable(true)
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

        imp.device_list.append(&row);
        imp.device_list.set_visible(true);
        imp.status_page.set_title("Devices Found");
        imp.status_page.set_description(Some("Tap a device to connect"));

        true
    }

    /// Get the address for a device at a given index.
    pub fn device_address_at(&self, index: usize) -> Option<bluer::Address> {
        self.imp().devices.borrow().get(index).map(|(a, _)| *a)
    }

    /// Connect the list's row-activated signal.
    pub fn connect_device_activated<F: Fn(bluer::Address) + 'static>(&self, f: F) {
        let page = self.clone();
        self.imp().device_list.connect_row_activated(move |_, row| {
            let idx = row.index() as usize;
            if let Some(addr) = page.device_address_at(idx) {
                f(addr);
            }
        });
    }

    pub fn clear_devices(&self) {
        let imp = self.imp();
        imp.devices.borrow_mut().clear();
        // Remove all rows
        while let Some(child) = imp.device_list.first_child() {
            imp.device_list.remove(&child);
        }
        imp.device_list.set_visible(false);
        imp.status_page.set_icon_name(Some("bluetooth-symbolic"));
    }
}

impl Default for PinepalDevicesPage {
    fn default() -> Self {
        Self::new()
    }
}
