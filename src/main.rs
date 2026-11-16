// без этого при двойном клике на exe рядом открывается чёрное окно консоли
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod server;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

#[derive(Clone, Copy)]
struct Pal {
    bg: Color32,
    panel: Color32,
    card: Color32,
    line: Color32,
    neon: Color32,
    dim: Color32,
    text: Color32,
    red: Color32,
    warn: Color32,
}

// Неоновая — по умолчанию; остальные включаются списком внизу окна.
const THEMES: [(&str, Pal); 4] = [
    ("неон", Pal {
        bg: Color32::from_rgb(8, 9, 11),
        panel: Color32::from_rgb(11, 13, 16),
        card: Color32::from_rgb(15, 18, 21),
        line: Color32::from_rgb(34, 42, 48),
        neon: Color32::from_rgb(61, 255, 143),
        dim: Color32::from_rgb(138, 154, 147),
        text: Color32::from_rgb(231, 242, 236),
        red: Color32::from_rgb(255, 92, 122),
        warn: Color32::from_rgb(255, 200, 110),
    }),
    ("киберпанк", Pal {
        bg: Color32::from_rgb(5, 7, 12),
        panel: Color32::from_rgb(8, 12, 20),
        card: Color32::from_rgb(13, 22, 38),
        line: Color32::from_rgb(30, 58, 82),
        neon: Color32::from_rgb(34, 211, 238),
        dim: Color32::from_rgb(125, 147, 168),
        text: Color32::from_rgb(228, 246, 255),
        red: Color32::from_rgb(255, 47, 179),
        warn: Color32::from_rgb(255, 209, 102),
    }),
    ("фиолетовая", Pal {
        bg: Color32::from_rgb(8, 4, 17),
        panel: Color32::from_rgb(13, 6, 23),
        card: Color32::from_rgb(24, 12, 38),
        line: Color32::from_rgb(63, 36, 96),
        neon: Color32::from_rgb(192, 132, 252),
        dim: Color32::from_rgb(157, 139, 178),
        text: Color32::from_rgb(241, 233, 255),
        red: Color32::from_rgb(255, 106, 162),
        warn: Color32::from_rgb(255, 209, 102),
    }),
    ("светлая", Pal {
        bg: Color32::from_rgb(242, 246, 244),
        panel: Color32::from_rgb(255, 255, 255),
        card: Color32::from_rgb(247, 251, 249),
        line: Color32::from_rgb(220, 231, 225),
        neon: Color32::from_rgb(9, 162, 87),
        dim: Color32::from_rgb(92, 114, 104),
        text: Color32::from_rgb(14, 32, 25),
        red: Color32::from_rgb(201, 49, 90),
        warn: Color32::from_rgb(148, 99, 0),
    }),
];

// Витринные цвета тем для точек в меню: в светлой теме pal().neon тёмный и читался плохо.
const ACCENT: [Color32; 4] = [
    Color32::from_rgb(61, 255, 143),
    Color32::from_rgb(34, 211, 238),
    Color32::from_rgb(192, 140, 255),
    Color32::from_rgb(157, 247, 192),
];

static THEME: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn pal() -> Pal {
    THEMES[THEME.load(std::sync::atomic::Ordering::Relaxed).min(THEMES.len() - 1)].1
}

fn theme_index(name: &str) -> usize {
    THEMES.iter().position(|(n, _)| *n == name).unwrap_or(0)
}

fn set_theme(i: usize) {
    THEME.store(i.min(THEMES.len() - 1), std::sync::atomic::Ordering::Relaxed);
}

struct App {
    shared: Arc<server::Shared>,
    ip: String,
    /// все локальные адреса: между ними можно переключаться в окне
    ips: Vec<String>,
    port: u16,
    root: Option<PathBuf>,
    qr: Option<egui::TextureHandle>,
    err: Option<String>,
    theme: usize,
    forget: std::collections::HashSet<String>,
    ask_purge: bool,
    /// кэш размеров: (когда считали, Temp, свободно на диске)
    sizes: (Instant, u64, u64),
    /// адрес скопирован — показываем подтверждение недолго
    copied: Option<Instant>,
    /// пользователь нажал закрыть во время передачи — спрашиваем подтверждение
    exit_ask: bool,
    /// закрытие подтверждено, больше не перехватываем
    force_exit: bool,
    /// ширина всех карточек: по самой длинной строке внутри + 12 px
    box_w: f32,
    /// сколько кадров окна упало во внутренностях egui (сервер при этом жив)
    ui_crashes: u32,
}

/// Всё, что переживает падение окна: сервер и настройки, с которыми его подняли.
#[derive(Clone)]
struct Boot {
    shared: Arc<server::Shared>,
    ip: String,
    ips: Vec<String>,
    port: u16,
    root: Option<PathBuf>,
    err: Option<String>,
    theme: usize,
}

