# nrxp-servo — план доработки недостающего функционала

> Рабочий файл форка Servo под браузер [netrunner-browser](../../../netrunner-browser) (`nrxp-browser`).
> **Сырой рабочий документ, не проходил ревью.** Факты про код Servo отмечены ссылками на файлы
> (проверены чтением исходников на 2026-10-09); всё остальное — оценки и допущения, они помечены «?».

## 0. Зачем форк и по каким правилам

Браузер сегодня встраивает **крейт `servo` 0.7.0 с crates.io** и уже вынужден патчить `servo-net` (WebSocket мимо прокси).
Часть недостающего (загрузки, поиск по странице, печать, MSE, HTTP/3, герметичная сеть) нельзя сделать снаружи — нужны правки
внутри движка. `nrxp-servo` — место для этих правок.

**Правила (чтобы форк не стал неподъёмным):**
1. **База — тег `v0.7.0`** (ветка `nrxp/0.7`), а не `main`: наш браузер собран на 0.7.0, версии крейтов совпадают, поэтому
   потребляется через `[patch.crates-io]` по путям (`servo-net`, `servo-script`, `servo`, …) — **только изменённые крейты**.
   `main` (`upstream/main`) — для чтения и для PR в апстрим; в `main` уже 151 коммит после 0.7.0.
2. **Каждая правка — маленькая, за настройкой (pref) и с выключенным по умолчанию поведением**, пока не доказана безопасность;
   публичный API `servo` только **добавляем** (новые методы/делегаты с пустой реализацией по умолчанию), ничего не ломаем.
3. **Сначала апстрим-совместимость:** то, что полезно всем (исправления N1, API загрузок, поиск по странице), оформляем так,
   чтобы можно было отправить PR в `servo/servo`; NRXP-специфичное (N2) — через общий интерфейс «embedder-provided connector»,
   а не «NRXP в ядре».
4. **Никаких долгих расхождений:** серия патчей хранится как `git format-patch` в `patches/` браузера (как сейчас
   `patches/servo/`) и как ветка в этом репозитории; ребейз на новый тег — раз в релиз Servo (N14).
5. **Приёмка каждой итерации — исполняемая:** тест встраивания (`components/servo/tests`), WPT-подмножество, либо сценарий
   браузера (`scripts/gui-*.sh`, корпус). Без доказательства итерация не закрыта.
6. **Лицензия:** MPL-2.0 (как апстрим); наши файлы — тем же.
7. **Честность:** если не вышло или ограничение осталось — пишем в «Итог» итерации, а не молчим (как в `netrunner-browser/docs/ITERATIONS.md`).

**Как вставлять этапы:** промежуточный этап получает номер `Nk.m` (например `N3.1`), добавляется строка в таблицу статусов (§3)
и раздел ниже; существующие номера не сдвигаем.

## 1. Что именно недостаёт (факты по исходникам v0.7.0)

Проверено чтением кода (`git grep` по тегу/ветке). «Обход в браузере» — что мы уже умеем **без** правок Servo.

