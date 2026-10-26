# Changelog

Each release is described in English and Russian. / Каждый выпуск описан на английском и русском.

## Unreleased

### English

**Fixes**

- On compatible versions, unpairing on one computer now removes the pair on the other too: immediately while online, or on its next connection within 30 days. The other computer shows a one-time notice.
- For peers running older versions, a manual **Remove** suggestion appears after a prolonged offline period and a certificate rejection. Network failures alone never remove a pair automatically.

### Русский

**Исправления**

- На совместимых версиях удаление сопряжения на одном компьютере теперь удаляет пару и на другом: сразу при наличии связи или при следующем подключении в течение 30 дней. Другой компьютер показывает однократное уведомление.
- Для компьютеров со старыми версиями после длительного отсутствия связи и отказа сертификата появляется ручная подсказка **«Удалить»**. Одни лишь сетевые ошибки никогда не удаляют пару автоматически.

## 0.3.0 — 2026-10-26

### English

**New**

- Game mode prevents accidental edge switches in fullscreen applications on Windows/X11 and cursor-confined applications on Windows. Settings offer automatic, always-on and off modes plus per-app exceptions. Ctrl+Alt+Shift+G and the tray toggle manual mode; explicit switching and emergency release still work. Wayland supports manual mode only, without automatic detection.
- Clipboard history's **Send to…** sends one item to a chosen online trusted computer. Sending the current copied item stops it following the cursor until the next copy; sending an older item leaves the current carry unchanged.

**Changes**

- The text clipboard now **follows the cursor by default**, instead of being sent to every computer at copy time. Passing through a screen does not overwrite its clipboard: delivery starts on the first key/button press or after about 0.7 seconds. To restore the old behaviour, choose **Settings → Clipboard → «всем сразу»** (broadcast), or set `policy = "broadcast"` under `[clipboard]` in the configuration.
- On returning to a Wayland computer, the very first paste within about 0.7 seconds may still paste the previous content: local key presses may be unavailable, so delivery waits for the cursor to settle.

**Fixes**

- Compatible computers confirm clipboard delivery before the first routed key/button press is released, reducing pastes of stale text. Older computers remain supported; local physical input is not delayed. Secret-marked text is never carried or retained.

### Русский

**Новое**

- Игровой режим предотвращает случайный переход по краю из полноэкранных приложений на Windows/X11 и приложений с ограничением курсора на Windows. В настройках доступны автоматический, постоянный и выключенный режимы, а также исключения для приложений. Ctrl+Alt+Shift+G и пункт трея переключают ручной режим; явное переключение и аварийный возврат работают всегда. На Wayland доступен только ручной режим, без автоопределения.
- «Отправить на…» в истории буфера передаёт одну запись выбранному доверенному компьютеру в сети. Отправка текущей копии прекращает её перенос за курсором до следующего копирования; отправка старой записи не меняет текущий перенос.

**Изменения**

- Теперь текстовый буфер **по умолчанию следует за курсором**, а не рассылается всем компьютерам в момент копирования. Проход через экран не перезаписывает его буфер: доставка начинается при первом нажатии клавиши/кнопки или примерно через 0,7 секунды. Чтобы вернуть прежнее поведение, выберите **Настройки → Буфер обмена → «всем сразу»** или укажите `policy = "broadcast"` в разделе `[clipboard]` конфигурации.
- При возврате на компьютер с Wayland самая первая вставка в течение примерно 0,7 секунды ещё может вставить прежнее содержимое: местные нажатия могут быть недоступны, поэтому доставка ждёт остановки курсора.

**Исправления**

- Совместимые компьютеры подтверждают доставку буфера до передачи первого перенаправленного нажатия клавиши/кнопки, уменьшая вероятность вставки старого текста. Старые компьютеры по-прежнему поддерживаются; местный физический ввод не задерживается. Помеченные секреты не переносятся и не сохраняются.

## 0.2.0 — 2026-10-25

### English

**New**

- Computers are discovered on the local network and paired with one click; entering an address by hand still works. Allow UDP 47902 through the firewall for discovery.
- Live p95 RTT and jitter figures on the status screen, and link latency on the map.
- Update badges on the map and "Update all computers": connected trusted computers update first, then the current one. Each computer checks the official release, the Ed25519 signature and the checksum itself; progress and the reasons for a refusal are shown separately.
- Computers on 0.1.x must be updated to 0.2.0 by hand once: 0.1.6 offers the release in Settings or in the startup notice. After that, "Update all computers" works.

**Fixes**

