// SPDX-License-Identifier: GPL-3.0-or-later

use anyhow::{Context, Result};
use chrono::NaiveDate;
use rusqlite::{params, Connection};
use std::path::PathBuf;

#[derive(Debug)]
pub struct StepDb {
    conn: Connection,
}

impl StepDb {
    pub fn open() -> Result<Self> {
        let path = Self::db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("Failed to open step database at {:?}", path))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS steps_daily (
                date  TEXT PRIMARY KEY,
                steps INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    fn db_path() -> Result<PathBuf> {
        let data_dir = glib::user_data_dir().join("pinepal");
        Ok(data_dir.join("steps.db"))
    }

    /// Upsert a daily step count, keeping the maximum value seen.
    pub fn upsert_steps(&self, date: &NaiveDate, steps: u32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO steps_daily (date, steps) VALUES (?1, ?2)
             ON CONFLICT(date) DO UPDATE SET steps = MAX(steps, excluded.steps)",
            params![date.format("%Y-%m-%d").to_string(), steps],
        )?;
        Ok(())
    }

    /// Get daily steps for a date range (inclusive).
    pub fn get_steps_range(
        &self,
        from: &NaiveDate,
        to: &NaiveDate,
    ) -> Result<Vec<(NaiveDate, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT date, steps FROM steps_daily
             WHERE date >= ?1 AND date <= ?2
             ORDER BY date ASC",
        )?;
        let rows = stmt.query_map(
            params![
                from.format("%Y-%m-%d").to_string(),
                to.format("%Y-%m-%d").to_string()
            ],
            |row| {
                let date_str: String = row.get(0)?;
                let steps: u32 = row.get(1)?;
                Ok((date_str, steps))
            },
        )?;
        let mut result = Vec::new();
        for row in rows {
            let (date_str, steps) = row?;
            if let Ok(date) = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") {
                result.push((date, steps));
            }
        }
        Ok(result)
    }

    /// Get all daily steps, ordered by date.
    pub fn get_all_steps(&self) -> Result<Vec<(NaiveDate, u32)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT date, steps FROM steps_daily ORDER BY date ASC")?;
        let rows = stmt.query_map([], |row| {
            let date_str: String = row.get(0)?;
            let steps: u32 = row.get(1)?;
            Ok((date_str, steps))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (date_str, steps) = row?;
            if let Ok(date) = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") {
                result.push((date, steps));
            }
        }
        Ok(result)
    }
}

use gtk::glib;
