# 08. Dependencies

## Состав
`Cargo.toml` (549 б): eframe 0.28 (обновлён с 0.27), rfd 0.14, qrcode 0.14 (без default features), axum 0.7, tokio 1 (rt-multi-thread, net, fs, io-util, macros, time, sync), futures-util 0.3, serde/serde_json, chrono 0.4 (clock), if-addrs 0.15.

Дерево: **484 пакета** в `Cargo.lock` (124 498 б). Основные ветки: egui/eframe/winit/glutin (UI), axum/tower/hyper (сервер), tokio, rfd (диалоги), qrcode/image, chrono.

## Дубликаты версий (`cargo tree --duplicates`)
| Пакет | Версии | Причина |
|---|---|---|
| log | 0.4.35 ×2 | build-dependency `gl_generator` vs runtime |
| miniz_oxide | 0.8.9, 0.9.1 | разные потребители (image/png, font) |
| raw-window-handle | 0.5.2, 0.6.2 | glutin 0.31 vs winit 0.29 |
| syn | 1.0.109, 3.0.10 | старые proc-macro в транзитивных зависимостях |
| windows-sys | 0.48.0, 0.61.2 | разные крейты |

Дубликаты не несут риска, кроме размера бинарника; `opt-level="s"` + LTO + strip сдерживают его до 5.27 МБ.

## Уязвимости
`cargo-audit` не установлен (и не ставился — не хотелось менять окружение). Проверка выполнена через официальный OSV API (`api.osv.dev/v1/querybatch`, экосистема crates.io) по всем 484 пакетам `Cargo.lock`.

Результат: **0 CVE/уязвимостей**. 10 записей — только advisories категории «unmaintained»:
| Пакет | Advisory | Тип |
|---|---|---|
| derivative 2.2.0 | RUSTSEC-2024-0388 | unmaintained |
| instant 0.1.13 | RUSTSEC-2024-0384 | unmaintained |
| paste 1.0.15 | RUSTSEC-2024-0436 | unmaintained |
| ttf-parser 0.25.1 | RUSTSEC-2026-0192 | unmaintained |
| zbus_names 2.6.1 / 3.0.0 | RUSTSEC-2026-0371 | unmaintained |
| zvariant 3.15.2 / 4.2.0 | RUSTSEC-2026-0370 | unmaintained |
| zvariant_derive 3.15.2 / 4.2.0 | RUSTSEC-2026-0372 | unmaintained |

Все они транзитивные: zbus/zvariant — Linux-ветка `rfd`/portal (в Windows-сборку не компилируются), derivative/instant — цепочка eframe/glow, paste/ttf-parser — egui/ab_glyph. Действий не требуют; появятся как «шум» при подключении `cargo-audit` в CI.

## Риски и находки
- **NS-DEP-01 (INFO)**: 0 известных уязвимостей; 10 unmaintained-предупреждений; отсутствие `cargo-audit`/`cargo-deny` в окружении и CI.
- **NS-DEP-02 (LOW)**: eframe 0.28 содержит известный баг egui `hit_test` (F-50), вызывающий редкие паники UI; обновление до 0.37 требует переписывания окна и отложено. Падение смягчено перезапуском окна и тем, что сервер живёт отдельно.
- **NS-DEP-03 (INFO)**: нет фиксации обновлений через cargo-deny/renovate, нет CI; `Cargo.lock` в репозитории есть, но git отсутствует (R-01) — воспроизводимость держится на локальных файлах и `.bak`.

## Рекомендации
1. Добавить `cargo-audit` (или `cargo-deny`) в pre-release проверку — разово, без влияния на код.
2. Не обновлять eframe «на всякий случай»: только вместе с задачей F-50.
3. Держать `Cargo.lock` и `Cargo.toml` в git, как только появится репозиторий.
4. При обновлении rfd/axum/токио проверить MSRV и Windows-таргеты (Linux-ветки не собираются, но лишние advisories появляются).

Оценка: **8/10**.
