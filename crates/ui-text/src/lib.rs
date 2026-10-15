//! The two languages of the window: one key per string the owner reads.
//!
//! The catalogue is one exhaustive match per language, so a key that is missing a
//! translation does not compile. Placeholders (`{}`, `{name}`) are filled in by the
//! callers, which are the small `format!` helpers next to the data they describe.

use serde::{Deserialize, Serialize};

/// The language of the window and of the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    /// Русский.
    #[default]
    Ru,
    /// English.
    En,
}

impl Lang {
    /// The stored name of the language (`ru` / `en`).
    pub const fn code(self) -> &'static str {
        match self {
            Lang::Ru => "ru",
            Lang::En => "en",
        }
    }

    /// The other one.
    pub const fn toggled(self) -> Lang {
        match self {
            Lang::Ru => Lang::En,
            Lang::En => Lang::Ru,
        }
    }

    /// The language this system is set to: Russian for a `ru*` system, English for
    /// anything else. It only decides the first start, because the choice is stored.
    pub fn from_system() -> Lang {
        match system_language() {
            Some(tag) if tag.to_ascii_lowercase().starts_with("ru") => Lang::Ru,
            _ => Lang::En,
        }
    }
}

/// The language tag of this system, when it names one.
#[cfg(windows)]
fn system_language() -> Option<String> {
    // `GetUserDefaultUILanguage` is the user's chosen UI language; its primary language
    // id is the low ten bits (ru is 0x19), which is all the window needs.
    let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    let primary = id & 0x03ff;
    match primary {
        0x19 => Some("ru".to_owned()),
        _ => None,
    }
}

/// The language tag of this system, when it names one.
#[cfg(not(windows))]
fn system_language() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
}