| Возможность | Состояние в Servo 0.7.0 (где смотреть) | Обход в браузере сейчас |
|---|---|---|
| Сеть: подключение | `components/net/connector.rs`: `ServoHttpConnector` (hyper `HttpConnector`, прямой TCP+DNS) и `ProxyConnector` (только HTTP-CONNECT по prefs `network_http(s)_proxy_uri`); `websocket_loader.rs:332` — свой `TcpStream::connect` в обход прокси | локальный CONNECT-прокси + патч `servo-net` для WebSocket; герметичный netns (`--hermetic`) |
| Сеть: HTTP/3, QUIC | нет вообще (в `Cargo.toml` нет quinn/h3) | нет |
| Загрузки | нет: ни обработки `Content-Disposition`, ни атрибута `download`, ни события для встраивателя. Ответ навигации идёт в `ParserContext::process_response` (`components/script/dom/servoparser/mod.rs:1339`) и рисуется/показывается по MIME | эвристика по URL + свой клиент (план N3a) |
| Куки для встраивателя | **есть**: `SiteDataManager::cookies_for_url`, `set_cookie_for_url`, `clear_cookies` (`components/servo/site_data_manager.rs:234–305`) | — (поправка: раньше считал, что нет) |
| Поиск по странице | нет (`git grep find_in_page` пусто) | JS через `WebView::evaluate_javascript` (N5a) |
| Печать / PDF | нет API (`print` только в layout-коде отрисовки) | нет |
| Медиа | `components/media/backends/{gstreamer,dummy,auto,ohos}`; feature `media-gstreamer` требует системный GStreamer; **MSE (`MediaSource`/`SourceBuffer`) в DOM отсутствует** | нет |
| Service Workers | код есть (`components/script/dom/serviceworker*`), настройка `dom_serviceworker_enabled` выключена | не включаем |
| WebRTC | `components/media/webrtc`, настройка `dom_webrtc_enabled` выключена; сокеты — внутри `webrtc`, встраиватель их не контролирует | выключено |
| IntersectionObserver, adoptedStyleSheets, события композиции | настройки есть, **выключены по умолчанию** (`components/config/prefs.rs`: `dom_intersection_observer_enabled` и др.) | включаем в `build_preferences` (нужно react.dev, svelte.dev, GitHub) |
| PerformanceObserver | **поправка:** реализован по спецификации — `observe()` на неподдерживаемый тип только пишет предупреждение «No valid entry type» (`performanceobserver.rs:159`), `supportedEntryTypes` есть; это не дефект. Не хватает отдельных типов записей (`longtask`, `layout-shift`, `event`, `first-input`) — добавлять только если какой-то сайт на них ломается | предупреждение в консоли, на работу не влияет |
| Остановка процесса | **падение при завершении**: `components/script/dom/webgl/webglbuffer.rs:85` и `webglprogram.rs:269` — `assert!(self.is_deleted())` в `delete()`, вызываемом при разборе контекста WebGL (на GitHub) → abort/SIGSEGV после готового результата | выход через `_exit`, `ManuallyDrop` |
| Хранилище без профиля | `components/storage/client_storage.rs:697` — `let _ = temp_dir;` **не удерживает** временный каталог; тот удаляется сразу, хранилище откатывается на базу в памяти с относительными путями (`./bottles/…`) | свой временный каталог |
| Диалоги/элементы управления | есть `EmbedderControl`: `SelectElement`, `ColorPicker`, `FilePicker`, `InputMethod`, `ContextMenu` (`components/shared/embedder/embedder_controls.rs`); есть `request_permission`, `request_authentication`, `show_notification`, `show_console_message`, `request_protocol_handler`, `notify_accessibility_tree_update` | реализованы в оболочке |
| Перехват запросов | есть `WebViewDelegate::load_web_resource` / `ServoDelegate::load_web_resource` (подмена ответа или отмена; тела запроса и заголовков ответа **нет**) | блокировщик рекламы |
| Менеджер паролей | нет событий «форма отправлена / поле логина найдено» | нет |
| Песочница, мультипроцесс | feature `multiprocess` есть у `servo` (в браузере выключена) | — |
| WebExtensions | нет (есть `UserContentManager`: скрипты и стили) | userscripts |

Шаблон для нового API встраивателя — `EmbedderControl*` (запрос → ответ по id) и `WebResourceLoad`: так проще всего получить «настоящий»
интерфейс, который примут в апстрим.

## 2. Итерации

Обозначения: **Размер** S (дни) / M (1–3 недели) / L (месяц+) — грубая оценка для одного человека (?).
**Без форка** — что можно сделать в браузере до/вместо правки движка.

### N0 — Бутстрап форка и сборка по путям
Размер: S | Зависит от: — | Статус: **done по сборке браузера (2026-10-09)**; тесты встраивания форка — в процессе
**Цель** — браузер собирается на локальном форке без единой правки (доказательство, что схема потребления работает).
**Задачи**
- [x] Клон `servo/servo` (частичный, `--filter=blob:none`) в `~/netrunner/nrxp-servo`; remote `upstream`; ветка `nrxp/0.7` от тега `v0.7.0`
- [x] Подключение форка в браузере: `netrunner-browser/scripts/setup-upstream.sh` → `scripts/gen-servo-patch.py` генерирует `.cargo/config.toml` с `[patch.crates-io]` **для всех 55 крейтов `servo*`** (путь `NRXP_SERVO_DIR`, по умолчанию `../nrxp-servo`); без форка — легаси-режим (только `servo-net` с патчем). Секция `[patch]` из `Cargo.toml` браузера убрана
- [x] **Выяснено:** подменять нужно **все** `servo*` сразу (иначе path- и registry-версии одного крейта дают два набора несовместимых типов); `Cargo.lock` при этом меняется только удалением `source`/`checksum` у 55 подменённых пакетов (110 строк), **версии сторонних крейтов не двигаются**
- [x] Тулчейн: форк закреплён на 1.97.1 (`rust-toolchain.toml`), но как зависимость собирается браузерным `stable` (rustc 1.97.1 на этой машине) — предупреждения о невыполненных `#[expect]` есть, ошибок нет
- [x] Патч `0001-websocket-via-proxy` перенесён в ветку форка отдельным коммитом (`a3ac40eda8`); легаси-файл `patches/servo/` остаётся для режима без форка
- [x] Проверено: `cargo build -p nrb --features servo` на форке — 3 мин (debug, чистый `target-fork`); `scripts/gui-smoke.sh` зелёный; корпус на бинаре из форка 43/52 (82,7%) против 44/52 на крейтах crates.io (в пределах шума ±4%); WebSocket через `--hermetic` работает (патч действует)
- [x] Тесты встраивания форка: **101/101** проходят под `cargo nextest run -p servo` (процесс на тест; обычный `cargo test` падает на «Already initialized: Opts» — глобальные настройки Servo одноразовые); с правками N1/N2 включительно. Запуск: `scripts/lowload.sh cargo nextest run -p servo --test-threads 2` из репозитория браузера (≈45 с после сборки)
- [ ] Скрипт `nrxp-servo-sync.sh` (`git format-patch v0.7.0..nrxp/0.7` → `patches/servo/`) — отложен: после N2 легаси-режим станет не нужен
**Приёмка** — достигнута: `git diff v0.7.0..nrxp/0.7` в `components/` = только перенесённый патч WebSocket; браузер собирается и проходит смоук/корпус на коде форка.
**Риски** — несовпадение транзитивных версий (не проявилось); долгая первая сборка (~10 ГБ, `target-fork`).