- Trusted computers are found again after an IP address change; a new address is accepted only after authentication.
- Update badges and latency figures no longer overlap labels or neighbouring screens on the map.
- Update buttons stay reachable in a small Settings window; an update safely waits for running components to finish.

### Русский

**Новое**

- Поиск компьютеров в локальной сети и сопряжение одним нажатием; ручной ввод адреса остаётся доступен. Для обнаружения разрешите UDP 47902 в брандмауэре.
- Живые показатели задержки RTT p95 и джиттера на экране статуса, задержка соединений на карте.
- Значки доступных обновлений на карте и «Обновить все компьютеры»: сначала обновляются подключённые доверенные компьютеры, затем текущий. Каждый сам проверяет официальный выпуск, подпись Ed25519 и контрольную сумму; ход обновления и причины отказа видны отдельно.
- Компьютеры с 0.1.x нужно один раз самостоятельно обновить до 0.2.0: 0.1.6 предложит выпуск в настройках или уведомлении при запуске. После этого работает «Обновить все компьютеры».

**Исправления**

- Доверенные компьютеры снова находятся после смены IP-адреса; новый адрес принимается только после проверки подлинности.
- Значки обновлений и задержки больше не перекрывают подписи и соседние экраны на карте.
- Кнопки обновления остаются доступны в небольшом окне настроек; обновление безопасно дожидается завершения работающих компонентов.

## 0.1.6 — 2026-10-24

### English

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

### Русский

- Исправлено игнорирование `--root=/path` (и других флагов путей) окном и ядром, хотя трей их учитывал. Теперь каждая роль принимает и `--root /path`, и `--root=/path`, поэтому окно, трей и запущенное им ядро всегда используют одни и те же каталоги.
- VPN-интерфейсы (Tailscale, WireGuard, ZeroTier, OpenVPN) показываются в выборе адреса в настройках, а не скрываются, и адаптер, в имени которого просто есть «tun» или «wg», больше не отбрасывается. Адрес физической локальной сети по-прежнему идёт первым, поэтому адрес по умолчанию не уходит в туннель.
- Запись автозапуска при входе теперь хранит класс окна и полный путь к исполняемому файлу, как в desktop-файле, который пишет установщик, поэтому окружения рабочего стола объединяют окно под значком Bifrost.
- На экране статуса ведомый компьютер в рабочем пространстве из трёх и более компьютеров теперь назван по имени, а не показывается только его идентификатор, пока он владел вводом.
- Исправлено многократное возвращение буфера обмена на другой компьютер, когда менеджер буфера обмена (Klipper, CopyQ) заново забирает выделение после удалённого копирования.
- Выпуски теперь включают контрольные суммы с подписью Ed25519; Bifrost 0.1.6 и новее отказываются от неподписанных или изменённых обновлений.
- Исправлен случайный сбой перезапуска при обновлении в Windows, когда предыдущая служба ещё завершалась.
- Применение адреса локальной сети теперь перезапускает службу, запущенную вне окна, и сохраняет надзор systemd в Linux.
- Ошибки остановки службы теперь показывают настоящую причину, программу, идентификатор процесса и код завершения.
- Службы, запущенные из окна, больше не оставляют зомби-процессы после выхода в Linux.

## 0.1.5 — 2026-10-24

### English

- Extended physical monitor-map zoom to 3×, shown on a remapped 50–200% control with the default view at 100%; existing saved views keep their scale.
- Map labels fade out secondary details and then names as zoom decreases, keeping small layouts uncluttered.
- Appearance now uses the shared settings modal, with consistent rows and controls in both languages and themes.
- Theme changes sweep across the window in a short, reversible ASCII-cell wave from the top-right theme button.

### Русский

- Масштаб физической карты мониторов увеличен до 3× и показан на переразмеченном регуляторе 50–200%, вид по умолчанию — 100%; сохранённые виды сохраняют свой масштаб.
- При уменьшении масштаба подписи на карте сначала убирают второстепенные детали, а затем названия, поэтому небольшие схемы остаются читаемыми.
- Раздел «Внешний вид» теперь использует общее модальное окно настроек с единообразными строками и элементами управления в обоих языках и темах.
- Смена темы проходит по окну короткой обратимой волной ASCII-ячеек от кнопки темы в правом верхнем углу.

## 0.1.4 — 2026-10-21

### English

- Added verified self-updates. Install this version manually once: 0.1.3 has no updater. Later versions can be installed from Settings or the startup update notice.
- Update downloads are checked against SHA256SUMS before installation; Bifrost keeps a backup and restarts its running roles safely.
- Updated TLS security dependencies rustls and rustls-webpki.

