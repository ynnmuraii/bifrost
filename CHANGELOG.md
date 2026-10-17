# Changelog

## Unreleased

- Wayland display discovery falls back from InputCapture zones to permission-independent xdg-output logical geometry; source selection and failures are logged.
- Map cards explicitly report when displays are not detected.
- Paired computers absent from the stored layout now use established-link membership for online status instead of always appearing offline.
- Core replaces probe estimates with actual agent capture/injection/emergency capabilities. Missing or older InputCapture portals remain receiver-only; Hello advertises source eligibility rather than configuration alone.
- GUI display queries wait for agent readiness instead of generating a misleading startup `CaptureFailed`.

## 0.1.1 — 2026-10-16

- Fixed the settings version display: it now sits in a padded, localized bottom row instead of overlapping the modal frame.
- Portable Windows executable available directly, alongside the archive with notices and example settings.
- Per-user Linux installation and removal with `install.sh`; existing settings and pairing are preserved.
- English project introduction, screenshots and clearer platform and installation requirements.
- A fresh GUI starts its core automatically and creates settings without a manual CLI step.
- Settings now offers a per-user Start at login toggle, pointing at the running executable.
- Licensed under MIT.

## 0.1.0 — 2026-10-16

First release of **Bifrost**, a software KVM for sharing a keyboard, mouse and clipboard between trusted computers on a local network.

- Windows 11 and Linux: X11, KDE Plasma Wayland and GNOME Wayland. Portal support varies: GNOME 46 can receive input but cannot capture it for another computer.
- Secure mTLS connections with persistent peer trust and pairing by a 6-digit comparison code, confirmed on both computers.
- A draggable monitor map with links across gaps, adjustable edge segments and an ASCII bridge between computers.
- Shared clipboard with history, configurable hotkeys and an edge-glow indicator.
- Russian and English interfaces, light and dark themes, and a system tray. System autostart uses Windows Run/scheduled tasks or a Linux desktop entry; the deployment helper migrates existing entries.
- One executable, `bifrost` / `bifrost.exe`, for the window, core, agent and tray. Existing settings and pairing remain in the legacy `lan-workspace` directories. Wayland asks for portal permissions once after this app-id change.

This is an early release. Check emergency-release hotkeys on your desktop before relying on remote input; portal availability and permissions determine Wayland capabilities.