### N1 — Срочные исправления малого размера (апстрим-кандидаты)
Размер: S | Зависит от: N0 | Без форка: есть обходы, но они лечат симптомы
**Цель** — убрать обходы в браузере и дать PR апстриму.
**Задачи**
- [ ] **WebGL при остановке:** `webglbuffer.rs:85`, `webglprogram.rs:269` — `delete()` не должен паниковать, если объект ещё не помечен на удаление (как уже сделано в `webglframebuffer.rs:127`: `if !self.is_deleted() { set_deleted… }`). Найти вызывающих при разборе контекста (`webglrenderingcontext.rs` Drop/очистка), добавить регрессионный тест (создать буфер, закрыть вкладку/движок — без паники). Проверить `main`: исправлено ли там
- [ ] **Хранилище без профиля:** `client_storage.rs:697` — реально удерживать `TempDir` (`let _keep = temp_dir;`), а откат на память делать с абсолютным/временным `base_dir`, не `PathBuf::new()`
- [ ] **Включить по умолчанию** (в `prefs.rs`) `dom_intersection_observer_enabled`, `dom_adoptedstylesheet_enabled`, `dom_composition_event_enabled` — отдельным обсуждаемым PR (метрики: react.dev, svelte.dev, GitHub падали без них); либо оставить за настройкой в браузере
- [~] **PerformanceObserver:** пункт снят — предупреждение в консоли соответствует спецификации (см. §1); новые типы записей — только по реальной поломке сайта
**Приёмка** — в браузере удаляются `_exit`/`ManuallyDrop`-обход и `ephemeral_dir()`; release-корпус не хуже 88,5% (46/52); `nrb snap` на GitHub завершается кодом 0 без обхода.
**Итог** — _todo_

### N2 — Сеть через встраивателя («embedder-provided connector»)
Размер: M–L | Зависит от: N0 | Статус: **done (2026-10-09)** | Без форка: локальный CONNECT-прокси (был)
**Цель** — Servo открывает TCP-потоки **через интерфейс встраивателя**, а не напрямую: NRXP-транспорт без loopback-прокси, без HTTP-CONNECT и без патча WebSocket.
**Почему это главное для проекта** — убирает класс утечек (любой прямой сокет в `net`), упрощает герметичный режим, снимает накладные расходы CONNECT и пароль на loopback, даёт одну точку для kill-switch и учёта.
**Дизайн (набросок, ?):**
```rust
// servo (публичный API) — новый трейт, по умолчанию None = поведение как сейчас
pub trait NetworkConnector: Send + Sync + 'static {
    /// host без DNS-разрешения (имя уходит выходной ноде), port
    fn connect_tcp(&self, host: &str, port: u16)
        -> Pin<Box<dyn Future<Output = io::Result<BoxedIo>> + Send>>;
    // позже (N9/N10): fn bind_udp(&self, …)
}
ServoBuilder::network_connector(Arc<dyn NetworkConnector>)
```
- `components/net/connector.rs`: если коннектор задан — `ProxyConnector`/`ServoHttpConnector` заменяются адаптером (`Service<Destination>` → `connect_tcp`); TLS остаётся rustls поверх выданного потока (`hyper_rustls` оборачивает коннектор, как сейчас)
- `components/net/websocket_loader.rs:332`: вместо `TcpStream::connect` — тот же коннектор (заменяет наш патч `0001-websocket-via-proxy`)
- остальные прямые сокеты в `components/` (поиск `TcpStream::connect`, `UdpSocket`, `lookup_host`, `ToSocketAddrs`) — перечислить, закрыть или отдать коннектору; devtools-listener (локальный) остаётся
- DNS: при коннекторе локально **не резолвим**; DoH/DoT — вне объёма
- конфигурация доходит в `net` через `ResourceThreads`/`CoreResourceThread` (проверить путь: коннектор не сериализуется через IPC — в однопроцессном режиме передаём `Arc` напрямую; при `multiprocess` — см. N12)
**Задачи**
- [x] Часть спайка T1: в `components/` (кроме тестов) прямой `TcpStream::connect` — **один**, `net/websocket_loader.rs:332`; `UdpSocket`/`lookup_host` в коде компонентов нет (UDP — внутри крейта `webrtc`, см. N9; DNS делает hyper `HttpConnector` внутри `ServoHttpConnector`). Остаётся проверить `devtools`/`webdriver_server` (локальные слушатели) и сторонние крейты
- [ ] Спайк (S): прочитать путь создания `ServoClient` → `create_http_client` в `resource_thread.rs`, где можно передать `Arc` (не `Serialize`)
- [ ] Реализация трейта и адаптера; тест встраивания: коннектор-заглушка считает соединения, страница на локальном сервере грузится **только** через него, любой обход = провал теста
- [ ] WebSocket и EventSource/fetch/image/script — все через коннектор (тест по каждому типу ресурса)
- [ ] В браузере: `NrxpTransport` реализует трейт; удалить `LocalProxy` из пути движка, патч WebSocket, `--hermetic` релей (остаётся как второй слой)
**Приёмка** — `scripts/leakwatch.sh` и `hermetic-e2e.sh` зелёные **без** локального прокси; корпус не хуже; тест «прямой сокет из Servo невозможен» (в netns без маршрутов работает вся страница, потому что весь трафик идёт через коннектор, а коннектор в другом ns/потоке).
**Риски** — `hyper`/`hyper-util` версии; HTTP/2 мультиплексирование поверх нашего потока; IPC-границы при мультипроцессе; поведение на обрыве (kill-switch).
**Итог** — _todo_

