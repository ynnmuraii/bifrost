# Changelog

Each release is described in English and Russian. / Каждый выпуск описан на английском и русском.

## 0.4.0 — 2026-10-30

### English

**New**

- Screen map: right-click a display-edge pair to allow or forbid crossing it. The rule covers every visible fragment of that pair, forbidden edges are drawn dashed and persist in the layout.
- Automatic short-edge contacts are disabled when the shared span is under the larger of 32 logical pixels and 2% of the full edge at either end after occlusion; such an edge can still be enabled manually.
- Physical display size is read from EDID on Windows (SetupAPI), X11 and Wayland and used to scale the map. A manual size can be entered as a diagonal of 3–120 inches; **Reset** returns to the automatic size when it is valid and to the legacy size otherwise. Shift+drag moves a single display, a normal drag moves the whole computer. Existing layouts keep the legacy map size until **Reset**.
- Input diagnostics use a source-host monotonic capture-receipt timestamp and bounded p50/p95/p99 histograms for queue age, TLS writes, inbound dispatch, local injection and send-to-InjectAck round trips. Logs contain durations/counters, not coordinates, keys or content; Ping RTT remains distinct from input latency.
- The newest absolute motion has a per-lease slot outside the bounded queue. The preceding position is frozen before button/key/wheel events, discrete ordering is retained and lease changes clear pending motion.
- A dedicated TLS writer keeps slow writes out of the reader/coordinator. Control/lease/Pong priority is bounded at whole-frame boundaries; pending writes close the link after 600 ms without progress or 1000 ms total, with one shared 100 ms graceful-close budget. Idle healthy links are not timed out by the writer.

**Fixes**

- Loss-recovery bursts no longer evict native-offer metadata after 64 outstanding batches and turn a successful completion into false Partial/InjectionFailed. Core→Agent admission is bounded to 64 offered plus 64 pending batches, ordered by exact local lease+sequence completions; control/release bypass pending input, not completion validation. Fresh input does not depend on network InjectAck credits.
- An admitted topology update cannot be overtaken by a later entry fence, and continuous motion cannot starve auxiliary frames. Metrics snapshots release their shared lock before synchronous logging, empty idle windows are skipped, and link shutdown preserves the first terminal reason.
- The file-only dry clipboard adapter keeps slow filesystem polling and selection writes off the agent session loop, without changing the 300 ms watchdog or native clipboard adapters.

**Breaking changes**

- 0.4.0 cannot connect, pair or share input/clipboard with any 0.3.x computer: the network protocol major version is 1 and the TLS channel names, the Protobuf packages and the local IPC endpoint names changed. Update every computer. Existing pairing, trusted computers and the private key are kept, so no new pairing is needed. Do not downgrade to 0.3.x.
- Close Bifrost 0.3.x completely before installing 0.4.0. A running 0.3.x window is not detected as an existing instance, and it can keep the old data directory in use.
- The application directory is renamed from `lan-workspace` to `bifrost` on Windows (`%APPDATA%`, `%LOCALAPPDATA%`) and Linux (`~/.config`, `~/.local/share`, `~/.local/state`). The first 0.4.0 start moves the existing directory automatically. The move is a rename, never a copy or a merge: if a `bifrost` directory already exists it wins and the old one is left untouched with a warning. If the old directory is still in use, that start keeps using it and the next start retries.
- The configuration/log environment variables are now `BIFROST_*` (`BIFROST_LOG`, `BIFROST_KEEP_ROOTS`); the old `LWS_*` names are no longer read.

**Notes**

- Log files are now `bifrost-core.log`, `bifrost-agent.log` and `bifrost-gui.log`. The old `lws-*.log` files move with the old directory and are no longer written to.
- “Update all computers” across this breaking version waits up to about five minutes for the whole batch, because the old version cannot verify the new protocol, and then reports the updated computers as unreachable before the requesting computer updates itself. This is expected; the computers reconnect after the requester restarts into 0.4.0.
- The input lease/control-return bounds, 150 ms repeat guard and 300 ms agent core-silence watchdog are unchanged. Controlled Docker loss tests show variable TCP retransmission tails, not a guaranteed reduction in Wi-Fi p99; aborted leases are marked separately rather than counted as successful latency samples.

### Русский

**Новое**

