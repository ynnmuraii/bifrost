# live-e2e — живые прогоны на двух машинах

Скрипты для выкладки и живых проверок на двух машинах владельца:

- **PC** — этот хост Windows, каталог установки `C:\Users\tihoo\Desktop\flow`.
- **ноутбук** — `ssh lws-laptop`, KDE Wayland, каталог `~/lan-workspace`.

С момента коммита b23335a продукт — **один** исполняемый файл `bifrost(.exe)`:
без аргументов открывается окно, `run` запускает службу, `agent …` внутренний,
остальные аргументы — CLI ядра (`status`, `switch`, `claim`, `peers`, `layout …`).

На Windows это GUI-приложение: прямой `& bifrost.exe status` в PowerShell не ждёт
и не заполняет `$LASTEXITCODE` (см. `packaging/README_RU.txt`, §12), поэтому все вызовы
идут через общую функцию `Invoke-Mdf` (Start-Process -Wait с перенаправленным выводом).

**Этот каталог не для git.** `docs/` в `.gitignore`; скрипты содержат машинные пути,
IP и ssh-имена и в репозиторий не попадают.

## Порядок прогона

```
package.ps1  →  deploy.ps1  →  start-cores.ps1 [-Dev]  →  smoke-edge.ps1 / smoke-switch.ps1
             →  finalize.ps1
```

`apply-layout.ps1`, `cross-live.ps1`, `probe-edge.ps1` и `verify-edge-disarm.ps1`
запускаются между `start-cores` и `finalize`, каждый — по своей задаче.
`gate-windows.ps1` и `gate-linux.sh` — это гейты сборки, они не выкладывают ничего.

Все скрипты должны запускаться из репозитория; они сами находят его корень от
`$PSScriptRoot`.

## Общий файл

`common.ps1` — общие функции (подключается через `. "$PSScriptRoot/common.ps1"`):
разрешение архивов `target/trial`, каталог доказательств, `Invoke-Mdf`, `Get-MdfStatus`,
`Wait-MdfFocus`, остановка процессов установки, эмуляция ввода, чтение дельт логов.

## Скрипты и параметры

### package.ps1
Собирает релизные архивы одного коммита: Windows — `cargo build --release --workspace`
(`target/release/bifrost.exe`), Linux — в одноразовом контейнере `rust:1.99`.
В каждом архиве `bifrost(.exe)`, `README_RU.txt`, `THIRD_PARTY_NOTICES.md`,
`config.example.toml`; как в `release.yml`, в Windows-zip дополнительно
`icon.ico`/`icon.svg`/`icon-256.png` из `apps/bifrost/assets/`, а в Linux-tar —
`bifrost.desktop` и дерево `share/applications` + `share/icons/hicolor`
(scalable SVG и PNG 16…256). Имена — `bifrost-<shortsha>-windows-x86_64.zip` /
`-linux-x86_64.tar.gz`. `target/trial` хранит одно поколение: старые архивы уезжают в
`target/trial-backup-<stamp>`, более старые бэкапы складываются в него же.
- `-Commit <7hex>` — по умолчанию HEAD (и обязан совпадать с HEAD);
- `-EvidenceDir <dir>` — куда положить копию `SHA256SUMS.txt`;
- `-PlanOnly`.

### deploy.ps1
Ставит архивы на PC и ноутбук, ничего не запускает. SHA256 проверяется до касания машин,
и ещё раз на ноутбуке после копирования. Текущий каталог установки уезжает в **один**
`*.old-<stamp>` (из него переносятся `config.toml`, `trust.toml`, `portal-restore.toml`
без изменений), старые бэкапы удаляются. Лишние `lws-core/lws-agent/lws-gui` удаляются.
Если в архиве нет `bifrost.exe` (архив до b23335a) — скрипт падает до подмены.
- `-Commit <7hex>` — по умолчанию самый свежий архив по `target/trial/SHA256SUMS.txt`;
- `-Archive <zip>`, `-LinuxArchive <tar.gz>` — явные архивы вместо `-Commit`;
- `-Laptop`, `-Install`, `-RemoteInstall` — по умолчанию `lws-laptop`,
  `C:/Users/tihoo/Desktop/flow`, `$HOME/lan-workspace`;
- `-EvidenceDir`; `-PlanOnly`.

### start-cores.ps1
Запускает службу на обеих машинах и проверяет, что обе отвечают `status`.
PC — `bifrost run`; с `-Dev` — `bifrost run --dev-capture-injected` и
`LWS_LOG=debug` (нужно для смоуков, конечное состояние возвращает `finalize.ps1`).
Ноутбук — под временным user-юнитом `lws-core-live-test` в настоящей KDE-сессии Wayland.
Скрипт отказывается работать, если установленный `bifrost` уже запущен.
- `-Dev`; `-Laptop`, `-Install`, `-RemoteInstall`; `-EvidenceDir`; `-PlanOnly`.

