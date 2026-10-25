# Changelog

## Unreleased

- Live latency telemetry: each link now times its own `Ping`/`Pong` pairs and reports the 95th-percentile round trip and the jitter (mean absolute difference of consecutive samples) over the last 30 seconds. The status screen shows «Задержка: N мс, джиттер M мс» and the map's «≈ N мс» pill appears once a link has answered three pings; until then both stay blank instead of showing zero. A reconnected link starts measuring from scratch, and the core's `status` verb prints the same pair as `rtt_us=`/`jitter_us=`.

## 0.1.6 — 2026-10-24

- Fixed `--root=/path` (and the other path flags) being ignored by the window and the core while the tray honoured it. Every role now accepts both `--root /path` and `--root=/path`, so the window, the tray and the core it starts always agree on their directories.
- VPN interfaces (Tailscale, WireGuard, ZeroTier, OpenVPN) are listed in the settings address picker instead of being hidden, and an adapter whose name merely contains “tun” or “wg” is no longer dropped. The physical LAN address still ranks first, so the default does not move onto the tunnel.
- The login-start entry now records the window class and the resolved executable path, matching the desktop entry the installer writes, so desktop environments group the window under Bifrost's icon.
- The status screen now names this computer on a follower in a workspace of three or more computers, instead of showing its id only while it owned input.
- Fixed the clipboard being echoed back to the other computer several times when a clipboard manager (Klipper, CopyQ) re-owns the selection after a remote copy.
- Releases now include Ed25519-signed checksums; Bifrost 0.1.6 and newer refuse unsigned or altered updates.
- Fixed Windows update restarts occasionally failing while the previous service was still shutting down.
- Applying a LAN address now restarts a service started outside the window and keeps systemd supervision on Linux.
- Service-stop errors now show the real reason, program, process ID and exit status.
- GUI-launched services no longer leave zombie processes after exiting on Linux.

## 0.1.5 — 2026-10-24

- Extended physical monitor-map zoom to 3×, shown on a remapped 50–200% control with the default view at 100%; existing saved views keep their scale.
- Map labels fade out secondary details and then names as zoom decreases, keeping small layouts uncluttered.
- Appearance now uses the shared settings modal, with consistent rows and controls in both languages and themes.
- Theme changes sweep across the window in a short, reversible ASCII-cell wave from the top-right theme button.

## 0.1.4 — 2026-10-21

- Added verified self-updates. Install this version manually once: 0.1.3 has no updater. Later versions can be installed from Settings or the startup update notice.
- Update downloads are checked against SHA256SUMS before installation; Bifrost keeps a backup and restarts its running roles safely.
- Updated TLS security dependencies rustls and rustls-webpki.

## 0.1.3 — 2026-10-19

- Fixed emergency-hotkey registration on KDE Wayland when Bifrost is launched from its desktop entry or an `app-bifrost@….service` unit. Shortcut refusals now show the portal's actual reason; GNOME 46 still has no GlobalShortcuts portal.
- Added `switch --to local|<peer>` for choosing an input destination explicitly; the tray's return action always selects local input.
- Unpairing removes the computer and its links from the shared monitor map, including stale entries, without revoking other computers' direct trust. Pair again to restore placement. `peers --revoke` now requires a running core.
- Fixed pairing and monitor-layout edits getting stuck after their window or caller closes.
- Successful portal grants and ready input agents clear stale refusal messages.
- Clipboard settings follow live status updates, and unresolved automatic links show why they are inactive.
- Small accent-coloured text is easier to read in the light theme, and zoomed or panned monitors only accept clicks inside the visible map.
- Added rotating window logs at `<state>/logs/lws-gui.log` and better diagnostics for connection and clipboard failures.

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
