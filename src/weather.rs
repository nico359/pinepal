// SPDX-License-Identifier: GPL-3.0-or-later
// Weather fetch + geocoding via open-meteo (free, no API key).
// ponytail: single provider is enough here; pinetime-furios supports met.no/NWS
// too, add a provider switch if open-meteo's coverage ever falls short.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_FORECAST_DAYS: usize = 5;
const USER_AGENT: &str = "pinepal/0.4 (+https://github.com/nico359/pinepal)";

#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct ForecastDay {
    pub min: i16,
    pub max: i16,
    pub icon: u8,
}

/// Weather ready to encode for the watch. Temperatures are int16 centidegrees C.
#[derive(Clone, Debug)]
pub struct WeatherData {
    pub timestamp: u64,
    pub location: String,
    pub current_temp: i16,
    pub today_min: i16,
    pub today_max: i16,
    pub current_icon: u8,
    pub forecast: Vec<ForecastDay>,
}

impl WeatherData {
    /// Human-readable summary for the dashboard row, e.g. "21°C, Berlin".
    pub fn summary(&self) -> String {
        format!("{}°C, {}", self.current_temp / 100, self.location)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("building http client")
}

/// Resolve a free-form address/city query to coordinates.
pub async fn geocode(query: &str) -> Result<Location> {
    let resp: GeoResponse = client()?
        .get("https://geocoding-api.open-meteo.com/v1/search")
        .query(&[("name", query.trim()), ("count", "1"), ("format", "json")])
        .send()
        .await
        .context("geocoding request")?
        .error_for_status()
        .context("geocoding status")?
        .json()
        .await
        .context("geocoding json")?;

    let r = resp
        .results
        .and_then(|r| r.into_iter().next())
        .context("no matching place found")?;
    Ok(Location { lat: r.latitude, lon: r.longitude, name: r.name })
}

/// Fetch current + daily forecast for a location.
pub async fn fetch(loc: &Location) -> Result<WeatherData> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={:.4}&longitude={:.4}\
         &current=temperature_2m,weather_code\
         &daily=weather_code,temperature_2m_max,temperature_2m_min\
         &timezone=auto&forecast_days=7",
        loc.lat, loc.lon
    );
    let resp: OmResponse = client()?
        .get(&url)
        .send()
        .await
        .context("open-meteo request")?
        .error_for_status()
        .context("open-meteo status")?
        .json()
        .await
        .context("open-meteo json")?;

    anyhow::ensure!(!resp.daily.time.is_empty(), "open-meteo returned no daily data");

    let forecast = (1..resp.daily.time.len().min(1 + MAX_FORECAST_DAYS))
        .map(|i| ForecastDay {
            min: to_centi(resp.daily.temperature_2m_min[i]),
            max: to_centi(resp.daily.temperature_2m_max[i]),
            icon: wmo_to_icon(resp.daily.weather_code[i]),
        })
        .collect();

    Ok(WeatherData {
        timestamp: now_secs(),
        location: loc.name.clone(),
        current_temp: to_centi(resp.current.temperature_2m),
        today_min: to_centi(resp.daily.temperature_2m_min[0]),
        today_max: to_centi(resp.daily.temperature_2m_max[0]),
        current_icon: wmo_to_icon(resp.current.weather_code),
        forecast,
    })
}

fn to_centi(celsius: f64) -> i16 {
    (celsius * 100.0).round() as i16
}

/// Map an open-meteo WMO weather code to InfiniTime's weather icon index.
fn wmo_to_icon(code: u8) -> u8 {
    match code {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        45 | 48 => 8,
        51 | 53 | 55 | 56 | 57 => 5,
        61 | 63 | 66 | 67 => 5,
        65 => 4,
        71 | 73 | 75 | 77 => 7,
        80 | 81 => 5,
        82 => 4,
        85 | 86 => 7,
        95 | 96 | 99 => 6,
        _ => 255,
    }
}

#[derive(Deserialize)]
struct GeoResponse {
    results: Option<Vec<GeoResult>>,
}
#[derive(Deserialize)]
struct GeoResult {
    latitude: f64,
    longitude: f64,
    name: String,
}

#[derive(Deserialize)]
struct OmResponse {
    current: OmCurrent,
    daily: OmDaily,
}
#[derive(Deserialize)]
struct OmCurrent {
    temperature_2m: f64,
    weather_code: u8,
}
#[derive(Deserialize)]
struct OmDaily {
    time: Vec<String>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
    weather_code: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wmo_icon_mapping() {
        assert_eq!(wmo_to_icon(0), 0);
        assert_eq!(wmo_to_icon(61), 5);
        assert_eq!(wmo_to_icon(200), 255);
    }

    #[test]
    fn centi_rounding() {
        assert_eq!(to_centi(21.234), 2123);
        assert_eq!(to_centi(-3.5), -350);
    }
}