/// One string of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// «Пары»
    Pairs,
    /// «Клавиши»
    Keys,
    /// «Буфер обмена»
    Clipboard,
    /// «Диагностика»
    Diagnostics,
    /// «Настройки»
    Settings,
    /// «кому доверяет этот компьютер»
    WhoThisComputerTrusts,
    /// «горячие клавиши службы ввода»
    TheInputServiceSHotkeys,
    /// «скопировать здесь — вставить там»
    CopyHerePasteThere,
    /// «отчёт для разбора проблем»
    AReportForTroubleshooting,
    /// «адрес и служба»
    AddressAndService,
    /// «Слева»
    Left,
    /// «Справа»
    Right,
    /// «Сверху»
    Top,
    /// «Снизу»
    Bottom,
    /// «состояние»
    State,
    /// «раскладка»
    Layout,
    /// «взять управление»
    TakeControl,
    /// «отдать управление»
    GiveControlBack,
    /// «открыть окно сопряжения»
    OpenThePairingWindow,
    /// «подключиться по адресу»
    ConnectByAddress,
    /// «ответ на сопряжение»
    AnswerThePairing,
    /// «сохранить раскладку»
    SaveTheLayout,
    /// «буфер обмена»
    TheClipboard,
    /// «эффекты краёв»
    TheEdgeEffects,
    /// «удалить компьютер из пары»
    RemoveAComputerFromThePair,
    /// «сохранить диагностику»
    SaveTheDiagnostics,
    /// «забыть разрешения порталов»
    ForgetThePortalGrants,
    /// «повторить запуск службы ввода»
    StartTheInputServiceAgain,
    /// «остановить службу»
    StopTheService,
    /// «Готово: {}»
    Done,
    /// «Не получилось: {}»
    Failed,
    /// «Настройки не прочитаны ({}), показаны стандартные значения»
    TheSettingsWereNotReadTheDefaultsAreShown,
    /// «компьютер {}»
    ComputerNamed,
    /// «служба ещё не отвечала»
    TheServiceHasNotAnsweredYet,
    /// «Окно открыто»
    TheWindowIsOpen,
    /// «Служба работает ({endpoint})»
    TheServiceIsRunningEndpoint,
    /// «Связь со службой потеряна: {}»
    LostTheConnectionToTheService,
    /// «Служба остановлена»
    TheServiceIsStopped,
    /// «Служба останавливается»
    TheServiceIsStopping,
    /// «запрос»
    Request,
    /// «Служба не запущена: {name} не отправлено»
    TheServiceIsNotRunningNameWasNotSent,
    /// «Не удалось запустить службу: {}»
    CouldNotStartTheService,
    /// «не удалось запустить службу ({})»
    CouldNotStartTheService2,
    /// «Настройки созданы автоматически (config.toml)»
    TheSettingsWereCreatedAutomaticallyConfigToml,
    /// «Служба прислала пустой кадр»
    TheServiceSentAnEmptyFrame,
    /// «Сопряжение: партнёр {} предлагает сверить код»
    PairingOffersACodeToCompare,
    /// «без имени»
    WithoutAName,
    /// «Служба прислала неожиданный кадр ({})»
    TheServiceSentAnUnexpectedFrame,
    /// «Раскладка: ревизия {} от {}»
    LayoutRevisionFrom,
    /// «Раскладка: ревизия {} старше текущей {} — оставлена прежняя»
    LayoutRevisionIsOlderThanTheCurrentThePreviousOneWasKept,
    /// «Раскладка получена: ревизия {} от {}, участников {}»
    LayoutReceivedRevisionFromMembers,
    /// «Окно сопряжения открыто: {} ({} с)»
    ThePairingWindowIsOpenS,
    /// «Окно сопряжения закрыто»
    ThePairingWindowIsClosed,
    /// «Компьютер»
    ComputerTitle,
    /// «Ответ на неизвестный запрос ({})»
    AnAnswerToAnUnknownRequest,
    /// «{}: готово»
    Done2,
    /// «{}: готово ({text})»
    DoneText,
    /// «{name} сопряжён»
    NameIsPaired,
    /// «Служба не отвечает»
    TheServiceIsNotAnswering,
    /// «Служба работает»
    TheServiceIsRunning,
    /// «Служба не запущена»
    TheServiceIsNotRunning,
    /// «Нет данных от службы»
    NoDataFromTheService,
    /// «Ввод здесь»
    InputHere,
    /// «Готовим переход…»
    PreparingTheSwitch,
    /// «другой компьютер»
    TheOtherComputer,
    /// «Ввод на {peer}»
    InputOnPeer,
    /// «Ввод остановлен из-за ошибки»
    InputStoppedBecauseOfAnError,
    /// «Ввод: служба сообщила неизвестное состояние ({other})»
    InputTheServiceReportedAnUnknownStateOther,
    /// «раскладка {}»
    Layout2,
    /// «Есть связь»
    Connected,
    /// «Нет связи»
    Disconnected,
    /// «Связь {} неактивна: экран не найден»
    LinkIsInactiveTheDisplayWasNotFound,
    /// «Связь {} неактивна: экран «{}» не найден»
    LinkIsInactiveDisplayWasNotFound,
    /// «экраны ещё не получены»
    TheDisplaysHaveNotArrivedYet,
    /// «Связь {link}: Весь рабочий стол (авто) — {screens}»
    LinkTheWholeDesktopAutoScreens,
    /// «Пока никого: откройте окно сопряжения»
    NobodyYetOpenThePairingWindow,
    /// «в сети»
    Online,
    /// «не в сети»
    Offline,
    /// «управляет клавиатурой»
    OwnsTheKeyboard,
    /// «не управляет клавиатурой»
    DoesNotOwnTheKeyboard,
    /// «неизвестно»
    Unknown,
    /// «нет данных от службы»
    NoDataFromTheService2,
    /// «сеанс владельца {}»
    TheOwnerSSession,
    /// «Этот компьютер»
    ThisComputerTitle,
    /// «нет владельца»
    NoOwner,
    /// «Служба ввода остановлена»
    TheInputServiceIsStopped,
    /// «Нет аварийной клавиши: прервать захват клавиатуры на этом компьютере можно только мышью»
    NoEmergencyKeyTheKeyboardCanOnlyBeReleasedWithTheMouseHere,
    /// «Физический ввод на этом компьютере не завершает сеанс автоматически — переключение только с источника»
    PhysicalInputHereDoesNotEndTheSessionByItselfOnlyTheSourceCanSwitch,
    /// «Общий буфер обмена недоступен»
    TheSharedClipboardIsUnavailable,
    /// «Этот компьютер не принимает абсолютное положение курсора»
    ThisComputerDoesNotAcceptAnAbsolutePointerPosition,
    /// «Этот компьютер не может перехватывать ввод»
    ThisComputerCannotCaptureInput,
    /// «Этот компьютер не может вводить события»
    ThisComputerCannotInjectEvents,
    /// «Ограничение: {other}»
    LimitationOther,
    /// «Разрешение портала восстанавливается из сохранённого токена; диалог не ожидается»
    ThePortalGrantIsBeingRestoredFromTheSavedTokenNoDialogIsExpected,
    /// «Ожидается диалог разрешения портала — подтвердите его, иначе узел не сможет принять ввод»
    APortalPermissionDialogIsExpectedConfirmItOrThisNodeCannotTakeInput,
    /// «Доступ отклонён в диалоге разрешения. Нажмите «Забыть разрешения порталов» в настройках и попробуйте снова»
    AccessWasDeniedInThePermissionDialogPressForgetThePortalGrantsInTheSettingsAndTryAgain,
    /// «Порталы рабочего стола недоступны на этой системе»
    DesktopPortalsAreUnavailableOnThisSystem,
    /// «Wayland: перехват ввода возможен только через порталы рабочего стола»
    WaylandInputCanOnlyBeCapturedThroughDesktopPortals,
    /// «Последнее сообщение: {}»
    LastMessage,
    /// «Задержка: {} мс»
    DelayMs,
    /// «не удалось открыть файл настроек ({})»
    CouldNotOpenTheSettingsFile,
    /// «адрес не выбран — выберите адрес в списке и нажмите «Применить» снова»
    NoAddressIsSelectedPickOneInTheListAndPressApplyAgain,
    /// «каталоги настроек не определены — перезапустите окно без особых параметров»
    TheSettingsDirectoriesAreUnknownRestartTheWindowWithoutSpecialFlags,
    /// «Адрес этого компьютера: {address}»
    ThisComputerSAddressAddress,
    /// «Адрес сохранён»
    TheAddressIsSaved,
    /// «адрес не сохранён ({})»
    TheAddressWasNotSaved,
    /// «служба запущена не этим окном — остановите её сами»
    TheServiceWasNotStartedByThisWindowStopItYourself,
    /// «номер процесса службы неизвестен — остановите её сами»
    TheServiceSProcessIdIsUnknownStopItYourself,
    /// «Служба перезапускается (pid {pid})»
    TheServiceIsRestartingPidPid,
    /// «Служба останавливается (pid {pid})»
    TheServiceIsStoppingPidPid,
    /// «Не удалось остановить службу: {why}»
    CouldNotStopTheServiceWhy,
    /// «не удалось остановить службу ({why})»
    CouldNotStopTheServiceWhy2,
    /// «файл настроек не найден»
    TheSettingsFileWasNotFound,
    /// «не удалось запустить обмен со службой: {error}»
    CouldNotStartTalkingToTheServiceError,
    /// «служба уже запущена и отвечает»
    TheServiceIsAlreadyRunningAndAnswering,
    /// «{program} не остановил процесс {pid} (код {})»
    ProgramDidNotStopProcessPidCode,
    /// «неизвестен»
    Unknown2,
    /// «{program} не запустился: {error}»
    ProgramDidNotStartError,
    /// «не удалось определить каталоги: {why}»
    CouldNotWorkOutTheDirectoriesWhy,
    /// «Не удалось определить каталоги: {}»
    CouldNotWorkOutTheDirectories,
    /// «без кода»
    NoCode,
    /// «нет аварийной клавиши»
    NoEmergencyKey,
    /// «нет завершения сеанса по физическому вводу»
    TheSessionCannotBeEndedWithPhysicalInput,
    /// «общий буфер обмена недоступен»
    TheSharedClipboardIsUnavailable2,
    /// «нет абсолютного курсора»
    NoAbsolutePointer,
    /// «нет перехвата ввода»
    InputCannotBeCaptured,
    /// «нет ввода событий»
    EventsCannotBeInjected,
    /// «раскладку изменили на другом компьютере»
    TheLayoutWasChangedOnAnotherComputer,
    /// «документ раскладки неверный»
    TheLayoutDocumentIsInvalid,
    /// «нет координатора (владельца)»
    ThereIsNoCoordinatorOwner,
    /// «координатор не подтвердил правку»
    TheCoordinatorDidNotConfirmTheEdit,
    /// «такой правки больше нет»
    ThatEditIsGone,
    /// «этот компьютер не в паре»
    ThisComputerIsNotPaired,
    /// «не удалось записать список пар»
    CouldNotWriteThePeerList,
    /// «буфер обмена не включился»
    TheClipboardDidNotTurnOn,
    /// «не удалось записать файл диагностики»
    CouldNotWriteTheDiagnosticsFile,
    /// «не удалось договориться о сопряжении»
    ThePairingCouldNotBeAgreed,
    /// «окно сопряжения уже закрылось»
    ThePairingWindowIsAlreadyClosed,
    /// «окно сопряжения уже открыто»
    ThePairingWindowIsAlreadyOpen,
    /// «окно сопряжения не открыто»
    ThePairingWindowIsNotOpen,
    /// «адрес неверный»
    TheAddressIsWrong,
    /// «это сопряжение уже не ждёт ответа»
    ThisPairingIsNoLongerWaitingForAnAnswer,
    /// «взять управление не получилось»
    TakingControlDidNotWork,
    /// «переключение не получилось»
    TheSwitchDidNotWork,
    /// «порталы не ответили»
    ThePortalsDidNotAnswer,
    /// «агент ввода потерян»
    TheInputAgentIsGone,
    /// «ядро не поняло запрос»
    TheCoreDidNotUnderstandTheRequest,
    /// «код {other}»
    CodeOther,
    /// «управление у этого компьютера»
    ThisComputerHasControl,
    /// «управление отдано»
    ControlWasGivenBack,
    /// «подключение начато»
    Connecting,
    /// «ответ отправлен»
    TheAnswerWasSent,
    /// «раскладка сохранена»
    TheLayoutIsSaved,
    /// «буфер обмена переключён»
    TheClipboardWasSwitched,
    /// «эффекты краёв переключены»
    TheEdgeEffectsWereSwitched,
    /// «компьютер удалён из пары»
    TheComputerWasRemovedFromThePair,
    /// «разрешения порталов забыты»
    ThePortalGrantsWereForgotten,
    /// «повторный запуск службы ввода начат»
    TheInputServiceIsStartingAgain,
    /// «остановка службы начата»
    TheServiceIsStopping2,
    /// «диагностика сохранена»
    TheDiagnosticsAreSaved,
    /// «окно сопряжения открыто»
    ThePairingWindowIsOpen,
    /// «ядро ответило»
    TheCoreAnswered,
    /// «раскладку изменили на другом компьютере — перетащите ещё раз»
    TheLayoutWasChangedOnAnotherComputerDragItAgain,
    /// «коды не совпали или партнёр отказался»
    TheCodesDidNotMatchOrThePartnerRefused,
    /// «Поставьте компьютер рядом с соседом, без пересечений»
    PutTheComputerNextToANeighbourWithoutOverlaps,
    /// «Сохранено»
    Saved,
    /// «компьютер»
    Computer,
    /// «компьютера»
    ComputersFew,
    /// «компьютеров»
    ComputersMany,
    /// «экран»
    Display,
    /// «экрана»
    DisplaysFew,
    /// «экранов»
    DisplaysMany,
    /// «ввод»
    Input,
    /// «этот компьютер»
    ThisComputer,
    /// «этот компьютер · {backend}»
    ThisComputerBackend,
    /// «ввод здесь»
    InputHere2,
    /// «офлайн»
    Offline2,
    /// «Светлая тема»
    LightTheme,
    /// «Тёмная тема»
    DarkTheme,
    /// «Оформление»
    Appearance,
    /// «Открыть окно сопряжения»
    OpenThePairingWindow2,
    /// «Открыть файл настроек»
    OpenTheSettingsFile,
    /// «Удалить»
    Remove,
    /// «Удалить «{}» из пары? Сопрягать придётся заново.»
    RemoveFromThePairYouWillHaveToPairAgain,
    /// «Да, удалить»
    YesRemove,
    /// «Отмена»
    Cancel,
    /// «Пусто»
    Empty,
    /// «Пока никого: откройте окно сопряжения.»
    NobodyYetOpenThePairingWindow2,
    /// «Аварийная»
    Emergency,
    /// «На этой системе аварийная клавиша недоступна: прервать захват клавиатуры можно только мышью.»
    TheEmergencyKeyIsUnavailableOnThisSystemTheKeyboardCanOnlyBeReleasedWithTheMouse,
    /// «Всегда возвращает мышь и клавиатуру этому компьютеру — даже если служба зависла.»
    AlwaysGivesTheMouseAndKeyboardBackToThisComputerEvenIfTheServiceIsStuck,
    /// «Общий буфер»
    SharedClipboard,
    /// «Включён»
    On,
    /// «Выключен»
    Off,
    /// «Применяем…»
    Applying,
    /// «Эта система не может передавать содержимое буфера обмена»
    ThisSystemCannotCarryTheClipboard,
    /// «Текст можно копировать на одном компьютере и вставлять на другом»
    TextCanBeCopiedOnOneComputerAndPastedOnTheOther,
    /// «Общий буфер обмена выключен»
    TheSharedClipboardIsOff,
    /// «Служба сообщила: общий буфер обмена недоступен»
    TheServiceReportedThatTheSharedClipboardIsUnavailable,
    /// «Что передаётся»
    WhatIsCarried,
    /// «Только текст. Файлы и изображения — нет.»
    TextOnlyNoFilesNoImages,
    /// «Содержимое буфера не попадает в отчёты диагностики»
    TheClipboardNeverReachesTheDiagnosticsReports,
    /// «Отчёт»
    Report,
    /// «Сохранить диагностику»
    SaveTheDiagnostics2,
    /// «Отчёт ещё не сохранялся»
    NoReportHasBeenSavedYet,
    /// «Служба запишет отчёт без ключей и содержимого буфера обмена»
    TheServiceWritesTheReportWithoutKeysAndWithoutTheClipboard,
    /// «Каталог»
    Directory,
    /// «каталог состояния службы не определён»
    TheServiceSStateDirectoryIsUnknown,
    /// «Служба ввода»
    InputService,
    /// «Повторить запуск»
    StartAgain,
    /// «Состояние»
    State2,
    /// «Связь: {}»
    Link,
    /// «Адрес в LAN»
    LANAddress,
    /// «Применить»
    Apply,
    /// «После «Применить» служба перезапустится с новым адресом»
    AfterApplyTheServiceRestartsOnTheNewAddress,
    /// «Служба»
    Service,
    /// «Перезапустить»
    Restart,
    /// «Остановить»
    Stop,
    /// «Настройки созданы автоматически (config.toml).»
    TheSettingsWereCreatedAutomaticallyConfigToml2,
    /// «Понятно»
    Understood,
    /// «Порталы»
    Portals,
    /// «Забыть разрешения порталов»
    ForgetThePortalGrants2,
    /// «Служба попросит разрешения заново»
    TheServiceWillAskForTheGrantsAgain,
    /// «сочетание клавиш»
    KeyboardShortcut,
    /// «сочетание клавиш сохранено»
    KeyboardShortcutSaved,
    /// «История»
    History,
    /// «Обновить»
    Refresh,
    /// «Очистить историю»
    ClearHistory,
    /// «Копировать»
    Copy,
    /// «Отправлено»
    Sent,
    /// «Получено»
    Received,
    /// «Последние 30 передач текста. Только в памяти; секреты не сохраняются.»
    TheLast30TextsInMemoryOnlySecretsAreNotKept,
    /// «История пока пуста»
    TheHistoryIsEmptyForNow,
    /// «Передача»
    Transfer,
    /// «{direction} · {peer_name} · {age_seconds} с назад · {size_bytes} байт»
    DirectionPeerNameAgeSecondsSAgoSizeBytesBytes,
    /// «история буфера обмена»
    TheClipboardHistory,
    /// «скопировать из истории»
    CopyFromTheHistory,
    /// «очистить историю буфера»
    ClearTheClipboardHistory,
    /// «история буфера обновлена»
    TheClipboardHistoryIsRefreshed,
    /// «текст скопирован»
    TheTextIsCopied,
    /// «история буфера очищена»
    TheClipboardHistoryIsCleared,
    /// «Открыть MultiDeskFlow»
    OpenMultiDeskFlow,
    /// «Передать ввод → {}»
    SendInputTo,
    /// «Ввод → {}»
    Input2,
    /// «Остановить службу»
    StopTheService2,
    /// «Выход (служба продолжит работать)»
    QuitTheServiceKeepsRunning,
    /// «Передать ввод → этот компьютер»
    SendInputToThisComputer,
    /// «Язык»
    Language,
    /// «Сопряжение»
    Pairing,
    /// «Следующий компьютер»
    NextComputer,
    /// «Вернуться сюда»
    ReturnHere,
    /// «Забрать управление»
    ClaimOwnership,
    /// «Изменить»
    Change,
    /// «По умолчанию»
    Reset,
    /// «Нажмите сочетание. Esc — отмена.»
    PressAShortcutEscCancels,
    /// «Недоступно»
    Unavailable,
    /// «Ожидаем состояние службы»
    WaitingForTheServiceStatus,
    /// «Недопустимое сочетание»
    InvalidShortcut,
    /// «адрес не выбран»
    NoAddressIsSelected,
    /// «Выберите адрес в локальной сети в настройках.»
    PickAnAddressOnTheLocalNetworkInTheSettings,
    /// «Этот адрес вводят на другом компьютере. Сменить — в настройках.»
    ThisAddressIsTypedOnTheOtherComputerChangeItInTheSettings,
    /// «Окно сопряжения»
    PairingWindow,
    /// «] {left} с»
    LeftS,
    /// «Скажите этот адрес владельцу другого компьютера. Окно закроется само.»
    TellThisAddressToTheOtherComputerSOwnerTheWindowClosesByItself,
    /// «Закрыть окно · скоро»
    CloseTheWindowSoon,
    /// «Пока окно сопряжения закрывается автоматически.»
    ForNowThePairingWindowClosesByItself,
    /// «На одном компьютере откройте окно, на другом введите адрес.»
    OpenTheWindowOnOneComputerAndTypeTheAddressOnTheOther,
    /// «Окно закроется само через 120 секунд»
    TheWindowClosesByItselfIn120Seconds,
    /// «Подключиться по адресу»
    ConnectByAddress2,
    /// «Адрес с экрана другого компьютера»
    TheAddressFromTheOtherComputerSScreen,
    /// «Подключиться»
    Connect,
    /// «Подключаемся…»
    Connecting2,
    /// «Адрес показывает окно сопряжения на другом компьютере»
    TheOtherComputerSPairingWindowShowsTheAddress,
    /// «Код сопряжения»
    PairingCode,
    /// « подключается — коды совпадают?»
    IsConnectingDoTheCodesMatch,
    /// «Да, совпадают»
    YesTheyMatch,
    /// «Нет»
    No,
    /// «Свернуть»
    Collapse,
    /// «Подробнее»
    Details,
    /// «ЭТОТ · SHA-256 {}»
    THISSHA256,
    /// «Отпечатки совпадают, а коды нет — не соглашайтесь.»
    TheFingerprintsMatchButTheCodesDoNotDoNotAgree,
    /// «Пара создана»
    ThePairIsMade,
    /// «{name} сопряжён. Доверие сохранено — повторять не нужно.»
    NameIsPairedTheTrustIsSavedNoNeedToRepeatThis,
    /// «Готово»
    Done3,
    /// «Подключаемся к другому компьютеру…»
    ConnectingToTheOtherComputer,
    /// «Ждём, пока другой компьютер подключится…»
    WaitingForTheOtherComputerToConnect,
    /// «Код появится после подключения»
    TheCodeAppearsAfterTheConnection,
    /// «Шесть цифр появятся на обоих экранах одновременно»
    SixDigitsAppearOnBothScreensAtOnce,
    /// «Внешний вид»
    Appearance2,
    /// «Тема»
    Theme,
    /// «Светлая»
    Light,
    /// «Тёмная»
    Dark,
    /// «Связи между компьютерами»
    LinksBetweenComputers,
    /// «ASCII-изолиния»
    ASCIIIsoline,
    /// «Линия»
    Line,
    /// «Фон · частицы»
    BackgroundParticles,
    /// «Собираются»
    Gather,
    /// «Разлетаются»
    Scatter,
    /// «Без движения»
    Still,
    /// «Скорость частиц»
    ParticleSpeed,
    /// «ASCII-облака»
    ASCIIClouds,
    /// «Без облаков»
    NoClouds,
    /// «Эффекты у краёв»
    EdgeEffects,
    /// «Прятать курсор, пока ввод на другой машине»
    HideTheCursorWhileInputIsOnAnotherComputer,
    /// «Раскладка ещё не получена»
    TheLayoutHasNotArrivedYet,
    /// «01 — КАРТА МОНИТОРОВ»
    K01MONITORMAP,
    /// «Запустить»
    Start,
    /// «Ещё раз»
    OnceMore,
}