/// Поднимает сервер один раз: окно может упасть и перезапуститься, сервер — нет.
fn boot() -> Boot {
    let (cfg_root, theme_name, cfg_ip) = match load_config() {
        Some((r, t, i)) => (r, t, i),
        None => (None, "неон".to_string(), None),
    };
    let theme = theme_index(&theme_name);
    set_theme(theme);

    let ips = local_ips();
    let ip = cfg_ip
        .filter(|i| ips.contains(i))
        .or_else(|| ips.first().cloned())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let shared = server::Shared::new();
    let root = cfg_root.or_else(default_root);
    if let Some(r) = &root {
        *shared.root.lock().unwrap() = Some(r.clone());
    }

    // второй экземпляр не должен поднимать второй сервер и путать устройства
    if running_instance() {
        return Boot {
            shared,
            ip,
            ips,
            port: 0,
            root,
            err: Some("NeonSend уже запущен".to_string()),
            theme,
        };
    }

    let mut port = 0;
    let mut err = None;
    let mut last = String::new();
    for candidate in 8765..=8790 {
        match server::start(&ip, candidate, shared.clone()) {
            Ok(()) => {
                port = candidate;
                break;
            }
            Err(e) => last = e,
        }
    }
    if port == 0 {
        err = Some(format!("не удалось занять порт 8765–8790: {last}"));
    }
    if let Some(r) = &root {
        shared.note(format!("папка приёма: {}", r.display()));
    } else {
        shared.note("папка приёма не выбрана");
    }
    if port != 0 {
        shared.note(format!("сервер запущен: http://{ip}:{port}"));
    }

    Boot { shared, ip, ips, port, root, err, theme }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, b: Boot) -> Self {
        style(&cc.egui_ctx);
        let addr = format!("http://{}:{}", b.ip, b.port);
        let qr = if b.port != 0 { make_qr(&cc.egui_ctx, &addr) } else { None };

        Self {
            shared: b.shared,
            ip: b.ip,
            ips: b.ips,
            port: b.port,
            root: b.root,
            qr,
            err: b.err,
            theme: b.theme,
            forget: std::collections::HashSet::new(),
            ask_purge: false,
            sizes: (Instant::now(), 0, 0),
            copied: None,
            exit_ask: false,
            force_exit: false,
            box_w: 400.0,
            ui_crashes: 0,
        }
    }

    fn addr(&self) -> String {
        format!("http://{}:{}", self.ip, self.port)
    }

    /// Ширина карточек: самая длинная строка внутри + 12 px (по 6 px с каждой стороны).
    fn measure_box(&mut self, ctx: &egui::Context) {
        let text_w = |text: &str, size: f32, mono: bool| -> f32 {
            let family = if mono {
                egui::FontFamily::Monospace
            } else {
                egui::FontFamily::Proportional
            };
            ctx.fonts_mut(|f| {
                f.layout_no_wrap(text.to_owned(), egui::FontId::new(size, family), pal().text)
                    .size()
                    .x
            })
        };
        let mut w: f32 = 330.0;
        w = w.max(118.0 + 10.0 + text_w(&self.addr(), 16.5, true));
        // ряд кнопок «Копировать» + «Открыть в браузере» правее QR
        w = w.max(118.0 + 10.0 + 96.0 + 8.0 + 126.0);
        if let Some(p) = &self.root {
            w = w.max(text_w(&p.display().to_string(), 12.5, true));
        }
        let (files, bytes) = *self.shared.received.lock().unwrap();
        w = w.max(text_w(
            &format!(
                "Temp: {:.1} МБ · свободно: {:.1} ГБ",
                self.sizes.1 as f64 / 1_048_576.0,
                self.sizes.2 as f64 / 1_073_741_824.0
            ),
            12.0,
            false,
        ));
        w = w.max(
            text_w(&format!("принято: {files}"), 17.5, false)
                + 6.0
                + text_w(&format!("файлов · {:.1} МБ", bytes as f64 / 1_048_576.0), 12.5, false),
        );
        {
            let log = self.shared.log.lock().unwrap();
            for line in log.iter().rev().take(50) {
                w = w.max(text_w(line, 12.5, true));
            }
        }
        for d in self.shared.devices_snapshot() {
            w = w.max(text_w(&d, 12.5, false) + 25.0);
        }
        for (ip, name) in self.shared.names_list() {
            w = w.max(text_w(&format!("{name}  ·  {ip}"), 12.5, false) + 25.0);
        }
        for (from, to, ..) in self.shared.transfers() {
            w = w.max(text_w(&format!("{from} → {to}"), 12.5, false));
        }
        if let Some(e) = &self.err {
            w = w.max(text_w(e, 14.5, false));
        }
        // не шире доступного места: не меньше 24 px до правой и левой границы окна
        let avail = ctx.content_rect().width() - 48.0;
        // +72: по 24 внутренних отступа, 12 запаса на текст и 12 на запас ширины
        self.box_w = (w + 72.0).min(avail).max(330.0_f32.min(avail));
    }
}