- Карта экранов: правый клик по паре краёв дисплеев разрешает или запрещает переход. Правило действует для всех видимых фрагментов этой пары, запрещённые края рисуются пунктиром и сохраняются в раскладке.
- Автоматические короткие контакты краёв отключаются, если общий участок после перекрытия меньше большего из 32 логических пикселей и 2% полной длины края хотя бы с одной стороны; такой край можно включить вручную.
- Физический размер дисплея читается из EDID на Windows (SetupAPI), X11 и Wayland и используется для масштаба карты. Размер можно задать вручную по диагонали 3–120 дюймов; **Сброс** возвращает автоматический размер, если он корректен, иначе прежний. Shift+перетаскивание двигает отдельный дисплей, обычное перетаскивание — весь компьютер. Существующие раскладки сохраняют прежний размер карты до **Сброса**.
- Диагностика ввода использует монотонную метку получения захвата на исходном компьютере и ограниченные гистограммы p50/p95/p99 для возраста очереди, TLS-записи, входящей диспетчеризации, локальной инъекции и времени send→InjectAck. В журналах — длительности/счётчики, не координаты, клавиши или содержимое; Ping RTT не выдаётся за задержку ввода.
- Новейшая абсолютная позиция хранится в отдельном слоте своей аренды вне ограниченной очереди. Перед кнопкой, клавишей или колесом фиксируется предшествующая позиция; порядок дискретных событий сохраняется, при смене аренды ожидающее движение очищается.
- Отдельная задача TLS-записи не задерживает чтение и координатор. Управление/аренда/Pong имеют ограниченный приоритет между целыми кадрами; ожидающая запись закрывает соединение после 600 мс без прогресса или 1000 мс суммарно, с общим бюджетом корректного закрытия 100 мс. Исправное соединение без ожидающей записи этот таймаут не закрывает.

**Исправления**

- Пачки после восстановления TCP больше не вытесняют metadata native-offers при 64 незавершённых операциях и не превращают успешное применение в ложный Partial/InjectionFailed. Core→Agent ограничивает число отправленных пачек до 64 и ожидающих до 64, продвигая FIFO по точным локальным завершениям аренда+номер; управление/освобождение обходят ожидающий ввод, но не проверку завершений. Свежий ввод не ждёт сетевых кредитов InjectAck.
- Принятое обновление топологии нельзя обогнать более поздним входом в аренду, а непрерывное движение не вытесняет вспомогательные кадры навсегда. Снимок метрик освобождает общий mutex до синхронной записи журнала; пустые окна бездействия пропускаются, а закрытие сохраняет первую причину отказа.
- Файловый dry-адаптер буфера выполняет медленный опрос файловой системы и запись выделения вне цикла агента; watchdog 300 мс и нативные адаптеры буфера не изменены.

**Ломающие изменения**

- 0.4.0 не соединяется, не сопрягается и не передаёт ввод/буфер с компьютерами 0.3.x: мажорная версия сетевого протокола теперь 1, изменились имена TLS-каналов, пакетов Protobuf и локальных IPC-эндпоинтов. Обновите все компьютеры. Сопряжения, доверенные компьютеры и приватный ключ сохраняются, повторное сопряжение не требуется. Не откатывайтесь на 0.3.x.
- Перед установкой 0.4.0 полностью закройте Bifrost 0.3.x. Запущенное окно 0.3.x не распознаётся как уже работающий экземпляр и может удерживать старый каталог данных.
- Каталог приложения переименован из `lan-workspace` в `bifrost` (Windows: `%APPDATA%`, `%LOCALAPPDATA%`; Linux: `~/.config`, `~/.local/share`, `~/.local/state`). Первый запуск 0.4.0 переносит существующий каталог автоматически. Это переименование, а не копирование и не объединение: если каталог `bifrost` уже существует, используется он, а старый остаётся нетронутым с предупреждением. Если старый каталог ещё занят, этот запуск продолжает работать с ним, а следующий попробует снова.
- Переменные окружения настроек и журналов теперь `BIFROST_*` (`BIFROST_LOG`, `BIFROST_KEEP_ROOTS`); старые имена `LWS_*` больше не читаются.

**Примечания**

- Файлы журналов теперь называются `bifrost-core.log`, `bifrost-agent.log` и `bifrost-gui.log`. Старые `lws-*.log` переезжают вместе с прежним каталогом, и запись в них больше не ведётся.
- «Обновить все компьютеры» при переходе через ломающую версию ждёт до примерно пяти минут на весь пакет, потому что старая версия не может проверить новый протокол, после чего помечает обновлённые компьютеры как недоступные и обновляется компьютер-инициатор. Это ожидаемо; компьютеры соединятся снова после перезапуска инициатора в 0.4.0.
- Границы аренды/возврата управления, защита повторов 150 мс и watchdog тишины core 300 мс не изменены. Контролируемый Docker-стенд показывает изменчивые хвосты повторных передач TCP, а не гарантированное снижение Wi-Fi p99; прерванные аренды помечаются отдельно, а не учитываются как успешные latency samples.