**Итог (2026-10-09).** Реализовано проще наброска: **один process-wide коннектор** (как prefs), без протаскивания через `ResourceThreads`
(менеджер ресурсов Servo живёт в процессе встраивателя — `Arc` не пересекает IPC; для `multiprocess` вопрос остаётся в N12).
* `net::connector`: трейты `NetworkConnector { fn connect(&self, host, port) -> ConnectFuture }` и `NetworkStream` (любой tokio `AsyncRead+AsyncWrite+Unpin+Send`),
  `set_network_connector`/`network_connector`, `ServoStream` (`TokioIo<Box<dyn NetworkStream>>`, `Connected::new()`) — новый `Response` для `ProxyConnector`;
  при установленном коннекторе `ProxyConnector::call` берёт host/port из `Destination` (скобки IPv6 снимаются, порт по схеме), прямые и CONNECT-пути остаются без изменений
* `websocket_loader.rs`: `connect_tcp` сначала спрашивает коннектор; тип потока `WebSocketStream<ClientStream<Box<dyn NetworkStream>>>`
* `servo::ServoBuilder::network_connector(...)` + реэкспорт `servo::{NetworkConnector, NetworkStream, ConnectFuture}`
* Тест встраивания `components/servo/tests/network_connector.rs`: документ `http://page.nrxp-test.invalid:8080/` и скрипт с `sub.nrxp-test.invalid:8081` загружаются **только** через
  коннектор, который отдаёт им локальный сервер; проверяется, что имена пришли нерезолвленными с портами из URL
* Тесты: полный `nextest` по `-p servo` — 101/101 (включая тест коннектора), прогон под лимитами ресурсов
* В браузере: `TransportConnector` → `NrxpTransport`; проверено на 12 сайтах, WebSocket, герметичном режиме, GUI-сценариях; сокетов самого движка нет (leakwatch по pid: 0)
**Не сделано (остаток):** `UdpSocket`/WebRTC/QUIC через тот же интерфейс (N9/N10); DNS-over-коннектор для не-HTTP путей (их в `components/` нет — проверено `git grep`); поведение при `multiprocess` (N12);
DevTools/webdriver-слушатели остаются локальными и вне объёма; проверить `components/net/tests` (тесты `net` собирают клиент без коннектора — поведение не менялось).

### N3 — Загрузки файлов
Размер: M | Зависит от: N0 (N3a — нет) | Без форка: частично (N3a)
**Цель** — `Content-Disposition: attachment`, `<a download>`, «неотображаемые» MIME-типы → загрузка через встраивателя с куками/авторизацией, прогрессом и отменой.
**N3a — без форка (быстро, ограниченно)**: в `load_web_resource` для главного документа по расширению/`download` отменяем навигацию и качаем собственным клиентом `nrb-net` **с куками** из `SiteDataManager::cookies_for_url`. Ограничения: только GET, нет `Content-Disposition`-имён и динамических эндпоинтов, нет `POST`-форм, Referer/UA вручную. Нужно как временное решение пользователю.
**N3b — в движке (правильно):**
- `ParserContext::process_response` (`servoparser/mod.rs:1339`) решает судьбу ответа по MIME. Добавить ветку: если `Content-Disposition: attachment` или тип не отображаем (и это навигация верхнего уровня, не подресурс) → **не создавать документ**, а отправить встраивателю `WebViewDelegate::request_download(DownloadRequest)` и передать **поток тела** (канал IPC `Bytes`) без повторного запроса
- `DownloadRequest { url, final_url, suggested_filename, mime, content_length, headers (подмножество), is_user_initiated }`; ответ встраивателя: `Allow(DownloadSink)` | `Deny`; sink получает чанки, `progress`, `finish/cancel`
- `<a download>` и `Blob`/`data:` — через `HTMLAnchorElement` (проверить, как обрабатывается атрибут), `blob:` — читать из blob-хранилища
- отмена навигации не должна оставлять вкладку в «загружается» и не должна затирать текущую страницу (поведение «204 No Content»)
- политика: песочница имени файла на стороне встраивателя (не доверять `filename`), лимиты, ask-always
- тест встраивания: локальный сервер отдаёт `attachment`, делегат принимает, байты совпадают; отказ → вкладка цела; перенаправление на attachment; `download`-атрибут
**Приёмка** — в браузере окно «Загрузки» (Ctrl+J): скачивание с авторизацией (кука) и с динамического эндпоинта; `scripts/gui-downloads.sh`.
**Итог (2026-10-10, форк готов, 5/5 + 4/4 тестов; браузер ещё не использует):** ветка `nrxp/0.7`. Решение T3: перехват не в `ParserContext`, а раньше — в `fetch::methods::main_fetch` перед `process_response` (там уже известны заголовки после редиректов, а тело ещё не ушло в скрипт). Условие: навигация (`RequestMode::Navigate`) с webview, статус 2xx кроме 204/205, `Content-Disposition: attachment` или `application/*` не из списка отображаемых (`components/net/download.rs`). Встраивателю уходит `NetToEmbedderMsg::RequestDownload` → `WebViewDelegate::request_download(DownloadRequest)`; `accept(buffer)` даёт `DownloadStream` (ограниченный канал → обратное давление; drop = отмена), `decline()`/drop — движок ведёт себя как раньше (по умолчанию). Принятая загрузка завершает навигацию как 204 (скрипт шлёт `AbortLoadUrl`): страница остаётся, вкладка не «висит». Типы лежат в крейте `net` (не в `embedder_traits`) — так не пересобирается весь Servo. Не сделано: `<a download>`, `blob:`/`data:`-загрузки, прогресс по `Content-Length` во время редиректов, подресурсы/iframe без webview. Тесты: `components/servo/tests/download.rs` (5), юнит-тесты разбора заголовков в `net::download`.