/// Карточка раздела: полупрозрачная поверхность с тонкой границей.
/// Центрирует карточки: зазор слева равен зазору справа (при скрытом скроллбаре).
fn column<R>(ui: &mut egui::Ui, w: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    // поля считаем от края окна: полоса прокрутки рисуется внутри правого поля,
    // поэтому зазоры слева и справа остаются одинаковыми
    let off = ((ui.ctx().content_rect().width() - w) * 0.5).max(0.0);
    ui.horizontal_top(|ui| {
        ui.add_space(off);
        ui.vertical(|ui| {
            ui.set_max_width(w);
            add(ui)
        })
        .inner
    })
    .inner
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(pal().card)
        .corner_radius(14)
        .stroke(egui::Stroke::new(1.0_f32, pal().line))
        .inner_margin(egui::Margin::same(24))
        .show(ui, |ui| {
            // ширину задаёт column: карточка занимает всё, что ей дали
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Заголовок раздела внутри карточки.
fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(pal().dim));
    ui.add_space(2.0);
}

/// Индикатор состояния: точка с мягким свечением.
fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
    let c = rect.center();
    ui.painter().circle_filled(c, 5.0, Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40));
    ui.painter().circle_filled(c, 3.0, color);
}

/// Логотип: шестиугольник с буквой N.
fn logo(ui: &mut egui::Ui, side: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
    let c = rect.center();
    let r = side * 0.46;
    let pts: Vec<egui::Pos2> = (0..6)
        .map(|i| {
            let a = std::f32::consts::FRAC_PI_6 + i as f32 * std::f32::consts::PI / 3.0;
            egui::pos2(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect();
    let p = ui.painter();
    p.add(egui::Shape::closed_line(pts, egui::Stroke::new(1.6_f32, pal().neon)));
    p.text(
        c,
        egui::Align2::CENTER_CENTER,
        "N",
        egui::FontId::proportional(side * 0.46),
        pal().neon,
    );
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // падение окна не должно ронять HTTP-сервер и передачу
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.draw(&ctx, ui))).is_err() {
            self.ui_crashes += 1;
        }
    }
    fn on_exit(&mut self) {
        // приложение закрывают — временные папки больше не нужны
        self.shared.delete_temps();
    }
}