## 0.3.3 — 2026-10-30

### English

**New**

- PNG clipboard images have a separate opt-in on both computers, off by default, on Windows, X11 and KDE Wayland (data-control); GNOME Wayland remains unsupported. The default PNG limit is 16 MiB (configurable up to 32 MiB), with a separate default rate limit of 8 MiB/s. Linux passes PNG through unchanged, including large X11 INCR transfers; Windows converts DIB/DIBV5 and eagerly publishes PNG + alpha-preserving DIBV5. Text wins, secret markers are checked before image reads, and images never enter history. Windows decode dimensions and memory are bounded separately.

**Fixes**

- Clipboard assembly requires the exact offered byte count, and agent applies enforce the 1 MiB text cap independently of IPC chunk counts. Clipboard writes, preparation and native apply no longer hold up the input loop; stalled writes close their channel instead of reusing a partially written frame.

### Русский

**Новое**

- PNG-изображения буфера включаются отдельной настройкой на обоих компьютерах, по умолчанию выключенной, на Windows, X11 и KDE Wayland (data-control); GNOME Wayland пока не поддерживается. Лимит PNG по умолчанию — 16 МиБ, настраивается до 32 МиБ; отдельная скорость по умолчанию — 8 МиБ/с. Linux передаёт PNG без преобразований, включая большие передачи X11 INCR; Windows преобразует DIB/DIBV5 и сразу публикует PNG и DIBV5 с альфа-каналом. Текст имеет приоритет, секретные маркеры проверяются до чтения изображения, изображения никогда не попадают в историю. Декодирование на Windows дополнительно ограничено по размерам и памяти.

**Исправления**

- Сборка буфера требует точного заявленного размера, а применение агентом ограничено 1 МиБ независимо от числа IPC-фрагментов. Запись, подготовка и применение буфера больше не задерживают цикл ввода; зависшая запись закрывает свой канал, не продолжая частично записанный кадр.

## 0.3.2 — 2026-10-29

### English

**Security and compatibility**

- Pairing now uses commitment/reveal before comparing the six-digit SAS, preventing either side from choosing its contribution after seeing the other's. GUI and CLI visibly warn when an older peer uses the less-protected legacy ceremony; always compare and confirm the codes on both computers.
- Clipboard transfers and incoming connection admission now have bounded concurrency, lifetimes and cancellation. Secret-marked text remains refused, and key identities are no longer written to diagnostic logs.
- Failed trust-store saves are retried so an unpair decision survives a restart. Clipboard offers are bound to the authenticated sender, origin and session epoch, with replay protection retained across reconnects and listener changes.
- Clipboard process-restart detection requires capability 14 on both peers. Legacy peers still reject replay across reconnects within the same process, but cannot detect a peer process restart. There is no protocol-version bump.

**Reliability and usability**

- Recover valid configuration backups before creating defaults; atomic replacement keeps the previous configuration readable, and process locks cannot be stolen from a live owner.
- Pairing rejection, success and timeout are reported accurately without premature success or silent expiry. Failed connection attempts recover, CLI pairing has bounded waits, and malformed non-ASCII hexadecimal input is rejected safely.
- The GUI tracks remote targets by NodeId instead of display name, clears stale targets and improves accessible roles and labels, including SAS controls. Login entries explicitly start the core; opening the window manually does not restart a deliberately stopped core. Re-enable an older login entry once to update it.
- Input and clipboard listeners recover from DHCP address changes without discarding logical clipboard state. Windows capture overflow and emergency release bypass a full input queue; rejected synthetic motion no longer leaves suppression budget behind.
- Abandoning an active lease releases control promptly and stale renewals cannot revive it. X11 refreshes pointer bounds after display changes and preserves asynchronous injection errors. X11 and Wayland clipboard reads and stalled transfers can be cancelled without blocking input; Wayland relative cursor initialization no longer repeats or loses its estimate.
- On Linux, closing a window releases its instance lock immediately even when a concurrently launched child process has not yet executed, allowing a quick relaunch.

### Русский

**Безопасность и совместимость**

