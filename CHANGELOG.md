# Changelog

Each release is described in English and Russian. / Каждый выпуск описан на английском и русском.

## Unreleased

### English

**Fixes**

- Compile the audio test RMS accessor only for its Unix-only consumers, keeping Windows all-target Clippy checks warning-free.
- Keep the input-loop future heap-owned across audio shutdown supervision, preventing Windows debug cores from overflowing their main-thread stack when clipboard sharing is toggled.
- Windows process-loopback capture preserves QPC host timestamps without advertising packet-local positions as a monotonic device clock; render clock positions remain supported.
- Audio settings quietly identify the current Listener and hide disabled microphone controls; system-audio popups use trusted computer names and a distinct app glyph.
- Enforce both peers' Hello requirements against stable implementation support and advertised offers, independent of transient agent readiness. Deliver compatibility refusals before graceful closure, select slow reconnects, and forbid bounded capability changes from withdrawing handshake requirements.
- Bound Hello advertised capability lists to 64 entries and required capability lists to 32 before membership or duplicate scans.
- Clipboard channels inherit their parent input link's negotiated protocol version for both outgoing frames and incoming validation, preserving clipboard compatibility across future mixed-minor peers.
- Keep input links usable when optional auxiliary export is unavailable; bind revocable key access to the current manager generation, erase roots on membership refusal, and return zeroizing keys through a closed purpose registry with bounded contexts.
- Disable TLS session resumption so every runtime and pairing connection re-runs certificate pin verification.
- Bind clipboard grants to the authenticated peer as well as the parent link, and fail closed when a live parent's negotiated version is missing.
- Fix boxed auxiliary exporter storage with the locked zeroize version while retaining a stable heap allocation and erasure before deallocation.
- Keep Unix-only audio-child test helpers out of Windows builds so all-target clippy remains warning-free.
- Keep Source pacing independent of Winsock's coarse receive timeout: shared UDP reception and capture-ring notifications wake media workers without a millisecond polling loop.

**New**

- Optional features can derive per-input-link keys without a new wire capability. The TLS exporter root stays inside transport, is erased on parent closure/replacement, and is accessible only through a redacted, revocable derivation view after validated Hello; consumers gate on their own feature capabilities.
- Add internal audio backend/Dry and pure packet/replay/status contracts, with dependency-direction checks that keep native adapters outside core and the input stack.
- Extend the audio backend contract with native clock/status snapshots, valid capture timestamps, reserved-hotkey collision checks and expiring, renewable PTT press leases.
- Preserve unknown native playback states and expose the OS-selected current media-session token in the shared backend contract and Windows adapter.
- Add bounded revisioned local audio IPC, an authoritative remote-audio popup, bilingual audio settings/device selection and tray privacy/mute/stop controls. Media metadata is transient and redacted from debug output.
- Define bounded P1 audio control messages and capability-gated, parent-bound transport delivery with isolated audio refusal and reserved Stop capacity.
- Add opt-in typed `[audio]` preferences, explicit directional consent, stable device selection and locked updates. Config version stays 2, but 0.5.0 configs containing `[audio]` are not downgrade-safe: restore the one-time `.pre-audio` copy made before adding audio to an audio-free config, or remove the audio tables. Hand-edited audio configs need their own earlier backup. Audio is disabled by default.
- Add a pure consent/session control model with contributory nonce/context binding, per-parent replay reservations, worker/generation revocation, deadline-driven failure and automatic Listener-group discovery. Listener child startup must succeed before Commit can start the Source.
- Add bounded Source-to-Listener StartRequest/StartRefusal negotiation with explicit request correlation, per-parent deduplication/rate limits and typed consent/capacity refusals; Listener still contributes the first nonce and starts before Commit. Track four peer control links separately from the three-session audio limit.
- Preserve explicit enabled audio intent while a Listener group is being minted or a Source's selected trusted Listener is unresolved. Config and GUI validation accept these pending states without granting consent or claiming active media.
- Wire optional system-audio control into core and GUI/tray requests: explicit bilateral consent, authenticated Listener-group discovery, parent-bound key derivation, isolated audio-child supervision, device listing and Listener-local gain/mute. Native startup precedes Commit, Active requires authenticated path validation, and per-session one-second leases fail closed independently of IPC heartbeat. Explicit disable removes the child and media sockets; microphone/PTT/media-command operations remain explicitly unsupported.
- Exercise isolated multi-core audio with deterministic Dry children, real pairing/consent/group discovery, Source-side starts, shared Listener rendering, lifecycle failures and audio-enabled KVM suites. Unpaired fixtures own process and root cleanup, including pairing-request tests. The hidden Dry-audio switch requires a debug build, dry input and an explicit test root; production does not read the harness-only audio environment flag.
- Choose each multi-core fixture's input and clipboard endpoints as one simultaneously bound batch, preventing duplicate ephemeral-port numbers before services start.
- Follow Focus now ducks Listener-local system-audio streams using owner-authenticated committed input focus, including third-PC Listeners and reconnect snapshots. Default debounce is250ms and the gain envelope is150ms; Preparing/Releasing/Blocked and stale owners cannot select a new primary. Unknown focus holds the last primary for two seconds, then returns to equal focus multipliers while preserving user gain/mute. Microphone targeting and OS master volume are unchanged; focus-report capability alone never starts an audio child.
- Define independent microphone-control/media-control wire capabilities, strict bounded and redacted messages, urgent mute/release admission, and pure Listener-authority PTT and session-scoped media permission policies.
- Fence audio offers/answers with the latest peer consent revision. Track microphone intent provenance: withdrawn or disconnected remote unmute is re-muted and never becomes child-recovery intent; departed-parent history is pruned without losing live replay protection.