/// The text of `key` in `lang`.
pub const fn text(lang: Lang, key: Key) -> &'static str {
    match lang {
        Lang::Ru => ru(key),
        Lang::En => en(key),
    }
}

/// The Russian strings, as the mockup writes them.
const fn ru(key: Key) -> &'static str {
    match key {
        Key::Pairs => "Пары",
        Key::Keys => "Клавиши",
        Key::Clipboard => "Буфер обмена",
        Key::Diagnostics => "Диагностика",
        Key::Settings => "Настройки",
        Key::WhoThisComputerTrusts => "кому доверяет этот компьютер",
        Key::TheInputServiceSHotkeys => "горячие клавиши службы ввода",
        Key::CopyHerePasteThere => "скопировать здесь — вставить там",
        Key::AReportForTroubleshooting => "отчёт для разбора проблем",
        Key::AddressAndService => "адрес и служба",
        Key::Left => "Слева",
        Key::Right => "Справа",
        Key::Top => "Сверху",
        Key::Bottom => "Снизу",
        Key::State => "состояние",
        Key::Layout => "раскладка",
        Key::TakeControl => "взять управление",
        Key::GiveControlBack => "отдать управление",
        Key::OpenThePairingWindow => "открыть окно сопряжения",
        Key::ConnectByAddress => "подключиться по адресу",
        Key::AnswerThePairing => "ответ на сопряжение",
        Key::SaveTheLayout => "сохранить раскладку",
        Key::TheClipboard => "буфер обмена",
        Key::TheEdgeEffects => "эффекты краёв",
        Key::RemoveAComputerFromThePair => "удалить компьютер из пары",
        Key::SaveTheDiagnostics => "сохранить диагностику",
        Key::ForgetThePortalGrants => "забыть разрешения порталов",
        Key::StartTheInputServiceAgain => "повторить запуск службы ввода",
        Key::StopTheService => "остановить службу",
        Key::Done => "Готово: {}",
        Key::Failed => "Не получилось: {}",
        Key::TheSettingsWereNotReadTheDefaultsAreShown => "Настройки не прочитаны ({}), показаны стандартные значения",
        Key::ComputerNamed => "компьютер {}",
        Key::TheServiceHasNotAnsweredYet => "служба ещё не отвечала",
        Key::TheWindowIsOpen => "Окно открыто",
        Key::TheServiceIsRunningEndpoint => "Служба работает ({endpoint})",
        Key::LostTheConnectionToTheService => "Связь со службой потеряна: {}",
        Key::TheServiceIsStopped => "Служба остановлена",
        Key::TheServiceIsStopping => "Служба останавливается",
        Key::Request => "запрос",
        Key::TheServiceIsNotRunningNameWasNotSent => "Служба не запущена: {name} не отправлено",
        Key::CouldNotStartTheService => "Не удалось запустить службу: {}",
        Key::CouldNotStartTheService2 => "не удалось запустить службу ({})",
        Key::TheSettingsWereCreatedAutomaticallyConfigToml => "Настройки созданы автоматически (config.toml)",
        Key::TheServiceSentAnEmptyFrame => "Служба прислала пустой кадр",
        Key::PairingOffersACodeToCompare => "Сопряжение: партнёр {} предлагает сверить код",
        Key::WithoutAName => "без имени",
        Key::TheServiceSentAnUnexpectedFrame => "Служба прислала неожиданный кадр ({})",
        Key::LayoutRevisionFrom => "Раскладка: ревизия {} от {}",
        Key::LayoutRevisionIsOlderThanTheCurrentThePreviousOneWasKept => "Раскладка: ревизия {} старше текущей {} — оставлена прежняя",
        Key::LayoutReceivedRevisionFromMembers => "Раскладка получена: ревизия {} от {}, участников {}",
        Key::ThePairingWindowIsOpenS => "Окно сопряжения открыто: {} ({} с)",
        Key::ThePairingWindowIsClosed => "Окно сопряжения закрыто",
        Key::ComputerTitle => "Компьютер",
        Key::AnAnswerToAnUnknownRequest => "Ответ на неизвестный запрос ({})",
        Key::Done2 => "{}: готово",
        Key::DoneText => "{}: готово ({text})",
        Key::NameIsPaired => "{name} сопряжён",
        Key::TheServiceIsNotAnswering => "Служба не отвечает",
        Key::TheServiceIsRunning => "Служба работает",
        Key::TheServiceIsNotRunning => "Служба не запущена",
        Key::NoDataFromTheService => "Нет данных от службы",
        Key::InputHere => "Ввод здесь",
        Key::PreparingTheSwitch => "Готовим переход…",
        Key::TheOtherComputer => "другой компьютер",
        Key::InputOnPeer => "Ввод на {peer}",
        Key::InputStoppedBecauseOfAnError => "Ввод остановлен из-за ошибки",
        Key::InputTheServiceReportedAnUnknownStateOther => "Ввод: служба сообщила неизвестное состояние ({other})",
        Key::Layout2 => "раскладка {}",
        Key::Connected => "Есть связь",
        Key::Disconnected => "Нет связи",
        Key::LinkIsInactiveTheDisplayWasNotFound => "Связь {} неактивна: экран не найден",
        Key::LinkIsInactiveDisplayWasNotFound => "Связь {} неактивна: экран «{}» не найден",
        Key::TheDisplaysHaveNotArrivedYet => "экраны ещё не получены",
        Key::LinkTheWholeDesktopAutoScreens => "Связь {link}: Весь рабочий стол (авто) — {screens}",
        Key::NobodyYetOpenThePairingWindow => "Пока никого: откройте окно сопряжения",
        Key::Online => "в сети",
        Key::Offline => "не в сети",
        Key::OwnsTheKeyboard => "управляет клавиатурой",
        Key::DoesNotOwnTheKeyboard => "не управляет клавиатурой",
        Key::Unknown => "неизвестно",
        Key::NoDataFromTheService2 => "нет данных от службы",
        Key::TheOwnerSSession => "сеанс владельца {}",
        Key::ThisComputerTitle => "Этот компьютер",
        Key::NoOwner => "нет владельца",
        Key::TheInputServiceIsStopped => "Служба ввода остановлена",
        Key::NoEmergencyKeyTheKeyboardCanOnlyBeReleasedWithTheMouseHere => "Нет аварийной клавиши: прервать захват клавиатуры на этом компьютере можно только мышью",
        Key::PhysicalInputHereDoesNotEndTheSessionByItselfOnlyTheSourceCanSwitch => "Физический ввод на этом компьютере не завершает сеанс автоматически — переключение только с источника",
        Key::TheSharedClipboardIsUnavailable => "Общий буфер обмена недоступен",
        Key::ThisComputerDoesNotAcceptAnAbsolutePointerPosition => "Этот компьютер не принимает абсолютное положение курсора",
        Key::ThisComputerCannotCaptureInput => "Этот компьютер не может перехватывать ввод",
        Key::ThisComputerCannotInjectEvents => "Этот компьютер не может вводить события",
        Key::LimitationOther => "Ограничение: {other}",
        Key::ThePortalGrantIsBeingRestoredFromTheSavedTokenNoDialogIsExpected => "Разрешение портала восстанавливается из сохранённого токена; диалог не ожидается",
        Key::APortalPermissionDialogIsExpectedConfirmItOrThisNodeCannotTakeInput => "Ожидается диалог разрешения портала — подтвердите его, иначе узел не сможет принять ввод",
        Key::AccessWasDeniedInThePermissionDialogPressForgetThePortalGrantsInTheSettingsAndTryAgain => "Доступ отклонён в диалоге разрешения. Нажмите «Забыть разрешения порталов» в настройках и попробуйте снова",
        Key::DesktopPortalsAreUnavailableOnThisSystem => "Порталы рабочего стола недоступны на этой системе",
        Key::WaylandInputCanOnlyBeCapturedThroughDesktopPortals => "Wayland: перехват ввода возможен только через порталы рабочего стола",
        Key::LastMessage => "Последнее сообщение: {}",
        Key::DelayMs => "Задержка: {} мс",
        Key::CouldNotOpenTheSettingsFile => "не удалось открыть файл настроек ({})",
        Key::NoAddressIsSelectedPickOneInTheListAndPressApplyAgain => "адрес не выбран — выберите адрес в списке и нажмите «Применить» снова",
        Key::TheSettingsDirectoriesAreUnknownRestartTheWindowWithoutSpecialFlags => "каталоги настроек не определены — перезапустите окно без особых параметров",
        Key::ThisComputerSAddressAddress => "Адрес этого компьютера: {address}",
        Key::TheAddressIsSaved => "Адрес сохранён",
        Key::TheAddressWasNotSaved => "адрес не сохранён ({})",
        Key::TheServiceWasNotStartedByThisWindowStopItYourself => "служба запущена не этим окном — остановите её сами",
        Key::TheServiceSProcessIdIsUnknownStopItYourself => "номер процесса службы неизвестен — остановите её сами",
        Key::TheServiceIsRestartingPidPid => "Служба перезапускается (pid {pid})",
        Key::TheServiceIsStoppingPidPid => "Служба останавливается (pid {pid})",
        Key::CouldNotStopTheServiceWhy => "Не удалось остановить службу: {why}",
        Key::CouldNotStopTheServiceWhy2 => "не удалось остановить службу ({why})",
        Key::TheSettingsFileWasNotFound => "файл настроек не найден",
        Key::CouldNotStartTalkingToTheServiceError => "не удалось запустить обмен со службой: {error}",
        Key::TheServiceIsAlreadyRunningAndAnswering => "служба уже запущена и отвечает",
        Key::ProgramDidNotStopProcessPidCode => "{program} не остановил процесс {pid} (код {})",
        Key::Unknown2 => "неизвестен",
        Key::ProgramDidNotStartError => "{program} не запустился: {error}",
        Key::CouldNotWorkOutTheDirectoriesWhy => "не удалось определить каталоги: {why}",
        Key::CouldNotWorkOutTheDirectories => "Не удалось определить каталоги: {}",
        Key::NoCode => "без кода",
        Key::NoEmergencyKey => "нет аварийной клавиши",
        Key::TheSessionCannotBeEndedWithPhysicalInput => "нет завершения сеанса по физическому вводу",
        Key::TheSharedClipboardIsUnavailable2 => "общий буфер обмена недоступен",
        Key::NoAbsolutePointer => "нет абсолютного курсора",
        Key::InputCannotBeCaptured => "нет перехвата ввода",
        Key::EventsCannotBeInjected => "нет ввода событий",
        Key::TheLayoutWasChangedOnAnotherComputer => "раскладку изменили на другом компьютере",
        Key::TheLayoutDocumentIsInvalid => "документ раскладки неверный",
        Key::ThereIsNoCoordinatorOwner => "нет координатора (владельца)",
        Key::TheCoordinatorDidNotConfirmTheEdit => "координатор не подтвердил правку",
        Key::ThatEditIsGone => "такой правки больше нет",
        Key::ThisComputerIsNotPaired => "этот компьютер не в паре",
        Key::CouldNotWriteThePeerList => "не удалось записать список пар",
        Key::TheClipboardDidNotTurnOn => "буфер обмена не включился",
        Key::CouldNotWriteTheDiagnosticsFile => "не удалось записать файл диагностики",
        Key::ThePairingCouldNotBeAgreed => "не удалось договориться о сопряжении",
        Key::ThePairingWindowIsAlreadyClosed => "окно сопряжения уже закрылось",
        Key::ThePairingWindowIsAlreadyOpen => "окно сопряжения уже открыто",
        Key::ThePairingWindowIsNotOpen => "окно сопряжения не открыто",
        Key::TheAddressIsWrong => "адрес неверный",
        Key::ThisPairingIsNoLongerWaitingForAnAnswer => "это сопряжение уже не ждёт ответа",
        Key::TakingControlDidNotWork => "взять управление не получилось",
        Key::TheSwitchDidNotWork => "переключение не получилось",
        Key::ThePortalsDidNotAnswer => "порталы не ответили",
        Key::TheInputAgentIsGone => "агент ввода потерян",
        Key::TheCoreDidNotUnderstandTheRequest => "ядро не поняло запрос",
        Key::CodeOther => "код {other}",
        Key::ThisComputerHasControl => "управление у этого компьютера",
        Key::ControlWasGivenBack => "управление отдано",
        Key::Connecting => "подключение начато",
        Key::TheAnswerWasSent => "ответ отправлен",
        Key::TheLayoutIsSaved => "раскладка сохранена",
        Key::TheClipboardWasSwitched => "буфер обмена переключён",
        Key::TheEdgeEffectsWereSwitched => "эффекты краёв переключены",
        Key::TheComputerWasRemovedFromThePair => "компьютер удалён из пары",
        Key::ThePortalGrantsWereForgotten => "разрешения порталов забыты",
        Key::TheInputServiceIsStartingAgain => "повторный запуск службы ввода начат",
        Key::TheServiceIsStopping2 => "остановка службы начата",
        Key::TheDiagnosticsAreSaved => "диагностика сохранена",
        Key::ThePairingWindowIsOpen => "окно сопряжения открыто",
        Key::TheCoreAnswered => "ядро ответило",
        Key::TheLayoutWasChangedOnAnotherComputerDragItAgain => "раскладку изменили на другом компьютере — перетащите ещё раз",
        Key::TheCodesDidNotMatchOrThePartnerRefused => "коды не совпали или партнёр отказался",
        Key::PutTheComputerNextToANeighbourWithoutOverlaps => "Поставьте компьютер рядом с соседом, без пересечений",
        Key::Saved => "Сохранено",
        Key::Computer => "компьютер",
        Key::ComputersFew => "компьютера",
        Key::ComputersMany => "компьютеров",
        Key::Display => "экран",
        Key::DisplaysFew => "экрана",
        Key::DisplaysMany => "экранов",
        Key::Input => "ввод",
        Key::ThisComputer => "этот компьютер",
        Key::ThisComputerBackend => "этот компьютер · {backend}",
        Key::InputHere2 => "ввод здесь",
        Key::Offline2 => "офлайн",
        Key::LightTheme => "Светлая тема",
        Key::DarkTheme => "Тёмная тема",
        Key::Appearance => "Оформление",
        Key::OpenThePairingWindow2 => "Открыть окно сопряжения",
        Key::OpenTheSettingsFile => "Открыть файл настроек",
        Key::Remove => "Удалить",
        Key::RemoveFromThePairYouWillHaveToPairAgain => "Удалить «{}» из пары? Сопрягать придётся заново.",
        Key::YesRemove => "Да, удалить",
        Key::Cancel => "Отмена",
        Key::Empty => "Пусто",
        Key::NobodyYetOpenThePairingWindow2 => "Пока никого: откройте окно сопряжения.",
        Key::Emergency => "Аварийная",
        Key::TheEmergencyKeyIsUnavailableOnThisSystemTheKeyboardCanOnlyBeReleasedWithTheMouse => "На этой системе аварийная клавиша недоступна: прервать захват клавиатуры можно только мышью.",
        Key::AlwaysGivesTheMouseAndKeyboardBackToThisComputerEvenIfTheServiceIsStuck => "Всегда возвращает мышь и клавиатуру этому компьютеру — даже если служба зависла.",
        Key::SharedClipboard => "Общий буфер",
        Key::On => "Включён",
        Key::Off => "Выключен",
        Key::Applying => "Применяем…",
        Key::ThisSystemCannotCarryTheClipboard => "Эта система не может передавать содержимое буфера обмена",
        Key::TextCanBeCopiedOnOneComputerAndPastedOnTheOther => "Текст можно копировать на одном компьютере и вставлять на другом",
        Key::TheSharedClipboardIsOff => "Общий буфер обмена выключен",
        Key::TheServiceReportedThatTheSharedClipboardIsUnavailable => "Служба сообщила: общий буфер обмена недоступен",
        Key::WhatIsCarried => "Что передаётся",
        Key::TextOnlyNoFilesNoImages => "Только текст. Файлы и изображения — нет.",
        Key::TheClipboardNeverReachesTheDiagnosticsReports => "Содержимое буфера не попадает в отчёты диагностики",
        Key::Report => "Отчёт",
        Key::SaveTheDiagnostics2 => "Сохранить диагностику",
        Key::NoReportHasBeenSavedYet => "Отчёт ещё не сохранялся",
        Key::TheServiceWritesTheReportWithoutKeysAndWithoutTheClipboard => "Служба запишет отчёт без ключей и содержимого буфера обмена",
        Key::Directory => "Каталог",
        Key::TheServiceSStateDirectoryIsUnknown => "каталог состояния службы не определён",
        Key::InputService => "Служба ввода",
        Key::StartAgain => "Повторить запуск",
        Key::State2 => "Состояние",
        Key::Link => "Связь: {}",
        Key::LANAddress => "Адрес в LAN",
        Key::Apply => "Применить",
        Key::AfterApplyTheServiceRestartsOnTheNewAddress => "После «Применить» служба перезапустится с новым адресом",
        Key::Service => "Служба",
        Key::Restart => "Перезапустить",
        Key::Stop => "Остановить",
        Key::TheSettingsWereCreatedAutomaticallyConfigToml2 => "Настройки созданы автоматически (config.toml).",
        Key::Understood => "Понятно",
        Key::Portals => "Порталы",
        Key::ForgetThePortalGrants2 => "Забыть разрешения порталов",
        Key::TheServiceWillAskForTheGrantsAgain => "Служба попросит разрешения заново",
        Key::KeyboardShortcut => "сочетание клавиш",
        Key::KeyboardShortcutSaved => "сочетание клавиш сохранено",
        Key::History => "История",
        Key::Refresh => "Обновить",
        Key::ClearHistory => "Очистить историю",
        Key::Copy => "Копировать",
        Key::Sent => "Отправлено",
        Key::Received => "Получено",
        Key::TheLast30TextsInMemoryOnlySecretsAreNotKept => "Последние 30 передач текста. Только в памяти; секреты не сохраняются.",
        Key::TheHistoryIsEmptyForNow => "История пока пуста",
        Key::Transfer => "Передача",
        Key::DirectionPeerNameAgeSecondsSAgoSizeBytesBytes => "{direction} · {peer_name} · {age_seconds} с назад · {size_bytes} байт",
        Key::TheClipboardHistory => "история буфера обмена",
        Key::CopyFromTheHistory => "скопировать из истории",
        Key::ClearTheClipboardHistory => "очистить историю буфера",
        Key::TheClipboardHistoryIsRefreshed => "история буфера обновлена",
        Key::TheTextIsCopied => "текст скопирован",
        Key::TheClipboardHistoryIsCleared => "история буфера очищена",
        Key::OpenMultiDeskFlow => "Открыть MultiDeskFlow",
        Key::SendInputTo => "Передать ввод → {}",
        Key::Input2 => "Ввод → {}",
        Key::StopTheService2 => "Остановить службу",
        Key::QuitTheServiceKeepsRunning => "Выход (служба продолжит работать)",
        Key::SendInputToThisComputer => "Передать ввод → этот компьютер",
        Key::Language => "Язык",
        Key::Pairing => "Сопряжение",
        Key::NextComputer => "Следующий компьютер",
        Key::ReturnHere => "Вернуться сюда",
        Key::ClaimOwnership => "Забрать управление",
        Key::Change => "Изменить",
        Key::Reset => "По умолчанию",
        Key::PressAShortcutEscCancels => "Нажмите сочетание. Esc — отмена.",
        Key::Unavailable => "Недоступно",
        Key::WaitingForTheServiceStatus => "Ожидаем состояние службы",
        Key::InvalidShortcut => "Недопустимое сочетание",
        Key::NoAddressIsSelected => "адрес не выбран",
        Key::PickAnAddressOnTheLocalNetworkInTheSettings => "Выберите адрес в локальной сети в настройках.",
        Key::ThisAddressIsTypedOnTheOtherComputerChangeItInTheSettings => "Этот адрес вводят на другом компьютере. Сменить — в настройках.",
        Key::PairingWindow => "Окно сопряжения",
        Key::LeftS => "] {left} с",
        Key::TellThisAddressToTheOtherComputerSOwnerTheWindowClosesByItself => "Скажите этот адрес владельцу другого компьютера. Окно закроется само.",
        Key::CloseTheWindowSoon => "Закрыть окно · скоро",
        Key::ForNowThePairingWindowClosesByItself => "Пока окно сопряжения закрывается автоматически.",
        Key::OpenTheWindowOnOneComputerAndTypeTheAddressOnTheOther => "На одном компьютере откройте окно, на другом введите адрес.",
        Key::TheWindowClosesByItselfIn120Seconds => "Окно закроется само через 120 секунд",
        Key::ConnectByAddress2 => "Подключиться по адресу",
        Key::TheAddressFromTheOtherComputerSScreen => "Адрес с экрана другого компьютера",
        Key::Connect => "Подключиться",
        Key::Connecting2 => "Подключаемся…",
        Key::TheOtherComputerSPairingWindowShowsTheAddress => "Адрес показывает окно сопряжения на другом компьютере",
        Key::PairingCode => "Код сопряжения",
        Key::IsConnectingDoTheCodesMatch => " подключается — коды совпадают?",
        Key::YesTheyMatch => "Да, совпадают",
        Key::No => "Нет",
        Key::Collapse => "Свернуть",
        Key::Details => "Подробнее",
        Key::THISSHA256 => "ЭТОТ · SHA-256 {}",
        Key::TheFingerprintsMatchButTheCodesDoNotDoNotAgree => "Отпечатки совпадают, а коды нет — не соглашайтесь.",
        Key::ThePairIsMade => "Пара создана",
        Key::NameIsPairedTheTrustIsSavedNoNeedToRepeatThis => "{name} сопряжён. Доверие сохранено — повторять не нужно.",
        Key::Done3 => "Готово",
        Key::ConnectingToTheOtherComputer => "Подключаемся к другому компьютеру…",
        Key::WaitingForTheOtherComputerToConnect => "Ждём, пока другой компьютер подключится…",
        Key::TheCodeAppearsAfterTheConnection => "Код появится после подключения",
        Key::SixDigitsAppearOnBothScreensAtOnce => "Шесть цифр появятся на обоих экранах одновременно",
        Key::Appearance2 => "Внешний вид",
        Key::Theme => "Тема",
        Key::Light => "Светлая",
        Key::Dark => "Тёмная",
        Key::LinksBetweenComputers => "Связи между компьютерами",
        Key::ASCIIIsoline => "ASCII-изолиния",
        Key::Line => "Линия",
        Key::BackgroundParticles => "Фон · частицы",
        Key::Gather => "Собираются",
        Key::Scatter => "Разлетаются",
        Key::Still => "Без движения",
        Key::ParticleSpeed => "Скорость частиц",
        Key::ASCIIClouds => "ASCII-облака",
        Key::NoClouds => "Без облаков",
        Key::EdgeEffects => "Эффекты у краёв",
        Key::HideTheCursorWhileInputIsOnAnotherComputer => "Прятать курсор, пока ввод на другой машине",
        Key::TheLayoutHasNotArrivedYet => "Раскладка ещё не получена",
        Key::K01MONITORMAP => "01 — КАРТА МОНИТОРОВ",
        Key::Start => "Запустить",
        Key::OnceMore => "Ещё раз",
    }
}

