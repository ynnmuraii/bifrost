# LAN Workspace (MultideskFlow)

Программа для управления несколькими компьютерами в локальной сети одной клавиатурой и
мышью: `lws-core` держит сеть и состояние (TLS-линки, протокол, маршрутизацию ввода,
владение вводом, сопряжение узлов, раскладку экранов), а `lws-agent` на каждой машине
захватывает и подавляет локальный ввод, инжектирует чужой и следит за аварийным
освобождением. Целевые платформы — Windows 11 и Linux (X11 и Wayland через XDG-порталы);
задуманный сценарий — три доверенных узла в одной LAN.

**Статус: в разработке, не для использования.** Проект собирается и проходит автотесты,
но реальные устройства, задержки, восстановление после сбоев и аварийный выпуск ввода
ещё не проверены; поведение, интерфейсы и формат конфигурации меняются без предупреждения.
Не запускайте это на машине, которой пользуетесь.

После подтверждения кода на обоих компьютерах доверие сохраняется, связь устанавливается без
перезапуска службы, а её состояние появляется в GUI по мере подключения.

Для кадров ввода, lease и буфера обмена TCP_NODELAY включён на обоих концах до TLS-handshake:
мелкие кадры не должны ждать delayed ACK при простое ввода. Каждый кадр input-link явно
flush-ится из TLS-буфера. Диагностика медленных очередей/IPC — на уровне DEBUG,
не чаще одной записи в секунду для каждого этапа. Пока lease находится в Remote,
INFO `lease liveness diagnostics` раз в секунду сообщает накопленные счётчики
challenge/renew/status на границе FSM, принятые/отклонённые renew/status и возраст
последних входящих lease-сообщений по монотонным часам. При завершении lease выводится
финальный снимок. `fsm` считается для одного lease, `transport_connection` — для
соединения: успешные записи в сокет и декодированные lease-кадры. Эти записи не
продлевают lease и не меняют TTL или правила watchdog.
Тот же снимок содержит накопленные `keys_down/keys_up/buttons_down/buttons_up`:
`hook_edges` (Windows, mapped-события до enqueue, включая dropped и OS autorepeat),
`captured_edges` (полученные core CapturedBatch в Remote), `inject_edges` (команды
Inject в агент) и `eis_applied_edges` (события, закодированные в EIS frames; ошибка
flush по-прежнему отражается в InjectResult). Эти поля считаются для одного lease;
`None` означает, что этап не применим или ещё не прислал снимок. `transport_connection`
дополнительно содержит `input_tx/input_rx` — discrete edges всех InputBatch за
соединение, после успешной записи / проверки входящего envelope соответственно.
Снимки диагностические, не атомарная транзакция между процессами. Старые односекундные
`agent CapturedBatch` / `transport InputBatch` / `EIS injection summary` показывают
только первый batch интервала: отсутствие отдельной строки up не доказывает потерю
release. Нормальный Observe в Local остаётся нужен для edge tracking; сообщение
`captured input outside a remote source lease; dropped` теперь DEBUG.

Heartbeat core↔agent учитывает задержку собственного цикла: если он проснулся позже
планового срока хотя бы на половину `core_silence_ms`, watchdog молчания переармируется,
а DEBUG сообщает `core loop stalled …; not blaming the agent` (у агента — обратное).
Это не продлевает lease/deadline: задержка самого процесса не доказывает отказ пира,
но освобождение ввода по сроку lease сохраняется.

Windows capture хранит callback-контекст на собственном потоке: после EndCapture →
BeginCapture события идут в новую очередь. В Preparing повторные edge/hotkey-триггеры
не меняют выбранный переход и не перезапускают capture; timeout, отказ и аварийный
возврат по-прежнему отменяют переход. Агент интегрирует относительные Raw Input-дельты
в absolute logical-позиции до CapturedBatch; абсолютные Raw Input-записи не трактуются как дельты.
Core ведёт оценку на дисплее receiver от entry point, проверяет return edge перед clamp
и отправляет только absolute InputBatch. Форма событий ввода в IPC и сети не меняется.

При старте owner заново заявляет `max_seen + 1`, даже если сохранённый владелец — он сам.
При соединении оба узла сообщают текущую пару `(serial, owner)`; более новая пара для
того же owner вызывает новую заявку. Отказ `StaleOwnerSerial` отменяет Preparing и
запускает переподтверждение владения, после которого следующий переход возможен без
ручной заявки. Ownership и layout сохраняют только свои поля `config.toml` под общим
lock, не откатывая состояние друг друга.

На Wayland владение по-прежнему берётся явной кнопкой GUI «Взять управление»:
касание тачпада не переназначает владельца. `InputCapture.Activated` передаёт в core
позицию и сторону принятого барьера; только owner в Local разрешает переход по
настроенному link. Активация follower-а, неизвестный барьер или отсутствующая позиция
возвращают capture композитору без открытия lease. EIS-дельты источника интегрируются
в node-logical absolute-позицию до `CapturedBatch`, как Raw Input на Windows.
Изменение зон не пересоздаёт включённую InputCapture-сессию: прежние барьеры сохраняются,
а невозможность переустановки сообщается как ошибка. Реальный capture и безопасный
возврат требуют отдельной проверки на живом KDE; headless-тесты этого не доказывают.

