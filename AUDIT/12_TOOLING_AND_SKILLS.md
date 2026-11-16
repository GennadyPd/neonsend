# 12. Tooling and Skills

## Что устанавливалось и куда
Проектные скиллы OpenCode: `<project>/.opencode/skills/<id>/SKILL.md` (проверено по https://opencode.ai/docs/skills/ — ID берётся из имени папки, frontmatter опционален, поиск идёт от текущего каталога вверх).

Установлено в `C:\Users\user\Documents\NeonSend\.opencode\skills\`:

### 1. `security-audit` (источник: cloudflare/security-audit-skill)
```
security-audit/
  SKILL.md                              (22 КБ)
  ATTACK-CLASSES.md
  WEB-PROTOCOL-AND-AUTH.md
  RESOURCE-EXHAUSTION-AND-AVAILABILITY.md
  DESKTOP-MOBILE-AND-LOCAL-IPC.md
  VALIDATION-AND-REPORTING.md
```
- Что даёт: source-first методология, правило «no finding without a path from attacker-controlled input to impact», классификация атак, проверка заголовков/auth, отчёты с boundary + observed result.
- Как использован: guidance-режим + reference-чек-листы (web/auth, resource exhaustion, local IPC). Именно по ним проверялись guard, path traversal, IDOR, лимиты, заголовки.

### 2. `reviewing-security` (источник: osmontero/opencode-skills)
```
reviewing-security/
  SKILL.md                              (13.5 КБ)
  references/vulnerability-patterns.md  (7.9 КБ)
```
- Что даёт: железное правило «NO FINDING WITHOUT A PATH FROM ATTACKER-CONTROLLED INPUT TO IMPACT», таблица severity, формат отчёта, порядок проверки (surface → sinks → authorization → secrets → deps).
- Как использован: структура 02_SECURITY_AUDIT и правило «severity только для подтверждённых»; `needs_validation` не присваивали severity.

## Что отклонено и почему
- **mattpocock/skills** — 40 SKILL.md, всё про engineering/productivity/TypeScript-workflow (ask-matt, code-review, tdd, triage, pr, wizard, wayfinder и т.п.). Ничего специфичного для Rust/аудита; дублирует уже установленные `do`, `make-plan`, `ponytail-review`, `handoff`, `grilling`. Установка добавила бы шум в репозиторий продукта.
- **Полный 6-фазный workflow cloudflare** — требует OS-sandbox с изолированным loopback и без внешней сети, пишет прогоны в `~/security-audit-skill/<repo>/run-N`, findings.json по схеме и три `.cjs`-валидатора. В среде нет песочницы, а живой сервер на LAN нельзя изолировать; поэтому взяли только методологию и чек-листы, без пайплайна и скриптов.
- **report-schema.json и `.cjs`-валидаторы** — не нужны: отчёт по 14 документам, формат задан пользователем.
- **osmontero a11y/perf/testing-скиллы** (`building-accessible-interfaces`, `investigating-performance`, `testing-webapps`) — не ставились: пересекаются с ponytail-ревью, а лишние файлы в репозитории продукта не нужны; a11y проверена вручную (DOM-метрики + расчёт контраста).
- **Остальные reference cloudflare** (AI-AND-LLM, CLOUD-*, MEMORY-SAFETY, PROTOCOLS-RPC, DATA-ISOLATION, RECONNAISSANCE, HUNTING, SUPPLY-CHAIN) — не относятся к локальному Rust-серверу без LLM/облака/бинарной памяти.

## Инструменты, фактически использованные
| Инструмент | Назначение | Результат |
|---|---|---|
| `cargo build --release` | сборка | успешно, 0.26 с инкрементально |
| `cargo test` | тесты | 5/5 passed |
| `cargo tree --duplicates` | дубликаты | 5 пар (log, miniz_oxide, raw-window-handle, syn, windows-sys) |
| OSV API (`api.osv.dev/v1/querybatch`) | уязвимости | 484 пакета, 0 vulns, 10 unmaintained |
| Node-скрипты `C:\Temp\opencode\*.mjs` | живые HTTP-сценарии, замеры, CDP | см. 13_TEST_EVIDENCE |
| CDP (порт 9223, Android Chrome) | DOM/ARIA-метрики | 0 заголовков, нет label, 0 aria-live, чип 36 px |
| PowerShell `snap.ps1` | скриншот окна egui | `win.png` (576×979) |
| Расчёт контраста (Node) | WCAG по 4 темам | `--faint` 2.48–3.52 |

`cargo-audit` в системе отсутствует и не устанавливался (чтобы не менять окружение); его роль закрыл OSV API.

## Ограничения среды, повлиявшие на аудит
- Chrome на ПК с CDP был закрыт, экран Android-устройства выключен → `Page.captureScreenshot` таймаутил; скриншоты веб-части не получены (UX-07).
- iOS-устройства нет → B-03/B-19 не проверены.
- PowerShell портит кириллицу в пайплайнах — файлы создавались инструментом записи/Node, не через `Out-File` с текстом.
- Git-репозитория нет → правки не отслеживались, аудит read-only по факту (создан только каталог `AUDIT/`).

## Итог по скиллам
Два установленных скилла дали ровно то, что нужно: чек-листы и дисциплину доказательств, без тяжёлого пайплайна. Остальные кандидаты отклонены осознанно — либо не про этот стек, либо дублируют имеющееся, либо требуют инфраструктуры, которой нет.