impl App {
    fn draw(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ctx.request_repaint_after(Duration::from_millis(400));

        // защита от дурака: не даём закрыть окно посреди передачи
        if ctx.input(|i| i.viewport().close_requested()) && !self.force_exit && self.shared.busy() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.exit_ask = true;
        }
        if self.exit_ask {
            egui::Window::new("идёт передача")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(
                        RichText::new("Передача ещё не закончена. Если закрыть окно, она прервётся.")
                            .size(13.0)
                            .color(pal().text),
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Всё равно закрыть").size(13.0)).clicked() {
                            self.exit_ask = false;
                            self.force_exit = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if ui.button(RichText::new("Подождать").size(13.0)).clicked() {
                            self.exit_ask = false;
                        }
                    });
                });
        }
        self.measure_box(ctx);

        // шапка — отдельная панель: при прокрутке контента она остаётся на месте
        let mut head_bottom = 0.0_f32;
        egui::Panel::top("head_panel")
            .frame(egui::Frame::NONE.fill(pal().card))
            .show(ui, |ui| {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    // логотип и название прижаты к левому краю с отступом 24 px
                    ui.add_space(24.0);
                    logo(ui, 30.0);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("NEONSEND").size(19.5).strong().color(pal().neon));
                        ui.label(
                            RichText::new("локальная передача файлов")
                                .size(12.5)
                                .color(pal().dim),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // состояние и тема — к правому краю, тоже 24 px
                        ui.add_space(24.0);
                        self.theme_widget(ui, ctx);
                        ui.add_space(8.0);
                        dot(ui, if self.port != 0 { pal().neon } else { pal().red });
                        ui.label(
                            RichText::new(if self.port != 0 { "работает" } else { "остановлен" })
                                .size(12.5)
                                .color(if self.port != 0 { pal().dim } else { pal().red }),
                        );
                    });
                });
                ui.add_space(12.0);
                head_bottom = ui.max_rect().bottom();
            });

        // журнал живёт в нижней панели: его прокрутка не задевает страницу
        let mut log_top = 0.0_f32;
        egui::Panel::bottom("log_panel")
            .frame(egui::Frame::NONE.fill(pal().bg))
            .exact_size(186.0)
            .show(ui, |ui| {                log_top = ui.max_rect().top();
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(12.0);
                let w = (ui.ctx().content_rect().width() - 48.0).max(200.0);
                ui.horizontal_top(|ui| {
                    // по 12 px от краёв панели: карточка занимает почти всю её площадь
                    ui.add_space(24.0);
                    ui.vertical(|ui| {
                        ui.set_width(w);
                        card(ui, |ui| {
                            section(ui, "ЖУРНАЛ");
                            // высота ограничена панелью: длинные строки не должны вылезать за карточку
                            egui::ScrollArea::vertical()
                                .max_height(92.0)
                                .auto_shrink([false, false])
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    let log = self.shared.log.lock().unwrap();
                                    if log.is_empty() {
                                        ui.label(
                                            RichText::new("событий пока нет")
                                                .size(12.5)
                                                .color(pal().dim),
                                        );
                                    }
                                    for line in log.iter() {
                                        ui.label(
                                            RichText::new(line)
                                                .monospace()
                                                .size(12.5)
                                                .color(pal().dim),
                                        );
                                    }
                                });
                        });
                    });
                });
                ui.add_space(12.0);
            });

        // серая полоска шапки: рисуем после контента, иначе карточки её перекрывают
        let xr = ctx.content_rect().x_range();
        {
            let p = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("head_rule"),
            ));
            p.hline(xr, head_bottom - 2.0, egui::Stroke::new(2.0_f32, pal().line));
            // полоска над панелью журнала — тоже поверх карточек
            p.hline(xr, log_top + 2.0, egui::Stroke::new(2.0_f32, pal().line));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(pal().bg).inner_margin(egui::Margin {
                left: 0,
                right: 0,
                top: 0,
                bottom: 0,
            }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // зазор в самом начале списка, а не отступ панели: при прокрутке карточки
                    // проходят через эту полосу и не прячутся за ней
                    ui.add_space(24.0);
                    column(ui, self.box_w, |ui| {
                    if let Some(e) = &self.err {
                        card(ui, |ui| {
                            ui.label(RichText::new(e).size(14.5).color(pal().red));
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("закройте другую копию NeonSend или освободите порт")
                                    .size(12.5)
                                    .color(pal().dim),
                            );
                        });
                        return;
                    }

                    // ---- подключение: QR, адрес, копирование, открыть ----
                    card(ui, |ui| {
                        section(ui, "ПОДКЛЮЧЕНИЕ");
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if let Some(tex) = &self.qr {
                                let side = 118.0;
                                let (rect, _) =
                                    ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
                                // светлая подложка под QR — иначе на тёмном фоне не читается
                                ui.painter().rect_filled(
                                    rect.expand(6.0),
                                    egui::CornerRadius::same(10),
                                    Color32::from_rgb(230, 245, 238),
                                );
                                ui.painter().image(
                                    tex.id(),
                                    rect,
                                    egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    Color32::WHITE,
                                );
                            }
                            ui.add_space(10.0);
                            ui.vertical(|ui| {
                                let addr = self.addr();
                                let resp = ui.add(
                                    egui::Label::new(
                                        RichText::new(&addr)
                                            .monospace()
                                            .size(16.5)
                                            .color(pal().neon),
                                    )
                                    .sense(egui::Sense::click()),
                                );
                                if resp.clicked() {
                                    open_url(&addr);
                                }
                                resp.on_hover_text("открыть в браузере");

                                ui.add_space(6.0);
                                let copied = self
                                    .copied
                                    .map(|t| t.elapsed() < Duration::from_millis(1400))
                                    .unwrap_or(false);
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_sized(
                                            [96.0, 30.0],
                                            egui::Button::new(
                                                RichText::new(if copied {
                                                    "скопировано"
                                                } else {
                                                    "Копировать"
                                                })
                                                .size(12.5),
                                            ),
                                        )
                                        .clicked()
                                    {
                                        ui.ctx().copy_text(addr.clone());
                                        self.copied = Some(Instant::now());
                                    }
                                    if ui
                                        .add_sized(
                                            [126.0, 30.0],
                                            egui::Button::new(RichText::new("Открыть в браузере").size(12.5)),
                                        )
                                        .clicked()
                                    {
                                        open_url(&addr);
                                    }
                                });
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new("наведите камеру телефона на QR-код")
                                        .size(12.5)
                                        .color(pal().dim),
                                );
                                // несколько сетей (Wi-Fi + Ethernet, VPN) — выбираем, что показать
                                if self.ips.len() > 1 {
                                    ui.add_space(6.0);
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new("сеть:").size(12.5).color(pal().dim));
                                        let mut pick = self.ip.clone();
                                        egui::ComboBox::from_id_salt("ip_pick")
                                            .selected_text(
                                                RichText::new(pick.clone()).monospace().size(12.5),
                                            )
                                            .show_ui(ui, |ui| {
                                                for ip in &self.ips {
                                                    ui.selectable_value(
                                                        &mut pick,
                                                        ip.clone(),
                                                        RichText::new(ip).monospace().size(12.5),
                                                    );
                                                }
                                            });
                                        if pick != self.ip {
                                            self.ip = pick;
                                            self.shared.set_local(&self.ip);
                                            self.qr = make_qr(ui.ctx(), &self.addr());
                                            save_config(
                                                self.root.as_ref(),
                                                THEMES[self.theme].0,
                                                Some(&self.ip),
                                            );
                                        }
                                    });
                                }
                            });
                        });
                    });

                    ui.add_space(8.0);

                    // ---- приём: папка, статистика, диск, Temp ----
                    card(ui, |ui| {
                        section(ui, "ПРИЁМ ФАЙЛОВ");
                        match self.root.clone() {
                            Some(p) => {
                                ui.label(
                                    RichText::new(p.display().to_string())
                                        .monospace()
                                        .size(12.5)
                                        .color(pal().text),
                                );
                                ui.add_space(6.0);
                                let busy = self.shared.busy();
                                ui.horizontal(|ui| {
                                    if ui.button(RichText::new("Открыть папку").size(12.5)).clicked() {
                                        let _ = std::process::Command::new("explorer.exe")
                                            .arg(&p)
                                            .spawn();
                                    }
                                    // во время передачи папку не меняем — файлы поедут не туда
                                    if ui
                                        .add_enabled(
                                            !busy,
                                            egui::Button::new(RichText::new("Сменить папку").size(12.5)),
                                        )
                                        .on_disabled_hover_text("идёт передача — дождитесь её окончания")
                                        .clicked()
                                    {
                                        self.pick_folder();
                                    }
                                });
                            }
                            None => {
                                ui.label(
                                    RichText::new("папка не выбрана — файлы не принимаются")
                                        .size(12.5)
                                        .color(pal().red),
                                );
                                ui.add_space(6.0);
                                if ui.button(RichText::new("Выбрать папку").size(12.5)).clicked() {
                                    self.pick_folder();
                                }
                            }
                        }

                        let (files, bytes) = *self.shared.received.lock().unwrap();
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("принято: {files}"))
                                    .size(17.5)
                                    .strong()
                                    .color(pal().neon),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "файлов · {:.1} МБ",
                                    bytes as f64 / 1_048_576.0
                                ))
                                .size(12.5)
                                .color(pal().dim),
                            );
                        });

                        // размеры пересчитываем раз в 3 с: обход папок не для каждого кадра
                        if self.sizes.0.elapsed() > Duration::from_secs(3) {
                            let temp = self.shared.temp_size();
                            let free = self.root.as_deref().and_then(free_space).unwrap_or(0);
                            self.sizes = (Instant::now(), temp, free);
                        }
                        ui.label(
                            RichText::new(format!(
                                "Temp: {:.1} МБ · свободно: {:.1} ГБ",
                                self.sizes.1 as f64 / 1_048_576.0,
                                self.sizes.2 as f64 / 1_073_741_824.0
                            ))
                            .size(12.5)
                            .color(pal().dim),
                        );

                        ui.add_space(6.0);
                        if ui
                            .button(RichText::new("Удалить временные папки").size(12.5))
                            .clicked()
                        {
                            if self.shared.busy() {
                                self.ask_purge = true;
                            } else {
                                self.shared.delete_temps();
                            }
                        }
                        if self.ask_purge {
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("идёт передача — точно удалить папки?")
                                    .size(12.5)
                                    .color(pal().warn),
                            );
                            ui.horizontal(|ui| {
                                if ui.button(RichText::new("Да, удалить").size(12.5)).clicked() {
                                    self.shared.delete_temps();
                                    self.ask_purge = false;
                                }
                                if ui.button(RichText::new("Отмена").size(12.5)).clicked() {
                                    self.ask_purge = false;
                                }
                            });
                        }
                    });

                    ui.add_space(8.0);

                    // ---- передачи ----
                    card(ui, |ui| {
                        section(ui, "ПЕРЕДАЧИ");
                        let tr = self.shared.transfers();
                        if tr.is_empty() {
                            ui.label(RichText::new("нет активных").size(12.5).color(pal().dim));
                        }
                        for (from, to, state, done, total) in tr {
                            let what = if state == "wait" {
                                "ждёт подтверждения"
                            } else {
                                "идёт передача"
                            };
                            ui.label(
                                RichText::new(format!("{from} → {to}"))
                                    .size(12.5)
                                    .color(pal().text),
                            );
                            ui.label(
                                RichText::new(format!("{what} · {done} из {total} файлов"))
                                    .size(12.5)
                                    .color(pal().dim),
                            );
                            let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                            ui.add(
                                egui::ProgressBar::new(frac)
                                    .desired_width(f32::INFINITY)
                                    .fill(pal().neon),
                            );
                            ui.add_space(4.0);
                        }
                    });

                    ui.add_space(8.0);

                    // ---- устройства: подключённые и сохранённые ----
                    card(ui, |ui| {
                        section(ui, "УСТРОЙСТВА В СЕТИ");
                        let devs = self.shared.devices_snapshot();
                        if devs.is_empty() {
                            ui.label(
                                RichText::new("никого — откройте адрес на телефоне")
                                    .size(12.5)
                                    .color(pal().dim),
                            );
                        }
                        for d in &devs {
                            ui.horizontal(|ui| {
                                dot(ui, pal().neon);
                                ui.label(RichText::new(d).size(12.5).color(pal().text));
                            });
                        }

                        ui.add_space(8.0);
                        section(ui, "СОХРАНЁННЫЕ");
                        let names = self.shared.names_list();
                        if names.is_empty() {
                            ui.label(RichText::new("пока никого").size(12.5).color(pal().dim));
                        }
                        for (ip, name) in &names {
                            let mut sel = self.forget.contains(ip);
                            if ui
                                .checkbox(&mut sel, RichText::new(format!("{name}  ·  {ip}")).size(12.5))
                                .changed()
                            {
                                if sel {
                                    self.forget.insert(ip.clone());
                                } else {
                                    self.forget.remove(ip);
                                }
                            }
                        }
                        if !names.is_empty()
                            && ui
                                .add_enabled(
                                    !self.forget.is_empty(),
                                    egui::Button::new(RichText::new("удалить выбранные").size(12.5)),
                                )
                                .clicked()
                        {
                            for ip in self.forget.drain() {
                                self.shared.forget_name(&ip);
                            }
                        }
                    });
                    ui.add_space(32.0);
                    });
                });
            });

    }
}