**Audio media internals**

- Add bounded timestamp-grid jitter scheduling, adaptive Ethernet/Wi-Fi targets and loss/concealment counters.
- Add a separate slow, slew-limited drift PI controller with anti-windup and bounded frame corrections.
- Add an internal media-worker crate with monotonic-counter ChaCha20-Poly1305, canonical key-context vectors, bundled Opus PLC/conditional FEC and preallocated near-unity drift resampling.
- Add explicitly started audio media sessions with bounded callback rings, UDP path authentication/reports, filtered native-rate conversion, local gain/mute ramps, a three-input soft-limited mix point and bounded worker shutdown. Dry/loopback tests cover loss/reorder/injection and device invalidation.
- Both audio endpoints immediately authenticate the UDP path; Listener startup attempts its first challenge before returning, enabling Listener-first commit ordering and stateful outbound firewall state. Media remains gated on validation, with the existing three-attempt/500 ms-window/two-second bounds; a two-sided stateful-firewall relay test verifies recovery and audio flow.

**Audio worker timing**

- Source and UDP receiver workers wait on lock-free capture/packet notifications and absolute media/control deadlines instead of idle polling. Capture clock hints prefer valid host/QPC timestamps and never use capture device positions, which may remain constant between process-loopback packets.
- Keep the Source host clock advancing when its capture ring overflows before path validation, preventing a permanently future capture epoch. On Windows, short media deadlines use high-resolution waits instead of coarse park timeouts while long idle waits remain event-driven.
- Expose bounded drift ratio ppm, filtered fill error, applied coarse-correction count and actual clock authority through private worker Health. Source reports no drift controller; Listener observations survive PI resets.
- Bound Source inbound work to 64 packets before revisiting shutdown and media/control deadlines, so pinned-peer invalid-tag floods cannot starve pacing or shutdown.
- Print receiver drift diagnostics and clock authority in the Windows audible E2E example alongside independent capture-clock estimates.
- Drive Listener drift timing from a valid native playback position/frequency/host snapshot, not callback submission. Preserve a monotonic timeline across fallback and report native queued depth without assuming unknown depth is zero.
- Discard preactivation capture and flush the live edge once when the UDP path validates, rather than transmitting old ring contents. Regression coverage keeps jitter anchoring on the first authenticated media frame, not control traffic or preactivation timestamps.
- Keep an uninterrupted transmitted sample grid through capture silence without DTX. Fill uncovered host-time gaps with bounded silence (at most 200 ms), reanchor larger gaps, and report native discontinuity flags separately from actual capture repairs.
- Expose bounded per-stage latency observations, use 5 ms aggregate mix blocks with a native-callback quantum floor, and bound decoded lane queues. A deterministic wired chirp fixture with a modeled 20 ms native FIFO improves from 86.688 to 81.688 ms without changing jitter profile targets; native hardware timing requires platform verification.
- Wake the mixer from render consumption and refill a complete native quantum before parking. When safe before the next callback, honor an imminent jitter deadline rather than queueing an extra silent output period.