## Сборка

Нужен Rust (stable) через [rustup](https://rustup.rs); версия канала и компоненты
зафиксированы в `rust-toolchain.toml` (`rustfmt`, `clippy`). Единственный нативный код в
зависимостях — `ring` (криптография для TLS), поэтому нужен компилятор C и архиватор;
всё остальное — чистый Rust.

### Windows

1. Установите **Build Tools for Visual Studio** и в нём workload «Desktop development
   with C++» (даёт MSVC, `cl.exe`, `link.exe` и ассемблер для `ring`; Visual Studio
   целиком не нужен).
2. Установите rustup и убедитесь, что хост-триплет — MSVC: `rustup default stable-msvc`
   (`rustup show` показывает `x86_64-pc-windows-msvc`).

### Ubuntu

```sh
sudo apt-get update
sudo apt-get install --yes --no-install-recommends build-essential binutils
```

### CachyOS / Arch

```sh
sudo pacman -S --needed base-devel
```

### Сборка и тесты

```sh
cargo build --workspace          # обе программы из apps/
cargo test  --workspace          # автотесты
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Собранные бинарники: `target/debug/lws-core` и `target/debug/lws-agent` (у агента нет
сетевых сокетов и вручную он не запускается — его поднимает и контролирует core).
Часть тестов Linux-бэкендов требует графической сессии; в CI для этого используется
`xvfb-run -a -s "-screen 0 1280x1024x24 -noreset"` (см. `.github/workflows/ci.yml`):
`-noreset` предотвращает сброс X-сервера между короткими probe-подключениями.
Fault harness ожидает `agent=ready` перед переключением и восстановление наблюдения
после каждого возврата в Local; завершение FSM не означает, что следующий tick уже прошёл.
Dry click regression проверяет ровно down→up на receiver сразу после входа в Remote
и спустя две секунды. Linux-тест с локальной Unix socket pair проверяет настоящие
EIS button frames и release_all для всех удерживаемых кнопок, без portal/desktop ввода.
Preparing-emergency harness стреляет аварийным сигналом сразу после Temporary-захвата,
а фазу подтверждает записанным focus — запуск CLI status не съедает окно prepare.
Graceful-shutdown transport test устанавливает один link
с одним инициатором и ждёт завершения formation events; dedup-races проверяются отдельно.

## Структура workspace

Cargo-воркспейс `apps/*` + `crates/*`; платформенный код вынесен за feature/cfg-границы,
так что ядро собирается на любой ОС.

Практически важное:

- `apps/lws-core` — сетевой процесс: TLS-линки, протокол, маршрутизация и FSM владения
  вводом, сопряжение и trust store, раскладка экранов, надзор за агентом.
- `apps/lws-agent` — платформенный ввод: захват с подавлением, инжекция, ledger нажатий,
  watchdog и аварийное освобождение.

Крейты:

- `crates/protocol` — форматирование и правила проводного протокола (Protobuf, лимиты,
  последовательности, Hello).
- `crates/transport` — защищённые линки (pinned TLS), дедупликация mesh, liveness.
- `crates/identity` — идентичность узла (`SHA-256(SPKI)`), закреплённый mTLS, сопряжение
  по 6-значному коду (SAS), trust store.
- `crates/config` — схема конфигурации (`config_version 2`), валидация, пути, атомарное
  сохранение.
- `crates/focus-router` — FSM владения и «фокуса» (owner/follower, claim, hand-off).
- `crates/ipc` — локальный канал core↔agent: фреймы, проверки пира, watchdog heartbeat.
- `crates/geometry` — дисплеи, рёбра, связи переходов и геометрия пересечения края.
- `crates/input-model` — нормализованные события ввода и таблицы HID↔evdev.
- `crates/clipboard` — текст clipboard: маркеры, лимиты, хэш.
- `crates/platform-probe` — определение сеанса/композитора/порталов и выбор бэкенда.
- `crates/platform-win` — Windows-бэкенд: hooks и Raw Input (захват), SendInput (инжекция).
- `crates/platform-x11` — X11-бэкенд: XTest (инжекция), XI2 (захват), clipboard.
- `crates/platform-wayland` — Wayland-бэкенд: XDG-порталы (InputCapture/RemoteDesktop) и
  EIS через `reis`.

## Лицензия

MIT — см. [LICENSE](LICENSE).

Сторонние компоненты: [licenses/THIRD_PARTY_NOTICES.md](licenses/THIRD_PARTY_NOTICES.md) — список крейтов
и их условий (в том числе шрифты Ubuntu Font Licence / OFL-1.1 в egui), генерируется
`cargo about generate --workspace --fail -o licenses/THIRD_PARTY_NOTICES.md licenses/notices.hbs` из
[about.toml](about.toml). Политику допустимых лицензий задаёт [deny.toml](deny.toml) и проверяет CI
командой `cargo deny check licenses`: GPL/LGPL/AGPL запрещены, MPL возможен только в dev-зависимостях.