/// The English strings.
const fn en(key: Key) -> &'static str {
    match key {
        Key::Pairs => "Pairs",
        Key::Keys => "Keys",
        Key::Clipboard => "Clipboard",
        Key::Diagnostics => "Diagnostics",
        Key::Settings => "Settings",
        Key::WhoThisComputerTrusts => "who this computer trusts",
        Key::TheInputServiceSHotkeys => "the input service's hotkeys",
        Key::CopyHerePasteThere => "copy here, paste there",
        Key::AReportForTroubleshooting => "a report for troubleshooting",
        Key::AddressAndService => "address and service",
        Key::Left => "Left",
        Key::Right => "Right",
        Key::Top => "Top",
        Key::Bottom => "Bottom",
        Key::State => "state",
        Key::Layout => "layout",
        Key::TakeControl => "take control",
        Key::GiveControlBack => "give control back",
        Key::OpenThePairingWindow => "open the pairing window",
        Key::ConnectByAddress => "connect by address",
        Key::AnswerThePairing => "answer the pairing",
        Key::SaveTheLayout => "save the layout",
        Key::TheClipboard => "the clipboard",
        Key::TheEdgeEffects => "the edge effects",
        Key::RemoveAComputerFromThePair => "remove a computer from the pair",
        Key::SaveTheDiagnostics => "save the diagnostics",
        Key::ForgetThePortalGrants => "forget the portal grants",
        Key::StartTheInputServiceAgain => "start the input service again",
        Key::StopTheService => "stop the service",
        Key::Done => "Done: {}",
        Key::Failed => "Failed: {}",
        Key::TheSettingsWereNotReadTheDefaultsAreShown => "the settings were not read ({}); the defaults are shown",
        Key::ComputerNamed => "computer {}",
        Key::TheServiceHasNotAnsweredYet => "the service has not answered yet",
        Key::TheWindowIsOpen => "The window is open",
        Key::TheServiceIsRunningEndpoint => "The service is running ({endpoint})",
        Key::LostTheConnectionToTheService => "Lost the connection to the service: {}",
        Key::TheServiceIsStopped => "The service is stopped",
        Key::TheServiceIsStopping => "The service is stopping",
        Key::Request => "request",
        Key::TheServiceIsNotRunningNameWasNotSent => "The service is not running: {name} was not sent",
        Key::CouldNotStartTheService => "Could not start the service: {}",
        Key::CouldNotStartTheService2 => "could not start the service ({})",
        Key::TheSettingsWereCreatedAutomaticallyConfigToml => "The settings were created automatically (config.toml)",
        Key::TheServiceSentAnEmptyFrame => "The service sent an empty frame",
        Key::PairingOffersACodeToCompare => "Pairing: {} offers a code to compare",
        Key::WithoutAName => "without a name",
        Key::TheServiceSentAnUnexpectedFrame => "The service sent an unexpected frame ({})",
        Key::LayoutRevisionFrom => "Layout: revision {} from {}",
        Key::LayoutRevisionIsOlderThanTheCurrentThePreviousOneWasKept => "Layout: revision {} is older than the current {} — the previous one was kept",
        Key::LayoutReceivedRevisionFromMembers => "Layout received: revision {} from {}, {} members",
        Key::ThePairingWindowIsOpenS => "The pairing window is open: {} ({} s)",
        Key::ThePairingWindowIsClosed => "The pairing window is closed",
        Key::ComputerTitle => "Computer",
        Key::AnAnswerToAnUnknownRequest => "An answer to an unknown request ({})",
        Key::Done2 => "{}: done",
        Key::DoneText => "{}: done ({text})",
        Key::NameIsPaired => "{name} is paired",
        Key::TheServiceIsNotAnswering => "The service is not answering",
        Key::TheServiceIsRunning => "The service is running",
        Key::TheServiceIsNotRunning => "The service is not running",
        Key::NoDataFromTheService => "No data from the service",
        Key::InputHere => "Input here",
        Key::PreparingTheSwitch => "Preparing the switch…",
        Key::TheOtherComputer => "the other computer",
        Key::InputOnPeer => "Input on {peer}",
        Key::InputStoppedBecauseOfAnError => "Input stopped because of an error",
        Key::InputTheServiceReportedAnUnknownStateOther => "Input: the service reported an unknown state ({other})",
        Key::Layout2 => "layout {}",
        Key::Connected => "Connected",
        Key::Disconnected => "Disconnected",
        Key::LinkIsInactiveTheDisplayWasNotFound => "Link {} is inactive: the display was not found",
        Key::LinkIsInactiveDisplayWasNotFound => "Link {} is inactive: display «{}» was not found",
        Key::TheDisplaysHaveNotArrivedYet => "the displays have not arrived yet",
        Key::LinkTheWholeDesktopAutoScreens => "Link {}: the whole desktop (auto) — {screens}",
        Key::NobodyYetOpenThePairingWindow => "Nobody yet: open the pairing window",
        Key::Online => "online",
        Key::Offline => "offline",
        Key::OwnsTheKeyboard => "owns the keyboard",
        Key::DoesNotOwnTheKeyboard => "does not own the keyboard",
        Key::Unknown => "unknown",
        Key::NoDataFromTheService2 => "no data from the service",
        Key::TheOwnerSSession => "the owner's session {}",
        Key::ThisComputerTitle => "This computer",
        Key::NoOwner => "no owner",
        Key::TheInputServiceIsStopped => "The input service is stopped",
        Key::NoEmergencyKeyTheKeyboardCanOnlyBeReleasedWithTheMouseHere => "No emergency key: the keyboard can only be released with the mouse here",
        Key::PhysicalInputHereDoesNotEndTheSessionByItselfOnlyTheSourceCanSwitch => "Physical input here does not end the session by itself — only the source can switch",
        Key::TheSharedClipboardIsUnavailable => "The shared clipboard is unavailable",
        Key::ThisComputerDoesNotAcceptAnAbsolutePointerPosition => "This computer does not accept an absolute pointer position",
        Key::ThisComputerCannotCaptureInput => "This computer cannot capture input",
        Key::ThisComputerCannotInjectEvents => "This computer cannot inject events",
        Key::LimitationOther => "Limitation: {other}",
        Key::ThePortalGrantIsBeingRestoredFromTheSavedTokenNoDialogIsExpected => "The portal grant is being restored from the saved token; no dialog is expected",
        Key::APortalPermissionDialogIsExpectedConfirmItOrThisNodeCannotTakeInput => "A portal permission dialog is expected — confirm it, or this node cannot take input",
        Key::AccessWasDeniedInThePermissionDialogPressForgetThePortalGrantsInTheSettingsAndTryAgain => "Access was denied in the permission dialog. Press «Forget the portal grants» in the settings and try again",
        Key::DesktopPortalsAreUnavailableOnThisSystem => "Desktop portals are unavailable on this system",
        Key::WaylandInputCanOnlyBeCapturedThroughDesktopPortals => "Wayland: input can only be captured through desktop portals",
        Key::LastMessage => "Last message: {}",
        Key::DelayMs => "Delay: {} ms",
        Key::CouldNotOpenTheSettingsFile => "could not open the settings file ({})",
        Key::NoAddressIsSelectedPickOneInTheListAndPressApplyAgain => "no address is selected — pick one in the list and press «Apply» again",
        Key::TheSettingsDirectoriesAreUnknownRestartTheWindowWithoutSpecialFlags => "the settings directories are unknown — restart the window without special flags",
        Key::ThisComputerSAddressAddress => "This computer's address: {address}",
        Key::TheAddressIsSaved => "The address is saved",
        Key::TheAddressWasNotSaved => "the address was not saved ({})",
        Key::TheServiceWasNotStartedByThisWindowStopItYourself => "the service was not started by this window — stop it yourself",
        Key::TheServiceSProcessIdIsUnknownStopItYourself => "the service's process id is unknown — stop it yourself",
        Key::TheServiceIsRestartingPidPid => "The service is restarting (pid {pid})",
        Key::TheServiceIsStoppingPidPid => "The service is stopping (pid {pid})",
        Key::CouldNotStopTheServiceWhy => "Could not stop the service: {why}",
        Key::CouldNotStopTheServiceWhy2 => "could not stop the service ({why})",
        Key::TheSettingsFileWasNotFound => "the settings file was not found",
        Key::CouldNotStartTalkingToTheServiceError => "could not start talking to the service: {error}",
        Key::TheServiceIsAlreadyRunningAndAnswering => "the service is already running and answering",
        Key::ProgramDidNotStopProcessPidCode => "{program} did not stop process {pid} (code {})",
        Key::Unknown2 => "unknown",
        Key::ProgramDidNotStartError => "{program} did not start: {error}",
        Key::CouldNotWorkOutTheDirectoriesWhy => "could not work out the directories: {why}",
        Key::CouldNotWorkOutTheDirectories => "Could not work out the directories: {}",
        Key::NoCode => "no code",
        Key::NoEmergencyKey => "no emergency key",
        Key::TheSessionCannotBeEndedWithPhysicalInput => "the session cannot be ended with physical input",
        Key::TheSharedClipboardIsUnavailable2 => "the shared clipboard is unavailable",
        Key::NoAbsolutePointer => "no absolute pointer",
        Key::InputCannotBeCaptured => "input cannot be captured",
        Key::EventsCannotBeInjected => "events cannot be injected",
        Key::TheLayoutWasChangedOnAnotherComputer => "the layout was changed on another computer",
        Key::TheLayoutDocumentIsInvalid => "the layout document is invalid",
        Key::ThereIsNoCoordinatorOwner => "there is no coordinator (owner)",
        Key::TheCoordinatorDidNotConfirmTheEdit => "the coordinator did not confirm the edit",
        Key::ThatEditIsGone => "that edit is gone",
        Key::ThisComputerIsNotPaired => "this computer is not paired",
        Key::CouldNotWriteThePeerList => "could not write the peer list",
        Key::TheClipboardDidNotTurnOn => "the clipboard did not turn on",
        Key::CouldNotWriteTheDiagnosticsFile => "could not write the diagnostics file",
        Key::ThePairingCouldNotBeAgreed => "the pairing could not be agreed",
        Key::ThePairingWindowIsAlreadyClosed => "the pairing window is already closed",
        Key::ThePairingWindowIsAlreadyOpen => "the pairing window is already open",
        Key::ThePairingWindowIsNotOpen => "the pairing window is not open",
        Key::TheAddressIsWrong => "the address is wrong",
        Key::ThisPairingIsNoLongerWaitingForAnAnswer => "this pairing is no longer waiting for an answer",
        Key::TakingControlDidNotWork => "taking control did not work",
        Key::TheSwitchDidNotWork => "the switch did not work",
        Key::ThePortalsDidNotAnswer => "the portals did not answer",
        Key::TheInputAgentIsGone => "the input agent is gone",
        Key::TheCoreDidNotUnderstandTheRequest => "the core did not understand the request",
        Key::CodeOther => "code {other}",
        Key::ThisComputerHasControl => "this computer has control",
        Key::ControlWasGivenBack => "control was given back",
        Key::Connecting => "connecting",
        Key::TheAnswerWasSent => "the answer was sent",
        Key::TheLayoutIsSaved => "the layout is saved",
        Key::TheClipboardWasSwitched => "the clipboard was switched",
        Key::TheEdgeEffectsWereSwitched => "the edge effects were switched",
        Key::TheComputerWasRemovedFromThePair => "the computer was removed from the pair",
        Key::ThePortalGrantsWereForgotten => "the portal grants were forgotten",
        Key::TheInputServiceIsStartingAgain => "the input service is starting again",
        Key::TheServiceIsStopping2 => "the service is stopping",
        Key::TheDiagnosticsAreSaved => "the diagnostics are saved",
        Key::ThePairingWindowIsOpen => "the pairing window is open",
        Key::TheCoreAnswered => "the core answered",
        Key::TheLayoutWasChangedOnAnotherComputerDragItAgain => "the layout was changed on another computer — drag it again",
        Key::TheCodesDidNotMatchOrThePartnerRefused => "the codes did not match, or the partner refused",
        Key::PutTheComputerNextToANeighbourWithoutOverlaps => "Put the computer next to a neighbour, without overlaps",
        Key::Saved => "Saved",
        Key::Computer => "computer",
        Key::ComputersFew => "computers",
        Key::ComputersMany => "computers",
        Key::Display => "display",
        Key::DisplaysFew => "displays",
        Key::DisplaysMany => "displays",
        Key::Input => "input",
        Key::ThisComputer => "this computer",
        Key::ThisComputerBackend => "this computer · {backend}",
        Key::InputHere2 => "input here",
        Key::Offline2 => "offline",
        Key::LightTheme => "Light theme",
        Key::DarkTheme => "Dark theme",
        Key::Appearance => "Appearance",
        Key::OpenThePairingWindow2 => "Open the pairing window",
        Key::OpenTheSettingsFile => "Open the settings file",
        Key::Remove => "Remove",
        Key::RemoveFromThePairYouWillHaveToPairAgain => "Remove «{}» from the pair? You will have to pair again.",
        Key::YesRemove => "Yes, remove",
        Key::Cancel => "Cancel",
        Key::Empty => "Empty",
        Key::NobodyYetOpenThePairingWindow2 => "Nobody yet: open the pairing window.",
        Key::Emergency => "Emergency",
        Key::TheEmergencyKeyIsUnavailableOnThisSystemTheKeyboardCanOnlyBeReleasedWithTheMouse => "The emergency key is unavailable on this system: the keyboard can only be released with the mouse.",
        Key::AlwaysGivesTheMouseAndKeyboardBackToThisComputerEvenIfTheServiceIsStuck => "Always gives the mouse and keyboard back to this computer — even if the service is stuck.",
        Key::SharedClipboard => "Shared clipboard",
        Key::On => "On",
        Key::Off => "Off",
        Key::Applying => "Applying…",
        Key::ThisSystemCannotCarryTheClipboard => "This system cannot carry the clipboard",
        Key::TextCanBeCopiedOnOneComputerAndPastedOnTheOther => "Text can be copied on one computer and pasted on the other",
        Key::TheSharedClipboardIsOff => "The shared clipboard is off",
        Key::TheServiceReportedThatTheSharedClipboardIsUnavailable => "The service reported that the shared clipboard is unavailable",
        Key::WhatIsCarried => "What is carried",
        Key::TextOnlyNoFilesNoImages => "Text only. No files, no images.",
        Key::TheClipboardNeverReachesTheDiagnosticsReports => "The clipboard never reaches the diagnostics reports",
        Key::Report => "Report",
        Key::SaveTheDiagnostics2 => "Save the diagnostics",
        Key::NoReportHasBeenSavedYet => "No report has been saved yet",
        Key::TheServiceWritesTheReportWithoutKeysAndWithoutTheClipboard => "The service writes the report without keys and without the clipboard",
        Key::Directory => "Directory",
        Key::TheServiceSStateDirectoryIsUnknown => "the service's state directory is unknown",
        Key::InputService => "Input service",
        Key::StartAgain => "Start again",
        Key::State2 => "State",
        Key::Link => "Link: {}",
        Key::LANAddress => "LAN address",
        Key::Apply => "Apply",
        Key::AfterApplyTheServiceRestartsOnTheNewAddress => "After «Apply» the service restarts on the new address",
        Key::Service => "Service",
        Key::Restart => "Restart",
        Key::Stop => "Stop",
        Key::TheSettingsWereCreatedAutomaticallyConfigToml2 => "The settings were created automatically (config.toml).",
        Key::Understood => "Understood",
        Key::Portals => "Portals",
        Key::ForgetThePortalGrants2 => "Forget the portal grants",
        Key::TheServiceWillAskForTheGrantsAgain => "The service will ask for the grants again",
        Key::KeyboardShortcut => "keyboard shortcut",
        Key::KeyboardShortcutSaved => "keyboard shortcut saved",
        Key::History => "History",
        Key::Refresh => "Refresh",
        Key::ClearHistory => "Clear history",
        Key::Copy => "Copy",
        Key::Sent => "Sent",
        Key::Received => "Received",
        Key::TheLast30TextsInMemoryOnlySecretsAreNotKept => "The last 30 texts. In memory only; secrets are not kept.",
        Key::TheHistoryIsEmptyForNow => "The history is empty for now",
        Key::Transfer => "Transfer",
        Key::DirectionPeerNameAgeSecondsSAgoSizeBytesBytes => "{direction} · {peer_name} · {age_seconds} s ago · {size_bytes} bytes",
        Key::TheClipboardHistory => "the clipboard history",
        Key::CopyFromTheHistory => "copy from the history",
        Key::ClearTheClipboardHistory => "clear the clipboard history",
        Key::TheClipboardHistoryIsRefreshed => "the clipboard history is refreshed",
        Key::TheTextIsCopied => "the text is copied",
        Key::TheClipboardHistoryIsCleared => "the clipboard history is cleared",
        Key::OpenMultiDeskFlow => "Open MultiDeskFlow",
        Key::SendInputTo => "Send input to {}",
        Key::Input2 => "Input → {}",
        Key::StopTheService2 => "Stop the service",
        Key::QuitTheServiceKeepsRunning => "Quit (the service keeps running)",
        Key::SendInputToThisComputer => "Send input to this computer",
        Key::Language => "Language",
        Key::Pairing => "Pairing",
        Key::NextComputer => "Next computer",
        Key::ReturnHere => "Return here",
        Key::ClaimOwnership => "Claim ownership",
        Key::Change => "Change",
        Key::Reset => "Reset",
        Key::PressAShortcutEscCancels => "Press a shortcut. Esc cancels.",
        Key::Unavailable => "Unavailable",
        Key::WaitingForTheServiceStatus => "Waiting for the service status",
        Key::InvalidShortcut => "Invalid shortcut",
        Key::NoAddressIsSelected => "no address is selected",
        Key::PickAnAddressOnTheLocalNetworkInTheSettings => "Pick an address on the local network in the settings.",
        Key::ThisAddressIsTypedOnTheOtherComputerChangeItInTheSettings => "This address is typed on the other computer. Change it in the settings.",
        Key::PairingWindow => "Pairing window",
        Key::LeftS => "] {left} s",
        Key::TellThisAddressToTheOtherComputerSOwnerTheWindowClosesByItself => "Tell this address to the other computer's owner. The window closes by itself.",
        Key::CloseTheWindowSoon => "Close the window · soon",
        Key::ForNowThePairingWindowClosesByItself => "For now the pairing window closes by itself.",
        Key::OpenTheWindowOnOneComputerAndTypeTheAddressOnTheOther => "Open the window on one computer and type the address on the other.",
        Key::TheWindowClosesByItselfIn120Seconds => "The window closes by itself in 120 seconds",
        Key::ConnectByAddress2 => "Connect by address",
        Key::TheAddressFromTheOtherComputerSScreen => "The address from the other computer's screen",
        Key::Connect => "Connect",
        Key::Connecting2 => "Connecting…",
        Key::TheOtherComputerSPairingWindowShowsTheAddress => "The other computer's pairing window shows the address",
        Key::PairingCode => "Pairing code",
        Key::IsConnectingDoTheCodesMatch => " is connecting — do the codes match?",
        Key::YesTheyMatch => "Yes, they match",
        Key::No => "No",
        Key::Collapse => "Collapse",
        Key::Details => "Details",
        Key::THISSHA256 => "THIS · SHA-256 {}",
        Key::TheFingerprintsMatchButTheCodesDoNotDoNotAgree => "The fingerprints match but the codes do not — do not agree.",
        Key::ThePairIsMade => "The pair is made",
        Key::NameIsPairedTheTrustIsSavedNoNeedToRepeatThis => "{name} is paired. The trust is saved — no need to repeat this.",
        Key::Done3 => "Done",
        Key::ConnectingToTheOtherComputer => "Connecting to the other computer…",
        Key::WaitingForTheOtherComputerToConnect => "Waiting for the other computer to connect…",
        Key::TheCodeAppearsAfterTheConnection => "The code appears after the connection",
        Key::SixDigitsAppearOnBothScreensAtOnce => "Six digits appear on both screens at once",
        Key::Appearance2 => "Appearance",
        Key::Theme => "Theme",
        Key::Light => "Light",
        Key::Dark => "Dark",
        Key::LinksBetweenComputers => "Links between computers",
        Key::ASCIIIsoline => "ASCII isoline",
        Key::Line => "Line",
        Key::BackgroundParticles => "Background · particles",
        Key::Gather => "Gather",
        Key::Scatter => "Scatter",
        Key::Still => "Still",
        Key::ParticleSpeed => "Particle speed",
        Key::ASCIIClouds => "ASCII clouds",
        Key::NoClouds => "No clouds",
        Key::EdgeEffects => "Edge effects",
        Key::HideTheCursorWhileInputIsOnAnotherComputer => "Hide the cursor while input is on another computer",
        Key::TheLayoutHasNotArrivedYet => "The layout has not arrived yet",
        Key::K01MONITORMAP => "01 — MONITOR MAP",
        Key::Start => "Start",
        Key::OnceMore => "Once more",
    }
}