### Русский

- Добавлены проверяемые самообновления. Эту версию нужно один раз установить вручную: в 0.1.3 нет средства обновления. Последующие версии можно установить из настроек или из уведомления об обновлении при запуске.
- Загруженные обновления проверяются по SHA256SUMS перед установкой; Bifrost хранит резервную копию и безопасно перезапускает работающие роли.
- Обновлены зависимости TLS rustls и rustls-webpki.

## 0.1.3 — 2026-10-19

### English

- Fixed emergency-hotkey registration on KDE Wayland when Bifrost is launched from its desktop entry or an `app-bifrost@….service` unit. Shortcut refusals now show the portal's actual reason; GNOME 46 still has no GlobalShortcuts portal.
- Added `switch --to local|<peer>` for choosing an input destination explicitly; the tray's return action always selects local input.
- Unpairing removes the computer and its links from the shared monitor map, including stale entries, without revoking other computers' direct trust. Pair again to restore placement. `peers --revoke` now requires a running core.
- Fixed pairing and monitor-layout edits getting stuck after their window or caller closes.
- Successful portal grants and ready input agents clear stale refusal messages.
- Clipboard settings follow live status updates, and unresolved automatic links show why they are inactive.
- Small accent-coloured text is easier to read in the light theme, and zoomed or panned monitors only accept clicks inside the visible map.
- Added rotating window logs at `<state>/logs/lws-gui.log` and better diagnostics for connection and clipboard failures.

### Русский

- Исправлена регистрация аварийных горячих клавиш в KDE Wayland, когда Bifrost запущен из своего desktop-файла или юнита `app-bifrost@….service`. Отказ в сочетании клавиш теперь показывает настоящую причину от портала; в GNOME 46 портала GlobalShortcuts по-прежнему нет.
- Добавлена команда `switch --to local|<peer>` для явного выбора получателя ввода; действие возврата в трее всегда выбирает локальный ввод.
- Отмена сопряжения удаляет компьютер и его связи с общей карты мониторов, включая устаревшие записи, не отзывая прямое доверие других компьютеров. Чтобы восстановить расположение, сопрягите компьютеры заново. `peers --revoke` теперь требует запущенного ядра.
- Исправлено зависание сопряжения и правки раскладки мониторов после закрытия их окна или вызывающей программы.
- Успешные разрешения портала и готовые агенты ввода убирают устаревшие сообщения об отказе.
- Настройки буфера обмена следуют живому обновлению статуса, а неразрешённые автоматические связи показывают, почему они неактивны.
- Мелкий текст акцентного цвета лучше читается в светлой теме, а мониторы при масштабировании и сдвиге принимают клики только внутри видимой карты.
- Добавлены ротируемые журналы окна в `<state>/logs/lws-gui.log` и улучшена диагностика сбоев соединения и буфера обмена.

## 0.1.2 — 2026-10-18

### English

- Wayland displays are now detected on GNOME even without screen-capture permission, using the compositor's logical output geometry; failures are reported instead of leaving the map empty.
- A computer's online status is now the same on both ends of a link, instead of one side showing a paired computer as always offline.
- Computers that can only receive input (for example GNOME 46, which cannot capture the screen) no longer drop the connection; eligibility is based on the capabilities a computer actually reports.
- The map labels each computer with its operating system and each monitor with the real resolution and scaling (for example 2560x1600 at 150%), while layout and input stay in logical coordinates.
- A peer's displays are refreshed after it restarts instead of being treated as stale.
- The input lease default is raised to 1000 ms (still configurable from 300–1000 ms) and the challenge window is fixed, so brief Wi-Fi stalls no longer end remote control prematurely. This tolerates a short stall, not unlimited connection loss.
- Returning the pointer across an edge no longer locks that edge out: it re-arms after a short retreat or a brief pause, and a carried-over push is briefly refused.
- Added diagnostics that record edge returns and lease expiries to help investigate future drops.

### Русский