- Перед сравнением шестизначного SAS-кода сопряжение теперь использует commitment/reveal: ни одна сторона не может выбрать свой вклад, уже узнав вклад другой. Окно и CLI явно предупреждают о менее защищённой старой процедуре при работе со старым компьютером; всегда сравнивайте и подтверждайте код на обоих экранах.
- Для передачи буфера обмена и приёма подключений ограничены число одновременных операций, время жизни и ожидание отмены. Секретно помеченный текст по-прежнему отклоняется; идентификаторы клавиш больше не записываются в диагностические журналы.
- Неудачная запись хранилища доверия повторяется, чтобы удаление сопряжения сохранялось после перезапуска. Предложения буфера привязаны к аутентифицированному отправителю, источнику и эпохе сеанса; защита от повторов сохраняется при переподключении и смене слушателя.
- Для обнаружения перезапуска процесса при обмене буфером нужна возможность 14 на обоих компьютерах. Старые версии защищены от повторов при переподключении в пределах одного процесса, но не обнаруживают перезапуск процесса соседа. Версия протокола не повышалась.

**Надёжность и удобство**

- Перед созданием настроек по умолчанию восстанавливается корректная резервная копия. Атомарная замена сохраняет доступность предыдущего файла; блокировку нельзя отнять у работающего процесса.
- Отказ, успех и истечение времени сопряжения отображаются достоверно, без преждевременного успеха и молчаливого завершения. Неудачные подключения восстанавливаются, ожидание CLI-сопряжения ограничено, некорректный шестнадцатеричный ввод с не-ASCII символами отклоняется безопасно.
- Окно различает удалённые компьютеры по NodeId, а не имени, очищает устаревшие цели и улучшает доступные роли и подписи, включая элементы SAS. Автозапуск явно запускает службу при входе в систему; ручное открытие окна не запускает намеренно остановленную службу. Старую запись автозапуска один раз выключите и включите.
- Слушатели ввода и буфера восстанавливаются после смены адреса DHCP без потери логического состояния буфера. На Windows переполнение очереди захвата и аварийное освобождение обрабатываются независимо от заполненной очереди; отклонённое синтетическое движение не оставляет лишний бюджет подавления.
- Отказ от активной аренды быстро возвращает управление, а запоздалые продления не воскрешают её. X11 обновляет границы указателя при изменении экрана и сохраняет асинхронные ошибки инъекции. Чтение буфера X11/Wayland и зависшие передачи отменяются без блокировки ввода; относительный курсор Wayland больше не инициализируется повторно и не теряет оценку положения.
- На Linux закрытие окна сразу освобождает блокировку экземпляра, даже если параллельно запущенный дочерний процесс ещё не выполнил exec, позволяя быстро открыть окно заново.

## 0.3.1 — 2026-10-27

### English

**New**

- Discovered, unpaired LAN computers now appear on the map as dotted cards with a gentle ASCII animation. **Connect** starts pairing; **Hide** removes a card, and **Show again** restores it. Manual address entry remains available.
- **Connect** now sends a pairing request that the other computer must **Accept** before comparing the six-digit code on both screens. Incoming requests can be disabled with **Settings → Accept pairing requests**. Requests require Bifrost 0.3.1 on both computers; older versions keep the manual pairing-window flow.

**Fixes**

- Unpairing now removes the pair on both computers: immediately while online, or on the other computer's next connection within 30 days. Both computers need Bifrost 0.3.1 for synchronized removal.
- Older versions instead show a manual **Remove** suggestion after a prolonged offline period and a certificate rejection. Network failures alone never remove a pair automatically.

### Русский

**Новое**

- Найденные в локальной сети несопряжённые компьютеры теперь показаны на карте пунктирными карточками с мягкой ASCII-анимацией. **«Подключиться»** начинает сопряжение, **«Скрыть»** убирает карточку, а **«Показать снова»** возвращает её. Ручной ввод адреса остаётся доступен.
- **«Подключиться»** теперь отправляет запрос сопряжения: другой компьютер должен **«Принять»** его перед сравнением шестизначного кода на обоих экранах. Входящие запросы отключаются переключателем **Настройки → Принимать запросы на сопряжение**. Для запросов на обоих компьютерах нужна Bifrost 0.3.1; со старыми версиями сохраняется ручное открытие окна сопряжения.

**Исправления**

- Удаление сопряжения теперь удаляет пару на обоих компьютерах: сразу при наличии связи или при следующем подключении другого компьютера в течение 30 дней. Для синхронизации удаления на обоих компьютерах нужна Bifrost 0.3.1.
- Для старых версий вместо этого появляется ручная подсказка **«Удалить»** после длительного отсутствия связи и отказа сертификата. Одни лишь сетевые ошибки никогда не удаляют пару автоматически.

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