impl App {
    /// Пилюля с названием темы и своим меню (стандартный ComboBox выбивался из вида).
    fn theme_widget(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let popup = egui::Id::unique("theme_popup");
        let resp = egui::Frame::NONE
            .fill(pal().card)
            .stroke(egui::Stroke::new(1.0_f32, pal().line))
            .corner_radius(14)
            .inner_margin(egui::Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("тема: {}", THEMES[self.theme].0))
                            .size(12.5)
                            .color(pal().text),
                    );
                });
            })
            .response
            .interact(egui::Sense::click());
        if resp.hovered() {
            ui.painter()
                .rect_stroke(resp.rect, 14, egui::Stroke::new(1.0_f32, pal().neon), egui::StrokeKind::Inside);
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let mut pick = None;
        // якорь сдвинут влево: правый край меню не заходит на полосу прокрутки
        let w = 168.0_f32.max(resp.rect.width());
        let shift = 40.0_f32;
        let anchor = egui::Rect::from_min_size(
            egui::pos2(resp.rect.right() - w - shift, resp.rect.top()),
            egui::vec2(w, resp.rect.height()),
        );
        egui::Popup::from_toggle_button_response(&resp)
            .id(popup)
            .anchor(anchor)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
            ui.set_min_width(w - 12.0);
            for (i, (name, _)) in THEMES.iter().enumerate() {
                let row = ui
                    .horizontal(|ui| {
                        dot(ui, ACCENT[i]);
                        let _ = ui.selectable_label(
                            i == self.theme,
                            RichText::new(*name).size(12.5).color(pal().text),
                        );
                    })
                    .response;
                // клик по всей строке, а не только по подписи
                if ui
                    .interact(row.rect, egui::Id::unique(("theme_item", i)), egui::Sense::click())
                    .clicked()
                {
                    pick = Some(i);
                }
            }
        });
        if let Some(i) = pick {
            self.theme = i;
            set_theme(i);
            style(ctx);
            save_config(self.root.as_ref(), THEMES[i].0, Some(&self.ip));
            egui::Popup::close_id(ui.ctx(), popup);
        }
    }
}

