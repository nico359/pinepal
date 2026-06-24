# PinePal

Companion app for [PineTime](https://www.pine64.org/pinetime/) smartwatches running [InfiniTime](https://github.com/InfiniTimeOrg/InfiniTime). Built with GTK4/libadwaita and Rust, aimed at mobile Linux devices such as the PinePhone.

## Features

- BLE connection with automatic reconnection and exponential backoff
- Live battery, heart rate, and step count display
- Step history chart with daily persistence (7d / 30d / all time)
- Desktop notification forwarding to watch
- Background mode (keeps connection alive when window is closed)
- Said background mode can also be started manually by running flatpak run io.github.nico359.pinepal --gapplication-service
- Autostart via Background Portal - might not work in e.g. Phosh because the Portal is not implemented there yet)

## To be improved

- Firmware Update of the watch
- Maybe also heart rate history with the new feature of InfiniTime 1.16
- Music controls
- Accepting or declining calls (if that is even possible)
- Synchronising time of the watch on connect

## Screenshots

<div style="display: flex; flex-wrap: wrap; gap: 20px;">
  <img src="data/dashboard.png" width="500" style="flex: 1; min-width: 250px;" />
  <img src="data/searching.png" width="500" style="flex: 1; min-width: 250px;" />
  <img src="data/bluetoothdisabled.png" width="500" style="flex: 1; min-width: 250px;" />
  <img src="data/mobile.png" width="160" style="flex: 1;" />
</div>

## Credits

Based on the work of [Watchmate](https://github.com/azymohliad/watchmate) by Andrii Zymohliad.

## AI Disclosure

This application was built with the assistance of AI (GitHub Copilot CLI, Claude).

## Building

The easiest way to build the app is by using GNOME Builder IDE or flatpak-builder.

Example using flatpak-builder as a flatpak:
-  Install flatpak-builder
```
flatpak install org.flatpak.Builder
```

-  Compile the project into a local repo
```
flatpak run org.flatpak.Builder --repo=repo --force-clean --user build io.github.nico359.pinepal.json
```

-  Then create a bundle which you can install
```
flatpak build-bundle repo pinepal.flatpak io.github.nico359.pinepal
```
