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
flush-ится из TLS-буфера. Диагностика медленных очередей/IPC и lease-кадров — на уровне DEBUG,
не чаще одной записи в секунду для каждого этапа.

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
`xvfb-run` (см. `.github/workflows/ci.yml`).

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