### smoke-edge.ps1
Смоук переходов и счётчиков краёв: вход в Remote через вертикальный край, 5 левых кликов
(нажатие/отпускание через 50 мс, 300 мс между кликами) сразу после входа и ещё 5 через
3 с, затем 3 пары клавиш, возврат через тот же край и аварийная комбинация. Доказательство
— постадийные `EdgeCounters` строки `lease liveness diagnostics` (хук PC, `CapturedBatch`,
`InputBatch` tx/rx, `Inject` и применённые EIS-края). Нужен PC-ядро с `-Dev`.
- `-Laptop`, `-Install`, `-LaptopLogDir`; `-EvidenceDir`.

### smoke-switch.ps1
Раунды счётчиков аренды без перехода через край: `bifrost switch <peer>` поднимает ту
же аренду Remote. Раунд: Remote → удержание `holdSeconds` мелкими движениями каждые
`holdStepMs` → аварийная комбинация → Local. Доказательство — дельты диагностики ядер и
пары счётчиков отправитель/получатель. Нужен PC-ядро с `-Dev`; id пира берётся из
`bifrost peers`.
- `-Peer <id>` (обязателен);
- `-Rounds` (3), `-HoldSeconds` (60), `-HoldStepMs` (2000);
- `-Laptop`, `-Install`, `-LaptopLogDir`; `-EvidenceDir`.

### probe-edge.ps1
Диагностика: один толчок к правому краю, выборка позиции курсора (отличить собственный
инжект от руки), опрос `status` и строки лога ядра про край/фокус/InputBatch. Только
чтение; нужен запущенный сервис.
- `-Install`; `-EvidenceDir`.

### verify-edge-disarm.ps1
Живая проверка «возврат разоружает край, через который вернулись»: один возврат, затем
секунда продолжения толчка в тот же край не должна снова войти, а уход из полосы — снова
войти. Читает только лог PC. Нужен PC-ядро с `-Dev`.
- `-Install`; `-EvidenceDir`.

### apply-layout.ps1
Применяет TOML раскладки к работающему ядру (`bifrost layout edit <файл>`), только
при связанном локальном владельце. TOML готовит IPC-клиент окна (`layout show`). Ядро
должно быть запущено.
- `-LayoutToml <файл>` (обязателен);
- `-Install`; `-PlanOnly`.

### cross-live.ps1
Сквозной живой переход: `start-cores.ps1 -Dev`, применение раскладки, редактор `cat` на
ноутбуке в KDE-сессии, вход через правый край, ввод метки, проверка, что ноутбук её
получил, скриншот ноутбука, удержание Remote 5 секунд без движения, аварийная комбинация и
проверка логов обоих ядер по всей цепочке capture → transport → inject.
- `-LayoutToml <файл>` (обязателен);
- `-Laptop`, `-Install`, `-LaptopLogDir`, `-WindowsLogDir`, `-Screenshot`;
- `-EvidenceDir`; `-PlanOnly`.

### finalize.ps1
Конечное состояние после прогона: копирует логи обеих машин в доказательства, гасит
временный юнит ноутбука, оставляет PC-ядро без dev-флага + окно, а на ноутбуке —
отделённые ядро и окно в KDE-сессии. `config/trust/portal-restore` не трогает.
- `-Laptop`, `-Install`, `-RemoteInstall`, `-LaptopLogDir`; `-EvidenceDir`.

### gate-windows.ps1
Гейт сборки на Windows, как в CI: `fmt`, `clippy -D warnings`, `cargo deny` (лицензии),
сборка и тесты workspace (в них входит тест агента «нет сети»).
- `-EvidenceDir`; `-PlanOnly`.

### gate-linux.sh
Гейт сборки на Linux (ноутбук или CI-раннер): установка системных пакетов, `fmt`,
`clippy -D warnings`, сборка и тесты workspace под `xvfb-run`. Запускать на Linux, не на PC.
- `[<evidence-dir>]` — необязательный первый аргумент.

## Правила безопасности

- Процессы гасятся **только** по пути внутри каталога установки; чужие одноимённые
  процессы (напр. fault-injection в `target/`) не трогаются.
- Каталог установки сохраняется ровно в **одной** копии `*.old-<stamp>`; старые бэкапы
  удаляются.
- `config.toml`, `trust.toml`, `portal-restore.toml` переносятся без изменений и никогда
  не перезаписываются содержимым архива.
- SHA архивов проверяется до подмены и после копирования; на Linux бинарник — `0755`.
- Устаревшие `lws-core` / `lws-agent` / `lws-gui` удаляются на каждом deploy.

## Где доказательства

Всё, что печатают скрипты и что они копируют, идёт в
`target/live-e2e/runs/<timestamp>/` (по умолчанию) или в каталог, переданный
`-EvidenceDir`. Доказательства остаются в `target/` и в git не попадают.
Передавайте один и тот же `-EvidenceDir` шагам одного прогона, чтобы они писали в общий
каталог; для `gate-linux.sh` это первый аргумент.