/// The `language` a `gui.toml`'s text names, when it names one.
///
/// The window and the tray read the same file, so this is the one place that knows what
/// its line looks like.
pub fn language_in(text: &str) -> Option<Lang> {
    #[derive(Deserialize)]
    struct GuiToml {
        language: Option<Lang>,
    }
    toml::from_str::<GuiToml>(text)
        .ok()
        .and_then(|gui| gui.language)
}

/// Fills the `{}` holes of `key`'s text, in order.
///
/// A catalogue lookup is not a string literal, so `format!` cannot take it: this is the
/// one formatter the window and the tray use for the strings that carry values.
pub fn fill(lang: Lang, key: Key, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(text(lang, key).len() + 16);
    let mut rest = text(lang, key);
    let mut next = args.iter();
    while let Some(at) = rest.find("{}") {
        out.push_str(&rest[..at]);
        match next.next() {
            Some(value) => out.push_str(&value.to_string()),
            // A hole without a value stays as it is: a wrong string must not take the
            // window down.
            None => out.push_str("{}"),
        }
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    out
}

/// Fills the `{name}` holes of `key`'s text; a hole without a value is left alone.
pub fn fill_named(lang: Lang, key: Key, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut out = text(lang, key).to_owned();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), &value.to_string());
    }
    out
}