### N4 — Данные сайтов и разрешения для встраивателя
Размер: S–M | Зависит от: N0 | Без форка: куки есть; остальное нет
**Цель** — управление данными сайтов в UI и постоянные разрешения.
**Задачи**
- [ ] Проверить, чего не хватает у `SiteDataManager` (список куки по домену с флагами, экспорт/импорт, `localStorage` по origin — читать/чистить)
- [ ] `request_permission` → постоянное хранение решения (на стороне оболочки; при необходимости в движке — «запомнить» как часть `PermissionRequest`)
- [ ] Режим «блокировать сторонние куки» (политика в `net/cookie.rs`) — настройка + тест
**Приёмка** — окно «Данные сайтов» в браузере; сторонние куки блокируются на тестовой странице.
**Итог (2026-10-10, частично):** сделан режим сторонних куки — `Preferences::network_block_third_party_cookies` (`net/http_loader.rs::cookies_blocked_as_third_party`: инициатор запроса не same-site с URL → куки не отправляются и не сохраняются; `Destination::Document` не затрагивается; судит по ближайшему документу), тесты `components/servo/tests/third_party_cookies.rs` (2). Не сделано: `document.cookie`/CookieStore из сторонних фреймов (идёт через ресурсный поток отдельным путём), верхний сайт вместо ближайшего документа (нужен top-level origin в запросе), постоянные разрешения (`request_permission` пока решает оболочка без памяти).

### N5 — Поиск по странице
Размер: S (N5a) + M (N5b) | Зависит от: N0 (N5a — нет)
**N5a — без форка:** `WebView::evaluate_javascript` + инжектируемый скрипт (TreeWalker по текстовым узлам, подсветка `<mark>`/CSS, счётчик, переход, очистка). Ограничения: меняет DOM, не видит shadow DOM/iframe из другого origin, ломает сложные страницы.
**N5b — в движке:** `WebView::find(text, FindOptions{case, backwards, whole_word})`, `find_next/prev`, `stop_finding`; делегат `notify_find_result(count, active)`. Реализация: обход текстовых фрагментов дерева layout (`components/layout`), подсветка активного/остальных совпадений через отрисовку выделения (тот же путь, что `::selection`), прокрутка к активному. Тест встраивания + `scripts/gui-find.sh`.
**Приёмка** — Ctrl+F в окне: количество совпадений, переход, подсветка; без изменения DOM (N5b).
**Итог N5b (2026-10-10, форк):** `WebView::find(FindRequest{action: Search|Refresh|Next|Previous|Clear, query, case_sensitive}, callback)` → JSON `{"count":N,"active":I|null}`. Путь: embedder `Find` → constellation → `ScriptThreadMessage::Find` → `script/dom/find.rs` (поиск по тексту DOM с свёрнутыми пробелами, совпадения через инлайн-элементы, не через блоки; текст `script/style/template/head/title/textarea` и узлы без текстового прогона пропускаются; лимит 5000) → ответ тем же путём, что `evaluate_javascript`. Подсветка рисуется layout'ом (`FindHighlight` в `TextRun`, `build_display_list_for_find_highlights`: жёлтый — остальные, оранжевый — активное) — **DOM, выделение и MutationObserver не затрагиваются**. Активное совпадение прокручивается в центр (по родительскому элементу). Тесты: `components/servo/tests/find.rs` (4: счёт/шаги/регистр/пробелы/инлайн, прокрутка и Refresh, DOM не менялся — 0 записей MutationObserver, outerHTML тот же, выделения нет, подсветка рисуется и снимается по пикселям); весь пакет 112/112. Ограничения: только главный документ (без iframe), без shadow DOM, подсветка пропадает при перестройке текстового прогона (встраиватель шлёт `Refresh`), прокрутка по элементу, а не по самому совпадению, без «целых слов».