**Shared Listener media**

- Mix up to three independently authenticated sessions into one native Listener render stream with stable lanes, per-session gain/mute ramps and aggregate limiting. Queued authorization-subset frames let the realtime callback revoke one input without muting the others, taking locks or allocating.
- Use one configured UDP port per audio child for all sessions, with one event-driven reader and bounded session-ID/pinned-peer demultiplexing. Unknown and cross-session tuple injections are silent drops; cryptographic counters and replay windows remain session-local.

**Linux audio backend internals**

- Add an explicitly connected local Unix Pulse protocol backend, without libpulse/PipeWire/ALSA linkage: stable device names, pinned monitor/microphone capture, playback, cached native timing and bounded device-change events. Process exclusion, ownership-safe endpoint mute and global PTT are explicitly unavailable. Live tests are opt-in and use only uniquely owned null sinks, including encrypted Linux-to-Linux UDP audio; no default routing is changed.

**Linux MPRIS internals**

- Discover session-bus media players with incarnation-scoped tokens, bounded private metadata, native capability checks, extrapolated timelines and bounded revision-checked command completions. Linux's current-player heuristic selects only the most recently observed Playing player. Opt-in live tests register/control only their own fake player and verify name cleanup.

**Audio child IPC**

- Add a separate private audio-worker protobuf/AudioLink contract with bounded commands, version/generation/sequence checks, explicit backend probing, one-use secret extraction and recursive Debug redaction. Reuse existing same-user IPC endpoints; no samples or secrets enter GUI messages.
- Add the internal audio child role behind the default-enabled binary `audio` feature. It lazily owns native/Dry backends and at most three authorized UDP sessions, revokes media on core silence/disconnect, rejects reused session/key material, and pushes bounded actual capture/send observations. No native device is opened before an explicit command; core supervision remains a separate integration step.
- Add an explicitly audible Windows audio-child E2E example with same-user named-pipe authorization, encrypted UDP, process-tree exclusion and endpoint-loopback verification of a listener-only tone tail. Diagnostic capture requests 48 kHz stereo conversion without changing endpoint settings.

**Audio process supervision**

- Add an independent core-side audio supervisor with private one-use attach tokens, same-user IPC, bounded correlated commands, heartbeat/watchdog and isolated child containment. Restarted children receive no previous session keys; prior sessions fail and require renegotiation. The coordinator is integrated separately.

**Audio authorization leases**

- Require explicit per-session authorization grants of at most one second, renewed independently of IPC Ping. Expiration gates queued render PCM and outgoing media, stops only the expired session, and cannot be resurrected by a stale renewal.
- Distinguish native `Started` acknowledgment from `PathValidated`, so the Listener can acknowledge initialization before the Source exists without deadlocking the commit handshake. Supervisor shutdown also has reserved admission during restart/backoff.

**Audio M5/M6 worker contract**

- Add bounded private microphone-mute/PTT and opt-in media-watch/command messages. Metadata is Debug-redacted, command completions are correlated, and lease/timestamp/hotkey validation remains fail-closed; core integration is separate.
- Extend the supervisor with generation-fenced microphone, shortcut and consent-gated media controls and correlated completion events. Separate persistent mute toggles from nonzero-ID remote talk grants, bounded to 250 ms and explicitly revocable; Ping never extends a talk grant.

**Audio M5/M6 child execution**