impl App {
    fn pick_folder(&mut self) {
        let mut dlg = rfd::FileDialog::new();
        if let Some(r) = &self.root {
            dlg = dlg.set_directory(r);
        }
        if let Some(p) = dlg.pick_folder() {
            *self.shared.root.lock().unwrap() = Some(p.clone());
            self.shared.note(format!("папка приёма: {}", p.display()));
            save_config(Some(&p), &THEMES[self.theme].0, Some(&self.ip));
            self.root = Some(p);
        }
    }
}

fn style(ctx: &egui::Context) {
    let mut s = (*ctx.style_of(egui::Theme::Dark)).clone();
    s.visuals = egui::Visuals::dark();
    s.visuals.panel_fill = pal().bg;
    s.visuals.window_fill = pal().bg;
    s.visuals.extreme_bg_color = pal().panel;
    s.visuals.override_text_color = Some(pal().text);
    s.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, pal().line);
    s.visuals.widgets.inactive.bg_fill = pal().card;
    s.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, pal().line);
    s.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, pal().text);
    s.visuals.widgets.hovered.bg_fill = pal().panel;
    s.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, pal().neon);
    s.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, pal().neon);
    s.visuals.widgets.active.bg_fill = pal().card;
    s.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, pal().neon);
    s.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, pal().neon);
    s.visuals.selection.bg_fill = pal().line;
    // кнопки-плитки: мягкий фон панели вместо серой заливки
    s.visuals.widgets.inactive.bg_fill = pal().panel;
    s.visuals.widgets.inactive.weak_bg_fill = pal().panel;
    s.visuals.widgets.hovered.weak_bg_fill = pal().card;
    s.visuals.widgets.active.weak_bg_fill = pal().card;
    // тонкие скруглённые скроллбары; inner+bar = 12 px зазора до карточек,
    // outer = 12 px до правой границы окна (панель без правого отступа)
    s.spacing.scroll.bar_width = 8.0;
    s.spacing.scroll.bar_inner_margin = 0.0;
    s.spacing.scroll.bar_outer_margin = 12.0;
    // плавающая полоса: место под неё не резервируется, поэтому зазоры слева и справа равны
    s.spacing.scroll.floating = true;
    // цвет ползунка берём из fg_stroke (иначе красился бы вместе с кнопками)
    s.spacing.scroll.foreground_color = true;
    s.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, pal().dim);
    s.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, pal().neon);
    // плитки: заметное, но не круглое скругление
    s.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(4);
    s.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(4);
    s.visuals.widgets.active.corner_radius = egui::CornerRadius::same(4);
    // всплывающие меню (выбор темы) — карточка с мягкой тенью, а не системный прямоугольник
    s.visuals.window_fill = pal().card;
    s.visuals.window_stroke = egui::Stroke::new(1.0_f32, pal().line);
    s.visuals.window_corner_radius = egui::CornerRadius::same(12);
    s.visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 8],
        blur: 22,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    s.visuals.popup_shadow = s.visuals.window_shadow;
    s.spacing.menu_margin = egui::Margin::same(6);
    ctx.set_style_of(egui::Theme::Dark, s);
}