### N6 — Печать и PDF
Размер: L | Зависит от: N0 | Без форка: нет
**Цель** — Ctrl+P / «Сохранить как PDF».
**Спайк (S) прежде всего:** есть ли в `layout` страничная вёрстка (`@page`, разрывы страниц, `@media print`); `paint`/webrender умеют ли выводить в вектор. Варианты: (1) paged-layout + PDF-бэкенд (новый крупный кусок); (2) растровая «печать» полос страницы (дёшево, плохое качество); (3) отдавать встраивателю «display list» для своего PDF. Решение — по итогам спайка; без решения этап не стартует.
**Приёмка** — страница сохраняется в многостраничный PDF с выделяемым текстом (вариант 1/3).

**Итог спайка T4 (2026-10-10, чтением исходников v0.7.0).** Страничной вёрстки **нет**: в `components/layout` нет `@page`/фрагментации по страницам/`break-before|after` (совпадения только про переносы строк в inline-раскладке); в `script` `@page` не встречается; PDF-бэкенда в `paint` нет (webrender растеризует в кадр). Вывод: варианты (1) paged-layout + PDF и (3) display list наружу — **крупная работа в layout (месяцы)**, не этап «L», а отдельный проект. Реалистично сейчас только вариант (2) — **растровая печать**: страница рендерится полосами высотой в лист (прокрутка скриптом + `WebView::take_screenshot` по прямоугольнику, ширина — под ширину листа с масштабом), полосы собираются в PDF картинками (`pdf-writer`). Ограничения: текст не выделяется и не ищется, фиксированные шапки повторяются, качество зависит от масштаба. Делается **в браузере без форка** (N6a). Решение: сначала N6a; «настоящая» печать — только если апстрим Servo получит фрагментацию (следить в N14).


### N7 — Медиа: `<video>/<audio>` и MSE
Размер: M (воспроизведение) + L (MSE) | Зависит от: N0
**Воспроизведение (M):** feature `media-gstreamer` — системный GStreamer (`libgstreamer1.0-dev` не стоит на машине автора; проверка сборки — часть задачи). Решить: ставим зависимость (deb `Depends`, `gst-plugins-*`) или документируем; патентованные кодеки (H.264/AAC) — лицензионный вопрос плагинов. Автовоспроизведение/звук: политика (`request_permission`/pref), `notify_media_session_event` уже есть в делегате.
**MSE (L):** в DOM нет `MediaSource`/`SourceBuffer` (`git grep` по `components/script/dom/mod.rs` пусто). YouTube/Twitch/большинство стримингов без него не играют. Спайк: что делает апстрим (roadmap), сколько стоит минимальный `MediaSource` поверх GStreamer `appsrc` (fMP4 + `SourceBuffer.appendBuffer`, `updateend`, буферизация). Решение «делаем здесь / ждём апстрим / второй движок для видео» — по итогам спайка.
**Приёмка** — воспроизведение MP4/WebM по прямой ссылке (`<video src>`); для MSE — YouTube-ролик.

**Итог спайка T5, часть 1 (2026-10-10).** `MediaSource`/`SourceBuffer` в WebIDL нет (в `HTMLMediaElement.webidl` тип `MediaProvider` содержит `MediaSource` только в комментарии) — MSE в Servo 0.7.0 **не реализован**; воспроизведение идёт через `servo-media` с бэкендами `gstreamer` (feature `media-gstreamer`, нужен системный GStreamer), `dummy`, `ohos`. Минимальный MSE — это новые DOM-интерфейсы (`MediaSource`, `SourceBuffer`, `SourceBufferList`, `ManagedMediaSource`?), очередь сегментов fMP4 в `appsrc` GStreamer, события `updateend`/`sourceopen`/`sourceclose`, `endOfStream`, оценка буфера — **L+, вне объёма до решения «делаем/ждём/второй движок»**. Для сайтов вроде YouTube/Twitch/Netflix это блокер без обходов. Следующий шаг спайка: позиция апстрима (issue/roadmap `servo/servo`), требует доступа к интернету.


### N8 — Service Workers и Cache API
Размер: M | Зависит от: N0, N2 (трафик воркеров должен идти через коннектор)
Включить `dom_serviceworker_enabled` **только** после N2 (иначе загрузки воркеров могут идти мимо `load_web_resource`/коннектора; в доках Servo: воркеры шлют `ServoDelegate::load_web_resource`). Прогнать WPT-подмножество `service-workers/`, составить список блокеров на корпусе (PWA, YouTube Music, Twitter), исправить наиболее частые.
**Приёмка** — тестовый PWA офлайн; корпус не хуже; нет утечек (leakwatch).