/// `1 computer`, `2 computers`, `5 computers`: the count with its noun.
pub fn computers(lang: Lang, n: u64) -> String {
    let noun = match lang {
        // Russian keeps three forms; English keeps two.
        Lang::Ru => match n % 100 {
            11..=14 => text(lang, Key::ComputersMany),
            _ => match n % 10 {
                1 => text(lang, Key::Computer),
                2..=4 => text(lang, Key::ComputersFew),
                _ => text(lang, Key::ComputersMany),
            },
        },
        Lang::En => {
            if n == 1 {
                text(lang, Key::Computer)
            } else {
                text(lang, Key::ComputersMany)
            }
        }
    };
    format!("{n} {noun}")
}

/// `1 display`, `2 displays`: the count with its noun.
pub fn displays(lang: Lang, n: u64) -> String {
    let noun = match lang {
        Lang::Ru => match n % 100 {
            11..=14 => text(lang, Key::DisplaysMany),
            _ => match n % 10 {
                1 => text(lang, Key::Display),
                2..=4 => text(lang, Key::DisplaysFew),
                _ => text(lang, Key::DisplaysMany),
            },
        },
        Lang::En => {
            if n == 1 {
                text(lang, Key::Display)
            } else {
                text(lang, Key::DisplaysMany)
            }
        }
    };
    format!("{n} {noun}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key of the catalogue, for the tests that walk it.
    pub const EVERY: [Key; 308] = [
        Key::Pairs,
        Key::Keys,
        Key::Clipboard,
        Key::Diagnostics,
        Key::Settings,
        Key::WhoThisComputerTrusts,
        Key::TheInputServiceSHotkeys,
        Key::CopyHerePasteThere,
        Key::AReportForTroubleshooting,
        Key::AddressAndService,
        Key::Left,
        Key::Right,
        Key::Top,
        Key::Bottom,
        Key::State,
        Key::Layout,
        Key::TakeControl,
        Key::GiveControlBack,
        Key::OpenThePairingWindow,
        Key::ConnectByAddress,
        Key::AnswerThePairing,
        Key::SaveTheLayout,
        Key::TheClipboard,
        Key::TheEdgeEffects,
        Key::RemoveAComputerFromThePair,
        Key::SaveTheDiagnostics,
        Key::ForgetThePortalGrants,
        Key::StartTheInputServiceAgain,
        Key::StopTheService,
        Key::Done,
        Key::Failed,
        Key::TheSettingsWereNotReadTheDefaultsAreShown,
        Key::ComputerNamed,
        Key::TheServiceHasNotAnsweredYet,
        Key::TheWindowIsOpen,
        Key::TheServiceIsRunningEndpoint,
        Key::LostTheConnectionToTheService,
        Key::TheServiceIsStopped,
        Key::TheServiceIsStopping,
        Key::Request,
        Key::TheServiceIsNotRunningNameWasNotSent,
        Key::CouldNotStartTheService,
        Key::CouldNotStartTheService2,
        Key::TheSettingsWereCreatedAutomaticallyConfigToml,
        Key::TheServiceSentAnEmptyFrame,
        Key::PairingOffersACodeToCompare,
        Key::WithoutAName,
        Key::TheServiceSentAnUnexpectedFrame,
        Key::LayoutRevisionFrom,
        Key::LayoutRevisionIsOlderThanTheCurrentThePreviousOneWasKept,
        Key::LayoutReceivedRevisionFromMembers,
        Key::ThePairingWindowIsOpenS,
        Key::ThePairingWindowIsClosed,
        Key::ComputerTitle,
        Key::AnAnswerToAnUnknownRequest,
        Key::Done2,
        Key::DoneText,
        Key::NameIsPaired,
        Key::TheServiceIsNotAnswering,
        Key::TheServiceIsRunning,
        Key::TheServiceIsNotRunning,
        Key::NoDataFromTheService,
        Key::InputHere,
        Key::PreparingTheSwitch,
        Key::TheOtherComputer,
        Key::InputOnPeer,
        Key::InputStoppedBecauseOfAnError,
        Key::InputTheServiceReportedAnUnknownStateOther,
        Key::Layout2,
        Key::Connected,
        Key::Disconnected,
        Key::LinkIsInactiveTheDisplayWasNotFound,
        Key::LinkIsInactiveDisplayWasNotFound,
        Key::TheDisplaysHaveNotArrivedYet,
        Key::LinkTheWholeDesktopAutoScreens,
        Key::NobodyYetOpenThePairingWindow,
        Key::Online,
        Key::Offline,
        Key::OwnsTheKeyboard,
        Key::DoesNotOwnTheKeyboard,
        Key::Unknown,
        Key::NoDataFromTheService2,
        Key::TheOwnerSSession,
        Key::ThisComputerTitle,
        Key::NoOwner,
        Key::TheInputServiceIsStopped,
        Key::NoEmergencyKeyTheKeyboardCanOnlyBeReleasedWithTheMouseHere,
        Key::PhysicalInputHereDoesNotEndTheSessionByItselfOnlyTheSourceCanSwitch,
        Key::TheSharedClipboardIsUnavailable,
        Key::ThisComputerDoesNotAcceptAnAbsolutePointerPosition,
        Key::ThisComputerCannotCaptureInput,
        Key::ThisComputerCannotInjectEvents,
        Key::LimitationOther,
        Key::ThePortalGrantIsBeingRestoredFromTheSavedTokenNoDialogIsExpected,
        Key::APortalPermissionDialogIsExpectedConfirmItOrThisNodeCannotTakeInput,
        Key::AccessWasDeniedInThePermissionDialogPressForgetThePortalGrantsInTheSettingsAndTryAgain,
        Key::DesktopPortalsAreUnavailableOnThisSystem,
        Key::WaylandInputCanOnlyBeCapturedThroughDesktopPortals,
        Key::LastMessage,
        Key::DelayMs,
        Key::CouldNotOpenTheSettingsFile,
        Key::NoAddressIsSelectedPickOneInTheListAndPressApplyAgain,
        Key::TheSettingsDirectoriesAreUnknownRestartTheWindowWithoutSpecialFlags,
        Key::ThisComputerSAddressAddress,
        Key::TheAddressIsSaved,
        Key::TheAddressWasNotSaved,
        Key::TheServiceWasNotStartedByThisWindowStopItYourself,
        Key::TheServiceSProcessIdIsUnknownStopItYourself,
        Key::TheServiceIsRestartingPidPid,
        Key::TheServiceIsStoppingPidPid,
        Key::CouldNotStopTheServiceWhy,
        Key::CouldNotStopTheServiceWhy2,
        Key::TheSettingsFileWasNotFound,
        Key::CouldNotStartTalkingToTheServiceError,
        Key::TheServiceIsAlreadyRunningAndAnswering,
        Key::ProgramDidNotStopProcessPidCode,
        Key::Unknown2,
        Key::ProgramDidNotStartError,
        Key::CouldNotWorkOutTheDirectoriesWhy,
        Key::CouldNotWorkOutTheDirectories,
        Key::NoCode,
        Key::NoEmergencyKey,
        Key::TheSessionCannotBeEndedWithPhysicalInput,
        Key::TheSharedClipboardIsUnavailable2,
        Key::NoAbsolutePointer,
        Key::InputCannotBeCaptured,
        Key::EventsCannotBeInjected,
        Key::TheLayoutWasChangedOnAnotherComputer,
        Key::TheLayoutDocumentIsInvalid,
        Key::ThereIsNoCoordinatorOwner,
        Key::TheCoordinatorDidNotConfirmTheEdit,
        Key::ThatEditIsGone,
        Key::ThisComputerIsNotPaired,
        Key::CouldNotWriteThePeerList,
        Key::TheClipboardDidNotTurnOn,
        Key::CouldNotWriteTheDiagnosticsFile,
        Key::ThePairingCouldNotBeAgreed,
        Key::ThePairingWindowIsAlreadyClosed,
        Key::ThePairingWindowIsAlreadyOpen,
        Key::ThePairingWindowIsNotOpen,
        Key::TheAddressIsWrong,
        Key::ThisPairingIsNoLongerWaitingForAnAnswer,
        Key::TakingControlDidNotWork,
        Key::TheSwitchDidNotWork,
        Key::ThePortalsDidNotAnswer,
        Key::TheInputAgentIsGone,
        Key::TheCoreDidNotUnderstandTheRequest,
        Key::CodeOther,
        Key::ThisComputerHasControl,
        Key::ControlWasGivenBack,
        Key::Connecting,
        Key::TheAnswerWasSent,
        Key::TheLayoutIsSaved,
        Key::TheClipboardWasSwitched,
        Key::TheEdgeEffectsWereSwitched,
        Key::TheComputerWasRemovedFromThePair,
        Key::ThePortalGrantsWereForgotten,
        Key::TheInputServiceIsStartingAgain,
        Key::TheServiceIsStopping2,
        Key::TheDiagnosticsAreSaved,
        Key::ThePairingWindowIsOpen,
        Key::TheCoreAnswered,
        Key::TheLayoutWasChangedOnAnotherComputerDragItAgain,
        Key::TheCodesDidNotMatchOrThePartnerRefused,
        Key::PutTheComputerNextToANeighbourWithoutOverlaps,
        Key::Saved,
        Key::Computer,
        Key::ComputersFew,
        Key::ComputersMany,
        Key::Display,
        Key::DisplaysFew,
        Key::DisplaysMany,
        Key::Input,
        Key::ThisComputer,
        Key::ThisComputerBackend,
        Key::InputHere2,
        Key::Offline2,
        Key::LightTheme,
        Key::DarkTheme,
        Key::Appearance,
        Key::OpenThePairingWindow2,
        Key::OpenTheSettingsFile,
        Key::Remove,
        Key::RemoveFromThePairYouWillHaveToPairAgain,
        Key::YesRemove,
        Key::Cancel,
        Key::Empty,
        Key::NobodyYetOpenThePairingWindow2,
        Key::Emergency,
        Key::TheEmergencyKeyIsUnavailableOnThisSystemTheKeyboardCanOnlyBeReleasedWithTheMouse,
        Key::AlwaysGivesTheMouseAndKeyboardBackToThisComputerEvenIfTheServiceIsStuck,
        Key::SharedClipboard,
        Key::On,
        Key::Off,
        Key::Applying,
        Key::ThisSystemCannotCarryTheClipboard,
        Key::TextCanBeCopiedOnOneComputerAndPastedOnTheOther,
        Key::TheSharedClipboardIsOff,
        Key::TheServiceReportedThatTheSharedClipboardIsUnavailable,
        Key::WhatIsCarried,
        Key::TextOnlyNoFilesNoImages,
        Key::TheClipboardNeverReachesTheDiagnosticsReports,
        Key::Report,
        Key::SaveTheDiagnostics2,
        Key::NoReportHasBeenSavedYet,
        Key::TheServiceWritesTheReportWithoutKeysAndWithoutTheClipboard,
        Key::Directory,
        Key::TheServiceSStateDirectoryIsUnknown,
        Key::InputService,
        Key::StartAgain,
        Key::State2,
        Key::Link,
        Key::LANAddress,
        Key::Apply,
        Key::AfterApplyTheServiceRestartsOnTheNewAddress,
        Key::Service,
        Key::Restart,
        Key::Stop,
        Key::TheSettingsWereCreatedAutomaticallyConfigToml2,
        Key::Understood,
        Key::Portals,
        Key::ForgetThePortalGrants2,
        Key::TheServiceWillAskForTheGrantsAgain,
        Key::KeyboardShortcut,
        Key::KeyboardShortcutSaved,
        Key::History,
        Key::Refresh,
        Key::ClearHistory,
        Key::Copy,
        Key::Sent,
        Key::Received,
        Key::TheLast30TextsInMemoryOnlySecretsAreNotKept,
        Key::TheHistoryIsEmptyForNow,
        Key::Transfer,
        Key::DirectionPeerNameAgeSecondsSAgoSizeBytesBytes,
        Key::TheClipboardHistory,
        Key::CopyFromTheHistory,
        Key::ClearTheClipboardHistory,
        Key::TheClipboardHistoryIsRefreshed,
        Key::TheTextIsCopied,
        Key::TheClipboardHistoryIsCleared,
        Key::OpenMultiDeskFlow,
        Key::SendInputTo,
        Key::Input2,
        Key::StopTheService2,
        Key::QuitTheServiceKeepsRunning,
        Key::SendInputToThisComputer,
        Key::Language,
        Key::Pairing,
        Key::NextComputer,
        Key::ReturnHere,
        Key::ClaimOwnership,
        Key::Change,
        Key::Reset,
        Key::PressAShortcutEscCancels,
        Key::Unavailable,
        Key::WaitingForTheServiceStatus,
        Key::InvalidShortcut,
        Key::NoAddressIsSelected,
        Key::PickAnAddressOnTheLocalNetworkInTheSettings,
        Key::ThisAddressIsTypedOnTheOtherComputerChangeItInTheSettings,
        Key::PairingWindow,
        Key::LeftS,
        Key::TellThisAddressToTheOtherComputerSOwnerTheWindowClosesByItself,
        Key::CloseTheWindowSoon,
        Key::ForNowThePairingWindowClosesByItself,
        Key::OpenTheWindowOnOneComputerAndTypeTheAddressOnTheOther,
        Key::TheWindowClosesByItselfIn120Seconds,
        Key::ConnectByAddress2,
        Key::TheAddressFromTheOtherComputerSScreen,
        Key::Connect,
        Key::Connecting2,
        Key::TheOtherComputerSPairingWindowShowsTheAddress,
        Key::PairingCode,
        Key::IsConnectingDoTheCodesMatch,
        Key::YesTheyMatch,
        Key::No,
        Key::Collapse,
        Key::Details,
        Key::THISSHA256,
        Key::TheFingerprintsMatchButTheCodesDoNotDoNotAgree,
        Key::ThePairIsMade,
        Key::NameIsPairedTheTrustIsSavedNoNeedToRepeatThis,
        Key::Done3,
        Key::ConnectingToTheOtherComputer,
        Key::WaitingForTheOtherComputerToConnect,
        Key::TheCodeAppearsAfterTheConnection,
        Key::SixDigitsAppearOnBothScreensAtOnce,
        Key::Appearance2,
        Key::Theme,
        Key::Light,
        Key::Dark,
        Key::LinksBetweenComputers,
        Key::ASCIIIsoline,
        Key::Line,
        Key::BackgroundParticles,
        Key::Gather,
        Key::Scatter,
        Key::Still,
        Key::ParticleSpeed,
        Key::ASCIIClouds,
        Key::NoClouds,
        Key::EdgeEffects,
        Key::HideTheCursorWhileInputIsOnAnotherComputer,
        Key::TheLayoutHasNotArrivedYet,
        Key::K01MONITORMAP,
        Key::Start,
        Key::OnceMore,
    ];

    #[test]
    fn no_translation_is_empty_and_the_english_one_has_no_cyrillic() {
        for key in EVERY {
            for lang in [Lang::Ru, Lang::En] {
                let line = text(lang, key);
                assert!(!line.trim().is_empty(), "{key:?} in {lang:?}");
            }
            assert!(
                !text(Lang::En, key)
                    .chars()
                    .any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)),
                "{key:?} is not translated"
            );
        }
    }

    #[test]
    fn the_plural_helpers_count_in_both_languages() {
        assert_eq!(computers(Lang::Ru, 1), "1 компьютер");
        assert_eq!(computers(Lang::Ru, 3), "3 компьютера");
        assert_eq!(computers(Lang::Ru, 11), "11 компьютеров");
        assert_eq!(computers(Lang::En, 1), "1 computer");
        assert_eq!(computers(Lang::En, 3), "3 computers");
        assert_eq!(displays(Lang::Ru, 2), "2 экрана");
        assert_eq!(displays(Lang::En, 2), "2 displays");
    }

    #[test]
    fn the_placeholders_of_both_languages_are_the_same() {
        fn holes(text: &str) -> Vec<&str> {
            text.split('{')
                .skip(1)
                .filter_map(|part| part.split('}').next())
                .collect()
        }
        for key in EVERY {
            assert_eq!(
                holes(text(Lang::Ru, key)),
                holes(text(Lang::En, key)),
                "{key:?}"
            );
        }
    }
}