- Execute owned microphone mute and shortcut observation, and keep persistent toggles separate from independently timed remote talk grants. Grant expiry remutes on a guard-owning thread even while other backend work stalls; native snapshots remain authoritative, pre-muted endpoints refuse grants, and Ping does not renew them.
- Forward bounded media metadata only during an explicitly enabled authorized Source watch, recheck the current player before commands, and correlate completion/cancellation. The explicit Dry harness has documented deterministic revisioned media; native backends never synthesize it.


### Русский

**Исправления**

- Вспомогательное чтение RMS в аудиотестах компилируется только для использующих его Unix-тестов, чтобы проверка Clippy всех целей Windows проходила без предупреждений.
- Цикл ввода остаётся в отдельном future в куче при завершении аудиосупервизора: переключение обмена буфером больше не переполняет стек главного потока отладочного ядра Windows.
- Захват звука процессов Windows сохраняет временные метки QPC, но больше не выдаёт позиции внутри пакета за монотонные часы устройства; позиции часов воспроизведения сохранены.
- Настройки аудио ненавязчиво отмечают текущий приёмник и скрывают отключённое управление микрофоном; окна системного звука показывают доверенное имя компьютера и отдельный значок приложения.
- Требования Hello обеих сторон проверяются по реализованным возможностям и объявленным предложениям независимо от готовности агента. Отказ совместимости доставляется перед плавным закрытием, повторное подключение откладывается, а ограниченные обновления возможностей не могут отзывать обязательные возможности Hello.
- Списки возможностей Hello ограничены 64 объявленными и 32 обязательными элементами до проверки включения и повторов.
- Каналы буфера обмена используют согласованную версию родительского входного соединения при отправке и проверке кадров, сохраняя совместимость при будущих различиях младшей версии протокола.
- Недоступность необязательного экспорта ключей не прерывает входное соединение; доступ к ключам привязан к текущему поколению менеджера, корни стираются при отказе членства, а вывод возвращает стираемые ключи через закрытый реестр назначений с ограниченным контекстом.
- Возобновление TLS отключено: каждое рабочее и сопрягающее соединение заново проверяет закреплённый сертификат.
- Разрешения буфера обмена привязаны к удостоверенному узлу и родительскому соединению; отсутствие согласованной версии живого родителя обрабатывается безопасным отказом.
- Исправлено хранение корневого секрета экспорта с закреплённой версией zeroize: сохраняются стабильное размещение в куче и стирание до освобождения памяти.
- Unix-only вспомогательный метод тестов аудиопроцесса исключён из сборки Windows, чтобы clippy всех целей не выдавал предупреждение.
- Темп Source больше не зависит от грубого таймера приёма Winsock: общий UDP-приём и уведомления кольца захвата пробуждают обработчики без миллисекундного опроса.

**Новое**