### N9 — WebRTC через туннель
Размер: L | Зависит от: N2 (+UDP в коннекторе)
**Цель** — WebRTC без утечки реального IP: все UDP-сокеты (ICE) создаёт встраиватель; режим `relay-only` (только TURN/NRXP UDP), host-кандидаты не раскрываются.
**Задачи** — расширить `NetworkConnector` методом `bind_udp`; провести через `components/media/webrtc` (`webrtc` крейт) фабрику сокетов; политика ICE (`iceTransportPolicy: relay` навязать); mDNS-кандидаты отключить; тест «страница WebRTC-leak не видит реальный IP».
**Приёмка** — автотест утечки зелёный; по умолчанию WebRTC остаётся выключенным.
**Итог N9 (2026-10-10, вариант «relay через прокси», без UDP в коннекторе):** в Servo 0.7 WebRTC минимален (`webrtcbin` из GStreamer; из `iceServers` брался только первый URL как STUN, TURN и `iceTransportPolicy` не поддерживались). Сток `libnice` открывает UDP-сокеты на **каждом** интерфейсе, шлёт SSDP/UPnP-запросы роутеру и раскрывает LAN-адреса и (через STUN) публичный IP — проверено `ss`. Сделано в `components/media/backends/gstreamer/webrtc.rs`: `WebRtcNetworkPolicy{relay_only, proxy, proxy_credentials}` + `servo::set_webrtc_network_policy`; всегда `upnp=false`; при `relay_only`: `ice-udp=false`, `force-relay`, `ice-transport-policy=relay`, STUN игнорируется, агент ограничен адресом 127.0.0.1 (`nice_agent_add_local_address` через `libloading`: `ice-udp` и `ice-tcp` нельзя выключить оба, а TCP-host-кандидаты иначе слушают на всех адресах LAN), TURN-серверы страницы (`turn:`/`turns:`, теперь **все** `iceServers` с логином и паролем — `IceServer`, `configure_ice_servers`) принудительно по TCP; весь ICE-TCP идёт через HTTP `CONNECT`-прокси встраивателя (`proxy-type=2` — это обычный guint, не enum-ник). Юнит-тест разбора TURN-URL. Не проверено: реальный TURN-сервер (медиа через relay) — стояла заглушка; `getUserMedia`/медиатреки Servo 0.7 почти не умеют, звонки Meet/Zoom не ожидаются.

### N10 — HTTP/3 (QUIC) в сетевом слое Servo
Размер: L | Зависит от: N2 (UDP), решение по потолку датаграммы ядра NRXP
Сейчас h3 в `net` отсутствует. Вариант: `quinn` + `h3` в `components/net` за настройкой, сокет — через `NetworkConnector::bind_udp` (fake-UDP поверх NRXP `UdpConnect`/`UdpData`), Alt-Svc/память по origin, гонка h3/h2 с форой, откат на h2. **Блокер вне Servo:** MTU датаграммной ноги NRXP 1150 < 1200 у QUIC — нужно решение владельца ядра (поднять лимит/фрагментация). Без него этап не стартует. План и метрики — как в `netrunner-browser` I10.

### N11 — API встраивателя для UX (пароли, формы, a11y)
Размер: M | Зависит от: N0
- **Менеджер паролей:** события «форма с полем пароля отправлена» (`request_credentials_save{origin, username, password}` — с явным подтверждением) и «найдено поле логина» (для автозаполнения: `WebView::fill_credentials`); разбор `autocomplete`-атрибутов; хранение — на стороне браузера (шифрование)
- **Автозаполнение форм/адресов**, `beforeunload`-диалог (проверить, есть ли `request_unload` — есть: `AllowOrDenyRequest`)
- **a11y:** `notify_accessibility_tree_update`/`set_accessibility_active` уже в API (AccessKit) — подключить в оболочке egui/winit, проверить скринридер (Orca)
**Приёмка** — сохранение и автозаполнение пароля на тестовом логине (`tests/fixtures/auth_*`); Orca читает вкладки/адрес/страницу (ручная проверка?).

### N12 — Мультипроцесс и песочница
Размер: L | Зависит от: N2
Feature `multiprocess` у `servo`: страницы (script/layout) в отдельных процессах, сеть/хранилище — в главном. Это естественно ложится на герметичность: **процессы контента запускаются в пустом netns и без файловых прав на профиль**, весь трафик — через IPC в главный процесс с коннектором. Спайк: работает ли `multiprocess` с нашей оболочкой (egui, offscreen-контекст); стоимость IPC для тела ответов; seccomp/`landlock` для процессов контента (Linux). Закрывает остаток I11 (эксплойт в Servo не получает ключи туннеля).
**Приёмка** — контент-процесс без `socket()` в seccomp-профиле; страница работает; корпус не хуже.

### N13 — Расширения (подмножество WebExtensions MV3)
Размер: L | Зависит от: N11 | Больше про встраивателя, чем про Servo
Что уже есть: `UserContentManager` (скрипты/стили), `load_web_resource` (аналог `webRequest.onBeforeRequest` — только отмена/подмена). Не хватает: изолированные миры для content scripts (сейчас в мире страницы), `chrome.storage`/`runtime` мосты, `declarativeNetRequest`. Спайк: можно ли дать content-скриптам отдельный JS-realm без правки SpiderMonkey-привязок (`components/script`)? Если нет — этап ограничивается userscripts.