- Дисплеи Wayland теперь определяются в GNOME даже без разрешения на захват экрана, по логической геометрии выходов композитора; сбои сообщаются, а не оставляют карту пустой.
- Статус «в сети» теперь одинаков на обоих концах связи, а не показывает сопряжённый компьютер постоянно отключённым с одной стороны.
- Компьютеры, которые умеют только принимать ввод (например, GNOME 46, который не может захватывать экран), больше не разрывают соединение; пригодность определяется по возможностям, которые компьютер действительно сообщает.
- Карта подписывает каждый компьютер операционной системой, а каждый монитор — реальным разрешением и масштабом (например, 2560x1600 при 150%), при этом раскладка и ввод остаются в логических координатах.
- Дисплеи соседнего компьютера обновляются после его перезапуска, а не считаются устаревшими.
- Значение простоя ввода по умолчанию увеличено до 1000 мс (по-прежнему настраивается в диапазоне 300–1000 мс), а окно проверки исправлено, поэтому короткие сбои Wi-Fi больше не завершают удалённое управление преждевременно. Это допускает короткий сбой, но не бесконечную потерю связи.
- Возврат указателя через край больше не блокирует этот край: он снова готов после короткого отхода или небольшой паузы, а перенесённый упор ненадолго отклоняется.
- Добавлена диагностика, которая записывает возвраты через край и истечения аренды, помогая разбираться с будущими обрывами.

## 0.1.1 — 2026-10-16

### English

- Fixed the settings version display: it now sits in a padded, localized bottom row instead of overlapping the modal frame.
- Portable Windows executable available directly, alongside the archive with notices and example settings.
- Per-user Linux installation and removal with `install.sh`; existing settings and pairing are preserved.
- English project introduction, screenshots and clearer platform and installation requirements.
- A fresh GUI starts its core automatically and creates settings without a manual CLI step.
- Settings now offers a per-user Start at login toggle, pointing at the running executable.
- Licensed under MIT.

### Русский

- Исправлено отображение версии в настройках: теперь она находится в нижней строке с отступами и локализована, а не перекрывает рамку окна.
- Портативный исполняемый файл для Windows доступен напрямую, вместе с архивом, содержащим уведомления и пример настроек.
- Установка и удаление в Linux для отдельного пользователя с помощью `install.sh`; существующие настройки и сопряжение сохраняются.
- Введение в проект на английском языке, скриншоты и более ясные требования к платформам и установке.
- Новое окно само запускает ядро и создаёт настройки без ручного шага в командной строке.
- В настройках появился переключатель «Запускать при входе» для отдельного пользователя, указывающий на работающий исполняемый файл.
- Проект распространяется под лицензией MIT.

## 0.1.0 — 2026-10-16

### English

First release of **Bifrost**, a software KVM for sharing a keyboard, mouse and clipboard between trusted computers on a local network.

- Windows 11 and Linux: X11, KDE Plasma Wayland and GNOME Wayland. Portal support varies: GNOME 46 can receive input but cannot capture it for another computer.
- Secure mTLS connections with persistent peer trust and pairing by a 6-digit comparison code, confirmed on both computers.
- A draggable monitor map with links across gaps, adjustable edge segments and an ASCII bridge between computers.
- Shared clipboard with history, configurable hotkeys and an edge-glow indicator.
- Russian and English interfaces, light and dark themes, and a system tray. System autostart uses Windows Run/scheduled tasks or a Linux desktop entry; the deployment helper migrates existing entries.
- One executable, `bifrost` / `bifrost.exe`, for the window, core, agent and tray. Existing settings and pairing remain in the legacy `lan-workspace` directories. Wayland asks for portal permissions once after this app-id change.

This is an early release. Check emergency-release hotkeys on your desktop before relying on remote input; portal availability and permissions determine Wayland capabilities.

### Русский

Первый выпуск **Bifrost** — программного KVM для совместного использования клавиатуры, мыши и буфера обмена между доверенными компьютерами в локальной сети.

- Windows 11 и Linux: X11, KDE Plasma Wayland и GNOME Wayland. Поддержка порталов различается: GNOME 46 может принимать ввод, но не может захватывать экран для другого компьютера.
- Защищённые соединения mTLS с постоянным доверием между компьютерами и сопряжением по 6-значному коду сверки, подтверждаемому на обоих компьютерах.
- Перетаскиваемая карта мониторов со связями через промежутки, настраиваемыми участками краёв и ASCII-мостом между компьютерами.
- Общий буфер обмена с историей, настраиваемыми горячими клавишами и индикатором свечения у края.
- Русский и английский интерфейсы, светлая и тёмная темы и системный трей. Автозапуск использует раздел Run или планировщик Windows либо desktop-файл в Linux; помощник развёртывания переносит существующие записи.
- Один исполняемый файл `bifrost` / `bifrost.exe` для окна, ядра, агента и трея. Существующие настройки и сопряжение остаются в прежних каталогах `lan-workspace`. После смены app-id Wayland один раз запрашивает разрешения портала.

Это ранний выпуск. Перед тем как полагаться на удалённый ввод, проверьте аварийные горячие клавиши на своём рабочем столе; доступность портала и разрешения определяют возможности Wayland.