- Необязательные функции могут выводить ключи входного соединения без новой сетевой возможности. Корневой секрет TLS остаётся внутри транспорта, стирается при закрытии или замене родителя и после проверки Hello доступен только через скрывающий секрет, отзываемый интерфейс вывода ключей; потребители проверяют возможности своих функций.
- Добавлены внутренние контракты аудиобэкенда/Dry, чистых пакетов, защиты от повторов и статуса с проверкой зависимостей: нативные адаптеры не входят в core и стек ввода.
- Контракт аудиобэкенда дополнен снимками часов/состояния потока, достоверностью временных меток, проверкой конфликтов горячих клавиш и возобновляемыми ограниченными по времени нажатиями PTT.
- Общий контракт бэкенда и адаптер Windows сохраняют неизвестные состояния воспроизведения и предоставляют выбранный ОС текущий медиасеанс.
- Добавлены ограниченный локальный аудио-IPC с ревизиями, плеер удалённых потоков по подтверждённому состоянию, двуязычные аудионастройки/выбор устройств и индикаторы приватности/отключение звука/остановка в трее. Метаданные медиасеансов не сохраняются и скрыты в отладочном выводе.
- Определены ограниченные сообщения управления звуком P1 и доставка с проверкой возможностей и родительского соединения, изолированными отказами аудио и отдельным резервом для Stop.
- Добавлены отключённые по умолчанию типизированные настройки `[audio]`, явные разрешения по направлениям, стабильный выбор устройств и запись под блокировкой. Версия конфига остаётся 2, но конфиги 0.5.0 с `[audio]` несовместимы со старыми бинарниками: восстановите предыдущую `.pre-audio` либо удалите аудиотаблицы. Для вручную отредактированных аудиоконфигов нужна своя предыдущая копия.
- Добавлена чистая модель разрешений и аудиосеансов: вклады обеих сторон в nonce/контекст, защита от повтора в рамках родителя, отзыв по поколениям связи и воркера, тайм-ауты и поиск группы Listener. До Commit и запуска Source требуется подтверждение запуска дочернего сеанса Listener.
- Добавлены ограниченные StartRequest/StartRefusal от Source к Listener с явной корреляцией запросов, защитой от повторов/ограничением частоты на родительском соединении и типизированными отказами разрешений/ёмкости. Первый nonce по-прежнему создаёт Listener, который запускается до Commit; четыре управляющих соединения учитываются отдельно от трёх аудиосеансов.
- Сохраняется явное включение звука, пока создаётся группа Listener или разрешается выбранный доверенный Listener для Source. Валидаторы конфига и GUI принимают такие ожидающие состояния, не выдавая разрешений и не заявляя об активной передаче.
- Управление системным звуком подключено к core и запросам GUI/трея: явные двусторонние разрешения, проверенный поиск группы Listener, ключи родительского соединения, отдельный аудиопроцесс, список устройств и локальная громкость/заглушение Listener. Нативный запуск предшествует Commit, Active требует проверки UDP-пути, а отдельные секундные разрешения сеансов истекают независимо от IPC heartbeat. Явное выключение убирает дочерний процесс и медиасокеты; микрофон/PTT/медиакоманды честно отвечают «не поддерживается».
- Добавлены проверки звука между изолированными core с детерминированными Dry-процессами: настоящее сопряжение, разрешения и поиск группы, запуск со стороны Source, общий рендер Listener, отказы жизненного цикла и KVM со включённым звуком. Несопряжённый стенд сам завершает процессы и удаляет корни, в том числе в тестах запросов сопряжения. Скрытый Dry-переключатель требует отладочной сборки, dry-ввода и явно заданного тестового корня; рабочий бинарник не читает аудиофлаг окружения тестового стенда.
- Входные и буферные порты многопроцессного стенда выбираются одним одновременно связанным набором, чтобы ОС не выдала один временный порт двум службам до их запуска.
- Follow Focus приглушает системные потоки у Listener по подтверждённому фокусу ввода от текущего владельца, включая третий ПК и запрос снимка после переподключения. По умолчанию задержка250мс, огибающая громкости150мс; Preparing/Releasing/Blocked и старый владелец не выбирают новый основной поток. Неизвестный фокус сохраняет прежний основной поток две секунды, затем выравнивает множители с сохранением пользовательской громкости и заглушения. Цель микрофона и системная громкость не меняются; возможность отчёта о фокусе сама по себе не запускает аудиопроцесс.
- Определены отдельные сетевые возможности управления микрофоном и медиаплеером, строгие ограниченные сообщения с сокрытием метаданных, приоритет заглушения/отпускания и чистые политики PTT с владельцем Listener и разрешений медиасеанса.
- Предложения и ответы аудиосеансов сверяются с последней ревизией согласия пира. Сохраняется происхождение намерения микрофона: удалённое снятие заглушения отменяется при отзыве разрешения или разрыве и не становится состоянием восстановления; история ушедших родителей очищается с сохранением защиты живых от повтора.

**Внутренний аудиотракт**