/// Адрес, по которому до нас дойдёт телефон. UDP-трюк мимо: при включённом VPN
/// он отдаёт адрес виртуального адаптера, поэтому перебираем обычные локальные сети.
fn local_ips() -> Vec<String> {
    let mut cands: Vec<(u8, String)> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => {
                let name = i.name.to_ascii_lowercase();
                let virtual_nic = ["vpn", "tap", "tun", "xray", "wintun", "wsl", "vethernet",
                    "hyper-v", "virtual", "vmware", "vbox", "bluetooth", "docker", "loopback"]
                    .iter()
                    .any(|k| name.contains(k));
                let o = v4.ip.octets();
                let rank = if virtual_nic {
                    9
                } else if o[0] == 192 && o[1] == 168 {
                    0
                } else if o[0] == 10 {
                    1
                } else if o[0] == 172 && (16..32).contains(&o[1]) {
                    2
                } else if o[0] == 169 && o[1] == 254 {
                    8
                } else {
                    3
                };
                Some((rank, v4.ip.to_string()))
            }
            _ => None,
        })
        .collect();
    cands.sort();
    cands.into_iter().map(|c| c.1).collect()
}

/// Второй экземпляр: если 8765 уже отвечает нашим ping — не поднимаем второй сервер.
fn running_instance() -> bool {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 8765));
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    if s.write_all(b"GET /api/ping?id=0 HTTP/1.0\r\nHost: 127.0.0.1:8765\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = String::new();
    let _ = s.take(300).read_to_string(&mut buf);
    buf.contains(" 200 ") && buf.contains("ok")
}

/// Свободное место на диске, где лежит папка приёма.
fn free_space(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut())
    };
    (ok != 0).then_some(free)
}

#[link(name = "kernel32")]
extern "system" {
    fn GetDiskFreeSpaceExW(
        dir: *const u16,
        free_to_caller: *mut u64,
        total: *mut u64,
        total_free: *mut u64,
    ) -> i32;
}

fn open_url(url: &str) {
    let _ = std::process::Command::new("rundll32.exe")
        .arg("url.dll,FileProtocolHandler")
        .arg(url)
        .spawn();
}

/// Папка приёма по умолчанию: Загрузки\NeonSendTemp. Временные папки кладутся внутрь неё.
fn default_root() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")?;
    let dir = PathBuf::from(home).join("Downloads").join("NeonSendTemp");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("NeonSend").join("config.json"))
}