### N14 — Сопровождение форка и апстрим
Размер: непрерывно
- ребейз на каждый новый тег Servo (раз в релиз); перед ним — `git range-diff`, прогон тестов встраивания и корпуса
- PR в `servo/servo`: N1 (исправления), N3b (API загрузок), N5b (поиск), N2 в виде общего «embedder connector» — по одному, с тестами
- список «что у нас сверх апстрима» — `docs/nrxp/DELTA.md` (генерируется из `git log v0.7.0..nrxp/0.7`)
- политика безопасности: следить за advisories Servo/SpiderMonkey (embargo не наш), быстрые обновления

## 3. Статусы

| Этап | Название | Зависит | Размер | Статус |
|---|---|---|---|---|
| N0 | Бутстрап форка и сборка по путям | — | S | done (остались тесты встраивания форка) |
| N1 | Срочные исправления (WebGL, хранилище, prefs) | N0 | S | todo |
| N2 | Сеть через коннектор встраивателя | N0 | M–L | done (остаток: UDP, multiprocess) |
| N3 | Загрузки (N3a без форка → N3b в движке) | N0 | M | N3a done (в браузере, 2026-10-10); N3b done (форк + браузер, 2026-10-10) |
| N4 | Данные сайтов, разрешения, сторонние куки | N0 | S–M | N4a done (окно «Данные сайтов»); N4b: сторонние куки done (2026-10-10), постоянные разрешения done (на стороне оболочки, без правок движка) |
| N5 | Поиск по странице (N5a JS → N5b движок) | N0 | S+M | N5a done (JS, 2026-10-10); N5b done (2026-10-10, форк) |
| N6 | Печать / PDF (сначала спайк) | N0 | L | N6a done (растровая печать в браузере, 2026-10-10); настоящая печать — ждёт апстрим |
| N7 | Медиа и MSE (сначала спайки) | N0 | M+L | todo |
| N8 | Service Workers / Cache API | N2 | M | todo |
| N9 | WebRTC через туннель | N2 | L | done в виде relay-only через прокси туннеля (2026-10-10); UDP в коннекторе не делали |
| N10 | HTTP/3 | N2, ядро | L | todo (блокер: потолок датаграммы) |
| N11 | Пароли, формы, a11y | N0 | M | todo |
| N12 | Мультипроцесс и песочница | N2 | L | todo |
| N13 | Расширения (MV3-подмножество) | N11 | L | todo |
| N14 | Сопровождение и апстрим | — | ∞ | постоянно |

**Рекомендуемый порядок:** N0 → N1 → (N3a и N5a параллельно в браузере, без форка) → N2 → N3b → N5b → N4 → N11 → N8 → N7 → N12 → N9 → N6 → N10 → N13.
Обоснование: N1 дёшев и снимает обходы; N3a/N5a дают пользователю функции сразу; N2 — фундамент (N8–N12 на нём стоят);
N10 ждёт решения по ядру, N6/N7 — по спайкам.

## 4. Спайки

| ID | Вопрос | Этап | Что решает |
|---|---|---|---|
| T1 | Где проходят все прямые сокеты в `components/`? (`TcpStream::connect`, `UdpSocket`, `lookup_host`) | N2 | полнота перехвата сети |
| T2 | Как передать несериализуемый `Arc<dyn NetworkConnector>` в `net` при однопроцессном и многопроцессном режимах | N2/N12 | форма API |
| T3 | Как `ParserContext::process_response` отличает навигацию верхнего уровня; можно ли «отдать поток» без второго запроса | N3b | дизайн загрузок |
| T4 | Есть ли страничная вёрстка в `layout`; что умеет `paint` в вектор | N6 | выбор варианта печати |
| T5 | Стоимость минимального MSE поверх GStreamer; позиция апстрима | N7 | делаем/ждём/второй движок |
| T6 | Сколько WPT `service-workers/` проходит на 0.7.0 с настройкой включённой | N8 | объём работ |
| T7 | Работает ли `multiprocess` с нашей оболочкой; цена IPC для тел ответов | N12 | схема песочницы |
| T8 | Изолированные миры JS (realm) для content scripts | N13 | границы расширений |

## 5. Ограничения и допущения

- Оценки размеров — грубые и основаны на объёме затрагиваемого кода, а не на опыте правок Servo (?).
- Форк потребляется как набор **путей** к крейтам; если `[patch.crates-io]` по путям окажется хрупким (транзитивные версии) — перейти на
  полную сборку из workspace форка (`cargo build -p servo` внутри репозитория) и подключать как git-зависимость.
- API-наброски в N2/N3b/N5b — предложения для обсуждения, а не готовый дизайн; апстрим может потребовать другую форму.
- Сборка форка не запускалась (N0 — первая проверка); тулчейн форка (1.97.1) отличается от тулчейна браузера.
- Правки безопасности критичны (сеть, песочница) — нужен ревью человеком до использования с реальными секретами.