- Добавлены ограниченный джиттер-буфер по временным меткам, адаптивная задержка Ethernet/Wi-Fi и счётчики потерь/маскирования.
- Добавлен отдельный медленный ПИ-регулятор дрейфа с ограничением скорости, защитой интегратора и ограниченной коррекцией кадров.
- Добавлен внутренний аудиотракт: ChaCha20-Poly1305 с монотонным счётчиком, канонический контекст ключей, встроенный Opus с PLC/условным FEC и ресэмплинг дрейфа без выделений в рабочем цикле.
- Добавлены явно запускаемые аудиосеансы с ограниченными кольцевыми буферами, проверкой UDP-пути и отчётами, преобразованием частоты, плавной локальной громкостью/заглушением, микшером трёх входов и ограниченным ожиданием остановки. Тесты Dry/loopback проверяют потери, перестановку, инъекции и потерю устройства.
- Оба конца аудиосеанса сразу проверяют UDP-путь; Listener пытается отправить первый аутентифицированный запрос до возврата из запуска, позволяя сначала открыть исходящее состояние межсетевого экрана. Медиа остаётся закрытым до проверки пути; сохранены три попытки, окна 500 мс и предел две секунды. Тест с двусторонним stateful-фильтром проверяет восстановление пути и доставку звука.

**Время аудиопроцесса**

- Обработчики Source и UDP-приёма ждут уведомлений захвата/пакетов без блокировок в callback и абсолютных сроков медиа/управления вместо опроса пустых очередей. Часы захвата используют достоверные временные метки host/QPC, а не позиции устройства, которые могут оставаться неизменными между пакетами process-loopback.
- Часы Source продолжают идти при переполнении кольца захвата до подтверждения пути, не позволяя эпохе захвата навсегда уйти в будущее. В Windows короткие медиадедлайны используют высокоточное ожидание вместо грубого park timeout; длинные ожидания сохраняют пробуждение по событиям.
- Закрытые сводки Health передают ограниченные ppm ресэмплинга, фильтрованную ошибку наполнения, число применённых грубых коррекций и фактический источник часов. У Source нет ПИ-регулятора дрейфа; счётчик Listener сохраняется при сбросах ПИ.
- Входящая обработка Source ограничена 64 пакетами перед повторной проверкой остановки и сроков медиа/управления: поток пакетов с неверным тегом от закреплённого адреса не блокирует темп передачи и завершение.
- Звуковой E2E-пример Windows выводит диагностику дрейфа приёмника и источник часов рядом с независимой оценкой часов захвата.
- Часы дрейфа Listener используют достоверные нативные позицию воспроизведения, частоту и время host, а не число переданных callback кадров. Переходы на резервные часы сохраняют монотонность; неизвестная глубина нативного буфера не подменяется нулём.
- Захват до подтверждения UDP-пути отбрасывается, а при активации один раз очищается старый хвост кольца. Регрессионные тесты закрепляют начало джиттера по первому проверенному медиакадру, а не управляющим пакетам и времени до активации.
- При тишине захвата сохраняется непрерывная временная сетка отправленных кадров без DTX. Непокрытые разрывы времени host заполняются тишиной до 200 мс; большие разрывы переустанавливают опорную точку. Нативные флаги discontinuity учитываются отдельно от реальных исправлений захвата.
- Добавлены ограниченные наблюдения задержки по стадиям, блоки общего микшера 5 мс с запасом не меньше нативного callback и ограниченные очереди декодированных входов. Детерминированный тест с моделью нативного FIFO 20 мс улучшился с 86,688 до 81,688 мс без изменения профилей джиттера; нативные задержки проверяются на соответствующей платформе.
- Микшер пробуждается при потреблении выхода и перед ожиданием заполняет весь нативный квант. Если запас до следующего callback позволяет, он ждёт близкий срок джиттера вместо постановки лишнего периода тишины в очередь.

**Общий аудиовыход Listener**

- До трёх независимо проверяемых сеансов микшируются в один нативный поток Listener со стабильными входами, плавной громкостью/заглушением и общим лимитером. Подготовленные варианты разрешённых входов позволяют callback немедленно отключить один источник, не заглушая остальные, без блокировок и выделений памяти.
- Все сеансы аудиопроцесса используют один настроенный UDP-порт, один событийный поток приёма и ограниченную маршрутизацию по ID сеанса и закреплённому адресу отправителя. Неизвестные и подменённые межсеансовые пакеты молча отбрасываются; криптосчётчики и защита от повторов остаются отдельными для каждого сеанса.

**Внутренний аудиобэкенд Linux**