fn load_config() -> Option<(Option<PathBuf>, String, Option<String>)> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let root = v.get("root").and_then(|r| r.as_str()).map(PathBuf::from).filter(|p| p.is_dir());
    let theme = v.get("theme").and_then(|t| t.as_str()).unwrap_or("неон").to_string();
    let ip = v.get("ip").and_then(|i| i.as_str()).map(str::to_string);
    Some((root, theme, ip))
}

fn save_config(root: Option<&PathBuf>, theme: &str, ip: Option<&str>) {
    if let Some(path) = config_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = serde_json::json!({
            "root": root.map(|r| r.to_string_lossy().to_string()),
            "theme": theme,
            "ip": ip,
        })
        .to_string();
        let _ = std::fs::write(path, body);
    }
}

fn make_qr(ctx: &egui::Context, text: &str) -> Option<egui::TextureHandle> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let w = code.width();
    let colors = code.to_colors();
    const SCALE: usize = 6;
    const QUIET: usize = 4;
    let dim = (w + QUIET * 2) * SCALE;
    let mut pixels = vec![Color32::from_rgb(230, 245, 238); dim * dim];
    for y in 0..w {
        for x in 0..w {
            if colors[y * w + x] == qrcode::Color::Dark {
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        let px = (x + QUIET) * SCALE + dx;
                        let py = (y + QUIET) * SCALE + dy;
                        pixels[py * dim + px] = Color32::from_rgb(8, 20, 14);
                    }
                }
            }
        }
    }
    Some(ctx.load_texture(
        "qr",
        egui::ColorImage { size: [dim, dim], source_size: egui::vec2(dim as f32, dim as f32), pixels },
        egui::TextureOptions::NEAREST,
    ))
}

fn main() -> eframe::Result<()> {
    // release собирается без консоли: причину падения иначе не увидеть
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        if let Some(dir) = config_path().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("crash.log"))
            {
                use std::io::Write;
                let _ = writeln!(
                    f,
                    "[{}] {}\n{}\n",
                    chrono::Local::now().format("%F %T"),
                    info,
                    bt
                );
            }
        }
    }));
    let options = eframe::NativeOptions {
        viewport: {
            let vp = egui::ViewportBuilder::default()
                .with_inner_size([560.0, 940.0])
                // высота не меньше карточки устройств целиком — иначе прокрутка режет её
                .with_min_inner_size([520.0, 720.0])
                .with_title("NeonSend");
            match app_icon() {
                Some(icon) => vp.with_icon(icon),
                None => vp,
            }
        },
        // своя палитра на все темы: системная тема Windows не должна перебивать стиль
        ..Default::default()
    };

    // egui 0.27 падает в hit_test при клике по исчезающему виджету, и это происходит
    // уже после нашей отрисовки — catch_unwind внутри App не спасает. Поэтому окно
    // перезапускаем, а сервер поднят один раз и живёт дальше (передачи не рвутся).
    let b = boot();
    let mut restarts = 0;
    loop {
        let bb = b.clone();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            eframe::run_native(
                "NeonSend",
                options.clone(),
                Box::new(move |cc| Ok(Box::new(App::new(cc, bb)))),
            )
        }));
        match res {
            Ok(r) => return r,
            Err(_) if restarts < 5 => {
                restarts += 1;
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(_) => return Err(eframe::Error::AppCreation(Box::new(Crash))),
        }
    }
}

/// Заглушка, чтобы вернуть «окно падало 5 раз подряд» как ошибку eframe.
struct Crash;
impl std::fmt::Debug for Crash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("окно падало 5 раз подряд")
    }
}
impl std::fmt::Display for Crash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("окно падало 5 раз подряд")
    }
}
impl std::error::Error for Crash {}

/// Иконка окна и панели задач — тот же шестиугольник с N, что и в exe (`icon.ico`).
fn app_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../icon.ico");
    parse_ico(bytes, 64).or_else(|| parse_ico(bytes, 32))
}

/// Достаёт кадр заданной ширины из ICO (DIB 32 бита, снизу вверх, BGRA).
fn parse_ico(b: &[u8], want: usize) -> Option<egui::IconData> {
    let n = u16::from_le_bytes([*b.get(4)?, *b.get(5)?]) as usize;
    for i in 0..n {
        let e = b.get(6 + i * 16..22 + i * 16)?;
        let w = if e[0] == 0 { 256 } else { e[0] as usize };
        if w != want {
            continue;
        }
        let size = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
        let off = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        let img = b.get(off..off + size)?;
        let h = u32::from_le_bytes([img[8], img[9], img[10], img[11]]) as usize / 2;
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in (0..h).rev() {
            let row = img.get(40 + y * w * 4..40 + (y + 1) * w * 4)?;
            for x in 0..w {
                let p = &row[x * 4..x * 4 + 4];
                rgba.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
            }
        }
        return Some(egui::IconData { rgba, width: w as u32, height: h as u32 });
    }
    None
}
