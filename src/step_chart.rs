// SPDX-License-Identifier: GPL-3.0-or-later
// Cairo bar chart for step history.

use chrono::{Days, Local, NaiveDate};
use gtk::prelude::*;

/// Draw a bar chart of daily steps onto a DrawingArea.
/// `data` is a slice of (date, steps) pairs, already sorted by date.
/// `range_days` is the number of days to show (0 = all time).
pub fn setup_step_chart(
    drawing_area: &gtk::DrawingArea,
    data: &[(NaiveDate, u32)],
    range_days: u32,
) {
    let data = data.to_vec();
    drawing_area.set_draw_func(move |_widget, cr, width, height| {
        let w = width as f64;
        let h = height as f64;

        // Background
        cr.set_source_rgb(0.96, 0.96, 0.96);
        let _ = cr.paint();

        let today = Local::now().date_naive();
        let bars: Vec<(NaiveDate, u32)> = if range_days > 0 {
            let from = today
                .checked_sub_days(Days::new((range_days - 1) as u64))
                .unwrap_or(today);
            fill_missing_dates(&data, &from, &today)
        } else if data.is_empty() {
            return;
        } else {
            fill_missing_dates(&data, &data[0].0, &today)
        };

        if bars.is_empty() {
            // Draw "no data" text
            cr.set_source_rgb(0.5, 0.5, 0.5);
            cr.set_font_size(14.0);
            let text = "No step data yet";
            let extents = cr.text_extents(text).unwrap();
            cr.move_to((w - extents.width()) / 2.0, (h + extents.height()) / 2.0);
            let _ = cr.show_text(text);
            return;
        }

        let max_steps = bars.iter().map(|(_, s)| *s).max().unwrap_or(1).max(1) as f64;
        let n = bars.len() as f64;

        let margin_left = 45.0;
        let margin_right = 10.0;
        let margin_top = 10.0;
        let margin_bottom = 30.0;

        let chart_w = w - margin_left - margin_right;
        let chart_h = h - margin_top - margin_bottom;
        let bar_total_w = chart_w / n;
        let bar_gap = (bar_total_w * 0.2).max(1.0);
        let bar_w = bar_total_w - bar_gap;

        // Draw grid lines
        cr.set_source_rgb(0.85, 0.85, 0.85);
        cr.set_line_width(0.5);
        for i in 0..=4 {
            let y = margin_top + chart_h * (1.0 - i as f64 / 4.0);
            cr.move_to(margin_left, y);
            cr.line_to(w - margin_right, y);
            let _ = cr.stroke();

            // Y-axis labels
            cr.set_source_rgb(0.4, 0.4, 0.4);
            cr.set_font_size(10.0);
            let label = format!("{}", (max_steps * i as f64 / 4.0) as u32);
            let ext = cr.text_extents(&label).unwrap();
            cr.move_to(margin_left - ext.width() - 4.0, y + ext.height() / 2.0);
            let _ = cr.show_text(&label);
            cr.set_source_rgb(0.85, 0.85, 0.85);
        }

        // Draw bars
        for (i, (date, steps)) in bars.iter().enumerate() {
            let bar_h = (*steps as f64 / max_steps) * chart_h;
            let x = margin_left + i as f64 * bar_total_w + bar_gap / 2.0;
            let y = margin_top + chart_h - bar_h;

            // Bar color — highlight today
            if *date == today {
                cr.set_source_rgb(0.20, 0.60, 0.86); // Accent blue
            } else {
                cr.set_source_rgb(0.40, 0.76, 0.64); // Teal
            }

            cr.rectangle(x, y, bar_w, bar_h);
            let _ = cr.fill();

            // X-axis date labels (sparse for readability)
            let show_label = n <= 14.0
                || (n <= 40.0 && i % 5 == 0)
                || (n > 40.0 && i % 10 == 0)
                || i == bars.len() - 1;

            if show_label {
                cr.set_source_rgb(0.4, 0.4, 0.4);
                cr.set_font_size(9.0);
                let label = date.format("%m/%d").to_string();
                let ext = cr.text_extents(&label).unwrap();
                let lx = x + bar_w / 2.0 - ext.width() / 2.0;
                cr.move_to(lx, h - margin_bottom + ext.height() + 4.0);
                let _ = cr.show_text(&label);
            }
        }
    });
}

/// Fill in missing dates with 0 steps.
fn fill_missing_dates(
    data: &[(NaiveDate, u32)],
    from: &NaiveDate,
    to: &NaiveDate,
) -> Vec<(NaiveDate, u32)> {
    use std::collections::HashMap;
    let lookup: HashMap<NaiveDate, u32> = data.iter().cloned().collect();
    let mut result = Vec::new();
    let mut date = *from;
    while date <= *to {
        let steps = lookup.get(&date).copied().unwrap_or(0);
        result.push((date, steps));
        date = date.succ_opt().unwrap_or(date);
        if date == *from {
            break; // safety
        }
    }
    result
}