- Добавлен явно подключаемый бэкенд локального Unix-протокола Pulse без зависимостей от libpulse/PipeWire/ALSA: стабильные имена устройств, закреплённый захват монитора/микрофона, воспроизведение, снимки времени и ограниченная очередь изменений устройств. Исключение своего процесса, безопасное владение заглушением и глобальный PTT явно недоступны. Натурные тесты включаются отдельно и используют только собственные null-sink, включая шифрованный UDP-тракт Linux→Linux; маршруты по умолчанию не меняются.

**Внутренний MPRIS Linux**

- Добавлены обнаружение медиаплееров на шине сеанса, токены отдельных экземпляров, ограниченные приватные метаданные, проверка возможностей, экстраполяция позиции и ограниченные по времени команды с проверкой ревизии. Текущим считается только последний замеченный играющий плеер. Натурный тест включается отдельно, управляет только собственным фиктивным плеером и проверяет освобождение его имени.

**IPC дочернего аудиопроцесса**

- Добавлен отдельный закрытый контракт protobuf/AudioLink аудиопроцесса с ограниченными командами, проверкой версии/поколения/последовательности, явным запросом проверки бэкенда, одноразовым извлечением секретов и скрытием их в Debug. Используются существующие локальные IPC-каналы того же пользователя; сэмплы и секреты не попадают в GUI.
- Добавлена внутренняя роль дочернего аудиопроцесса под включённой по умолчанию функцией сборки `audio`. Бэкенды native/Dry создаются только по запросу; не более трёх разрешённых UDP-сеансов останавливаются при потере core, повторное использование сеансов/ключей отклоняется, фактические захват/отправка передаются в ограниченных сводках. До явной команды устройства не открываются; надзор core интегрируется отдельно.
- Добавлен явно запускаемый звуковой E2E-пример аудиопроцесса Windows: авторизация через локальные named pipes, шифрованный UDP, исключение дерева процесса и проверка тона слушателя через endpoint loopback после остановки источника. Диагностический захват запрашивает преобразование в 48 кГц stereo без изменения настроек устройства.

**Надзор аудиопроцесса**

- Добавлен независимый надзор аудиопроцесса из core: приватные одноразовые токены, локальный IPC того же пользователя, ограниченные коррелированные команды, watchdog и изолированное завершение дочернего процесса. После перезапуска старые ключи не передаются, сеансы завершаются ошибкой и требуют нового согласования. Координатор интегрируется отдельно.

**Разрешения аудиосеансов**

- Каждый сеанс получает отдельное явное разрешение не более чем на секунду; продление не связано с IPC Ping. Истечение закрывает вывод подготовленного PCM и передачу медиа только этого сеанса; запоздалое продление не восстанавливает его.
- Нативное подтверждение `Started` отделено от `PathValidated`: Listener подтверждает инициализацию до запуска Source, не блокируя commit. Остановка надзора имеет отдельный канал, доступный и во время перезапуска/backoff.

**Контракт аудиопроцесса M5/M6**

- Добавлены ограниченные закрытые сообщения микрофонного mute/PTT и явно включаемых медиаданных/команд. Метаданные скрыты в Debug, результаты команд связаны с запросами, сроки PTT и сочетания клавиш проверяются с безопасным отказом; интеграция core выполняется отдельно.
- Супервизор дополнен управлением микрофоном, PTT и медиаданными с проверкой поколения и связанными результатами. Постоянное переключение mute отделено от удалённых разрешений говорить с ненулевым ID, сроком до 250 мс и явным отзывом; Ping не продлевает разрешение.

**Исполнение M5/M6 в аудиопроцессе**

- Реализованы владение заглушением микрофона и наблюдение PTT; постоянное переключение отделено от удалённых разрешений говорить. Независимый поток guard заглушает по истечении даже при задержке другой работы бэкенда; фактическое состояние подтверждают снимки, изначально заглушённый микрофон отклоняет разрешение, Ping его не продлевает.
- Ограниченные медиаданные передаются только при явно включённом наблюдении разрешённого Source; перед командой повторно проверяется текущий плеер, результаты и отмены связаны с запросами. Только явный Dry-режим предоставляет документированный детерминированный медиасеанс с ревизиями.


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
