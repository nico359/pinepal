// SPDX-License-Identifier: GPL-3.0-or-later

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::step_chart;
use crate::step_db::StepDb;

mod imp {
    use super::*;

    #[derive(Debug, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/nico359/pinepal/dashboard_page.ui")]
    pub struct PinepalDashboardPage {
        #[template_child]
        pub firmware_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub battery_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub battery_level: TemplateChild<gtk::LevelBar>,
        #[template_child]
        pub heart_rate_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub steps_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub notif_switch: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub range_7d: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub range_30d: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub range_all: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub step_chart: TemplateChild<gtk::DrawingArea>,
        #[template_child]
        pub disconnect_button: TemplateChild<gtk::Button>,

        pub step_db: RefCell<Option<Rc<StepDb>>>,
        pub range_days: Cell<u32>,
    }

    impl Default for PinepalDashboardPage {
        fn default() -> Self {
            Self {
                firmware_row: Default::default(),
                battery_row: Default::default(),
                battery_level: Default::default(),
                heart_rate_row: Default::default(),
                steps_row: Default::default(),
                notif_switch: Default::default(),
                range_7d: Default::default(),
                range_30d: Default::default(),
                range_all: Default::default(),
                step_chart: Default::default(),
                disconnect_button: Default::default(),
                step_db: RefCell::new(None),
                range_days: Cell::new(7),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PinepalDashboardPage {
        const NAME: &'static str = "PinepalDashboardPage";
        type Type = super::PinepalDashboardPage;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for PinepalDashboardPage {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_range_buttons();
        }
    }
    impl WidgetImpl for PinepalDashboardPage {}
    impl BoxImpl for PinepalDashboardPage {}
}

glib::wrapper! {
    pub struct PinepalDashboardPage(ObjectSubclass<imp::PinepalDashboardPage>)
        @extends gtk::Widget, gtk::Box;
}

impl PinepalDashboardPage {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    pub fn set_step_db(&self, db: Rc<StepDb>) {
        self.imp().step_db.replace(Some(db));
    }

    fn setup_range_buttons(&self) {
        let imp = self.imp();
        let page = self.clone();
        let page2 = self.clone();
        let page3 = self.clone();

        // Make toggle buttons mutually exclusive
        imp.range_30d.set_group(Some(&*imp.range_7d));
        imp.range_all.set_group(Some(&*imp.range_7d));

        imp.range_7d.connect_toggled(move |btn| {
            if btn.is_active() {
                page.set_range(7);
            }
        });
        imp.range_30d.connect_toggled(move |btn| {
            if btn.is_active() {
                page2.set_range(30);
            }
        });
        imp.range_all.connect_toggled(move |btn| {
            if btn.is_active() {
                page3.set_range(0);
            }
        });
    }

    fn set_range(&self, days: u32) {
        self.imp().range_days.set(days);
        self.refresh_chart();
    }

    pub fn set_firmware(&self, version: &str) {
        self.imp().firmware_row.set_subtitle(version);
    }

    pub fn set_battery(&self, level: u8) {
        let imp = self.imp();
        imp.battery_row.set_subtitle(&format!("{}%", level));
        imp.battery_level.set_value(level as f64);
    }

    pub fn set_heart_rate(&self, bpm: u8) {
        self.imp()
            .heart_rate_row
            .set_subtitle(&format!("{} bpm", bpm));
    }

    pub fn set_steps(&self, steps: u32) {
        self.imp()
            .steps_row
            .set_subtitle(&format!("{}", steps));
    }

    pub fn bind_settings(&self, settings: &gio::Settings) {
        settings
            .bind("forward-notifications", &*self.imp().notif_switch, "active")
            .build();
    }

    pub fn refresh_chart(&self) {
        let imp = self.imp();
        let db = imp.step_db.borrow();
        let range = imp.range_days.get();

        let data = if let Some(ref db) = *db {
            if range == 0 {
                db.get_all_steps().unwrap_or_default()
            } else {
                let today = chrono::Local::now().date_naive();
                let from = today
                    .checked_sub_days(chrono::Days::new((range - 1) as u64))
                    .unwrap_or(today);
                db.get_steps_range(&from, &today).unwrap_or_default()
            }
        } else {
            Vec::new()
        };

        step_chart::setup_step_chart(&imp.step_chart, &data, range);
        imp.step_chart.queue_draw();
    }

    pub fn connect_disconnect<F: Fn() + 'static>(&self, f: F) {
        self.imp().disconnect_button.connect_clicked(move |_| f());
    }
}

impl Default for PinepalDashboardPage {
    fn default() -> Self {
        Self::new()
    }
}
