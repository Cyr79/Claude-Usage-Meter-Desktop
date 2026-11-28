# Claude Usage Meter — Desktop v2.0.0

Your Claude.ai usage limits in the Windows system tray, with a daily pace check
and a live context meter for Claude Code.
GitHub: [Cyr79/Claude-Usage-Meter-Desktop](https://github.com/Cyr79/Claude-Usage-Meter-Desktop)

> **Windows only.**

## What's new in 2.0.0

- **Rewritten with Tauri (Rust + system WebView2)** instead of Electron. Same features, a fraction of the size and memory.
- **Per-day Weekly segments:** the Weekly bar is split by day (past days as wide as what they used, today as wide as today's budget, the rest shared over the coming days).
- **Adaptive polling:** slower when you are idle or the session is locked, backs off on errors and rate limits, wakes just after a reset.
- **No webview when idle:** the gauge window is destroyed when it closes, so the tray app runs with no WebView2 process at all. A browser window exists only while the gauge or the sign-in page is open.

## Resources

Windows, app idle in the tray, gauge closed, signed in. Same machine for both rows.

| | Electron 1.4.1 | Tauri 2.0.0 |
|---|---|---|
| Processes | 4 | **1** |
| Private memory | 234 MB | **7 MB** |
| Working set | 322 MB | **71 MB** |
| CPU (idle) | 0.003 % | 0.002 % |
| Installer | 226 MB | **2 MB** |

Idle CPU is equal for both (no CPU advantage claimed). Tauri with the gauge popup open: about 107 MB (measured by Claude Code); it is all released when the popup closes.

**Method:** `meas.ps1` (PowerShell, Windows) sums the whole process tree of the app, including its `msedgewebview2.exe` children, 30 s after start, then averages CPU over 60 s (`meas.ps1 -sec 60`).
The Tauri installer needs the Microsoft WebView2 runtime (already present on Windows 11; the installer fetches it if missing).

## Features

- **Usage badges:** one tray badge per limit (Session 5h, Weekly, Weekly Opus, Fable). Amber from 80%, red from 95%, grey when stale.
- **Energy gauge:** left-click a badge. White tick = where you should be if usage were spread evenly. Pin it to keep it on screen.
- **Daily pace:** hover a badge for average per day, today's allowance and time left.
- **Claude Code context:** live context-window meter.
- **Menu:** right-click a badge (display options, open Claude Desktop, About, Quit).

## Install

1. Download `Claude-Usage-Meter_2.0.0_x64-setup.exe` from [Releases](https://github.com/Cyr79/Claude-Usage-Meter-Desktop/releases).
> **Electron users:** v2.0.0 must be installed manually once. The old Electron updater will not migrate you; from 2.0.0 on, updates are automatic.

2. Quit the running meter (right-click a badge → Quit) and run the installer (per-user, no admin). Settings and sign-in are kept.
3. Sign in once when the window opens; it closes by itself and the meter polls in the background.

**SmartScreen:** the installer is not code-signed, so Windows may warn (*More info → Run anyway*). A Microsoft Store (MSIX) release is planned; it will be signed by Microsoft.

## Build

```
npm install
npm run tauri build      # NSIS installer in src-tauri/target/release/bundle/nsis
cargo test               # in src-tauri
```

## Credits and privacy

Fork of [Claude Usage Meter](https://github.com/SelectorsHub/Claude-Usage-Meter-Desktop) by SelectorsHub (MIT). Independent project, not affiliated with Anthropic.
The app talks only to claude.ai with your own sign-in and to GitHub for updates. Your login is stored on your PC (Windows DPAPI) and never sent anywhere else.

## Known limits

- Free plans don't work: `/usage` returns nothing for unmetered accounts.
- The context meter covers Claude Code only.

## License

MIT, as the upstream project.
