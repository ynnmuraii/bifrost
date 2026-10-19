# Changelog

## Unreleased

- Unpairing removes the computer and its links through the coordinator's shared layout revision, including on members that retain their own direct trust; stale display reports cannot restore the removed map card. Trust removal stays local and pairing again restores fresh placement. `peers --revoke` requires a running core so removal cannot silently leave the shared map unchanged.
- Successful portal grants no longer appear as refusals in `status`, and a ready input agent clears stale refusal/degraded details.
- Repeated cleanup also removes orphaned layout entries whose trust record is already absent; late portal token-store notifications cannot restore a refusal after the input agent is ready.
- Register the Wayland app id before portal requests so KDE can attribute emergency shortcuts to Bifrost; report refused or unbound shortcuts with their actual portal reason.
- Added directional `switch --to local|<peer>` commands; tray returns explicitly to local input rather than toggling against a possibly expired lease.
- Pairing handshakes abandoned after a window replacement no longer release the new window's pending slot.
- Late layout verdicts retain their request after its caller disconnects, allowing an applied edit to release the pending slot.
- Clipboard settings follow pushed status updates without an extra status request.
- Accent-coloured small text is now readable in the light theme: the countdown, the «ввод» tag and the map's notices use a darker orange (5.0:1 or better on the light background) while fills, borders and the bridge keep the brand accent.
- The window writes its own log to `<state>/logs/lws-gui.log`, using the daemon's rotation settings, and records native close requests with their origin, tray readiness and decision, plus show signals and minimized-state changes.
- Failed two-node integration tests now preserve both computers' status and the tails of their core and agent logs before cleanup, including clipboard and fault-injection timeouts.
- Unresolved automatic links are shown inactive with a reason instead of appearing usable.
- Zoomed or panned monitors only accept pointer input inside the map's visible bounds.

## 0.1.2 — 2026-10-18

- Wayland displays are now detected on GNOME even without screen-capture permission, using the compositor's logical output geometry; failures are reported instead of leaving the map empty.
- A computer's online status is now the same on both ends of a link, instead of one side showing a paired computer as always offline.
- Computers that can only receive input (for example GNOME 46, which cannot capture the screen) no longer drop the connection; eligibility is based on the capabilities a computer actually reports.
- The map labels each computer with its operating system and each monitor with the real resolution and scaling (for example 2560x1600 at 150%), while layout and input stay in logical coordinates.
- A peer's displays are refreshed after it restarts instead of being treated as stale.
- The input lease default is raised to 1000 ms (still configurable from 300–1000 ms) and the challenge window is fixed, so brief Wi-Fi stalls no longer end remote control prematurely. This tolerates a short stall, not unlimited connection loss.
- Returning the pointer across an edge no longer locks that edge out: it re-arms after a short retreat or a brief pause, and a carried-over push is briefly refused.
- Added diagnostics that record edge returns and lease expiries to help investigate future drops.

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
