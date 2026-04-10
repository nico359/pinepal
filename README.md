# PinePal

Companion app for [PineTime](https://www.pine64.org/pinetime/) smartwatches running [InfiniTime](https://github.com/InfiniTimeOrg/InfiniTime). Built with GTK4/libadwaita and Rust, aimed at mobile Linux devices such as the PinePhone.

## Features

- BLE connection with automatic reconnection and exponential backoff
- Live battery, heart rate, and step count display
- Step history chart with daily persistence (7d / 30d / all time)
- Desktop notification forwarding to watch
- Background mode (keeps connection alive when window is closed)

## Credits

Based on the work of [Watchmate](https://github.com/azymohliad/watchmate) by Andrii Zymohliad.

## AI Disclosure

This application was built with the assistance of AI (GitHub Copilot CLI, Claude Opus 4.6).

## Building

The easiest way to build the app is by using GNOME Builder IDE.
