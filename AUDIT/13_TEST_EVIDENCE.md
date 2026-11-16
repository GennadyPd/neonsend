# 13. Test Evidence

Все проверки — на живом `C:\Users\user\Documents\NeonSend\target\release\neonsend.exe` (PID 3596 → 12240 после перезапуска), LAN `192.168.1.25:8765`, реальные сессии: ПК `id=0 PC-1`, Android Chrome `id=1 Phone`. Скрипты и логи — в `C:\Temp\opencode\`.

## Окружение и сборка
```
cargo build --release   → Finished `release` profile [optimized] target(s) in 0.26s
cargo test              → running 5 tests … test result: ok. 5 passed; 0 failed
exe                     5 266 432 б, eframe 0.28.1, axum 0.7.9
```
Логи: `build.log`, `test.log`. Предупреждений в прогоне не выведено (инкрементальная сборка без пересборки крейтов).

## Живой полный цикл (`audit_flow.mjs`)
Комната 1→3: `request` → `answer accept` → `push planned` → upload 1/20/200 МБ → `sender_done` → download → `done` → `clear`.

| Замер | Значение |
|---|---|
| upload 1 МБ | 166 МБ/с |
| upload 20 МБ (2×10 МБ) | 527 / 542 МБ/с, resume `/api/part` → `{"off":10485760}` |
| upload 200 МБ | 626 МБ/с (319 мс) |
| download 1 МБ | 26 МБ/с (первый, прогрев) |
| download 20 МБ | 399 МБ/с |
| download 200 МБ | 626 МБ/с |
| sha256 | совпал для всех трёх файлов |
| `/api/state` (50 замеров) | p50 0.3 мс · p95 1.3 мс · max 2.6 мс |
| процесс | RSS 83.6 МБ · CPU ~12 с · 428 handles · 19 threads |

## Безопасность — сырые результаты
```
POST /api/request?from=1&to=3&files=1   → 200 {"id":4}      (от имени чужих устройств)
GET  /api/state?id=1                    → комната устройства 1, temp:"Temp(PC-1-Phone-4)"
POST /api/answer?id=4&accept=1          → {"ok":true}       (без идентичности)
POST /api/leave?id=999&req=4            → {"ok":true}       (чужой id прервал)
POST /api/done?id=999&req=5             → {"ok":true}       (комната удалена)
GET  /dl?req=7&name=c.bin&id=0          → 200, 20 байт      (PC_ID=0 известен)
GET  /api/part?req=8&name=hole.bin&id=999 → 200 {"off":10}  (утечка размера)
POST /api/exists?req=7                  → {"have":["c.bin"]} (оракул без владельца)
```
Guard: `Host: evil.com` → 403 «чужой адрес»; `Origin: http://evil.com` → 403 «чужой источник»; неизвестный маршрут → 404; `POST /api/ping` → 405; `/api/part` с `../../etc` → 400 «недопустимый путь».

## Функциональные сценарии (`func2`–`func5`)
```
request to dead id    200 {"error":"устройство не на связи"}
request to self       200 {"error":"нельзя передать самому себе"}
request while room    200 {"error":"вы уже в передаче — завершите её"}
answer reject         200 {"ok":true}; sender state=abort err="получатель отклонил передачу"
text 16 B             200 {"ok":true}; получатель видит "hello from audit"
text 70 KB            200 {"error":"текст слишком длинный (до 64 КБ)"}
text as recipient     200 {"error":"передача не разрешена"}
push planned          200 {"ok":true}
upload ow=0 existing  200 {"ok":true,"skip":true,"bytes":0}
upload ow=2 existing  имя освобождено; exists → {"have":["c.bin"]}
upload off>len(.part) 409 {"error":"кусок пришёл раньше начала файла"}
chunked 2×10 B        файл 20 B; items [{"name":"c.bin","size":10}]   ← баг
chunk 10 B + off=100  файл 105 B (дыра), items size=5                ← баг
sender_done           200; room state=ok, sender_done=true
done                  200; комната удалена
```
Прогресс/целостность: `/dl` отдал 20 Б и 105 Б ровно как на диске, `Content-Length` совпал.

## Заголовки
```
GET /  → 200; Content-Length: 61937
         Content-Type: text/html; charset=utf-8
         Cache-Control: no-store
         (нет CSP, X-Frame-Options, X-Content-Type-Options, Referrer-Policy)
GET /dl → 200; application/octet-stream; Content-Length; X-Content-Type-Options: nosniff;
         Content-Disposition: attachment; filename="…"; filename*=UTF-8''…
```

## UI / доступность
Скриншот окна: `C:\Temp\opencode\win.png` (окно 560×940, кадр 576×979). Видно: QR + URL, «принято: 3 файлов · 211.0 МБ», «Темп: 221.0 МБ · свободно: 149.0 ГБ», устройства (PC-1, PC-1, Phone), журнал обрезан снизу.

DOM-метрики (Android Chrome, CDP 9223):
```
h1–h4: 0 | landmarks: 2 | label[for=nameInput]: false | aria-label: null
aria-modal: 2 | aria-labelledby: 0 | прогресс с aria-live: 0
кнопки <44px: ["неон:36"] | lang=ru | title=NeonSend
```

Контраст (WCAG 2.1, расчёт Node):
```
neon   text 17.37 | muted 6.76 | faint 3.52 | neon 15.08 | danger 6.70 | warn 13.03
cyber  text 18.16 | muted 6.34 | faint 3.29 | neon 11.15 | danger 6.05 | warn 13.97
purple text 17.22 | muted 6.54 | faint 3.26 | neon  7.67 | danger 7.56 | warn 14.06
light  text 15.54 | muted 4.74 | faint 2.48 | neon  3.05 | danger 4.73 | warn  4.76
```
`--faint` ниже 4.5 во всех темах; в light также акцент.

## Зависимости
```
cargo tree --duplicates → log 0.4.35 ×2; miniz_oxide 0.8.9/0.9.1;
raw-window-handle 0.5.2/0.6.2; syn 1.0.109/3.0.10; windows-sys 0.48.0/0.61.2
OSV querybatch: 484 пакета → 10 записей, все "unmaintained"
(RUSTSEC-2024-0388, -0384, -0436, -2026-0192, -0370/0371/0372)
```
Полные ответы: `osv_out.txt`.

## Артефакты аудита
- `C:\Users\user\Documents\NeonSend\AUDIT\00…13` — эти документы (единственные созданные файлы в проекте).
- `C:\Temp\opencode\`: `audit_flow.mjs`, `audit_func2.mjs`…`audit_func5.mjs`, `func*_out.txt`, `build.log`, `test.log`, `deps_dup.txt`, `osv.mjs`, `osv2.mjs`, `osv_out.txt`, `contrast.mjs`, `win.png`.
- Тестовые данные: комнаты 4–8 создавали `Temp(PC-1-Phone-N)` в `C:\Users\user\Downloads\NeonSendTemp`; удалены после аудита (только созданные тестами). Пользовательские файлы в папке приёма не тронуты.
- Записи в `devices.json`/`log` от тестовых сессий не чистились (история устройств — пользовательские данные).

## Ограничения доказательной базы
1. Веб-скриншоты не сняты: экран Android выключен, CDP-сессия не отдаёт кадр; Chrome на ПК закрыт.
2. iOS не тестировался (нет устройства) — B-03/B-19 только по коду/документации.
3. Таймауты 180/60 с и рестарт сервера при передаче не воспроизводились (слишком долго/разрушительно).
4. Нагрузка 10+ клиентов не проверялась; выводы по производительности — на одном клиенте и локальном цикле.
5. `cargo-audit` недоступен; уязвимости проверены через OSV API (тот же источник RUSTSEC).
