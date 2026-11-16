use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, State};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;

struct Dev {
    label: String,
    named: bool,
    last: Instant,
    tx: broadcast::Sender<String>,
    room: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ReqState {
    Wait,
    Ok,
    Done,
    Abort,
}

impl ReqState {
    fn as_str(self) -> &'static str {
        match self {
            ReqState::Wait => "wait",
            ReqState::Ok => "ok",
            ReqState::Done => "done",
            ReqState::Abort => "abort",
        }
    }
}

/// Состояние передачи живёт на сервере: страница только показывает его,
/// поэтому с любого браузера видно то же самое.
struct Req {
    from: u64,
    to: u64,
    state: ReqState,
    files: u64,
    bytes: u64,
    files_done: u64,
    bytes_done: u64,
    err: String,
    since: Instant,
    items: Vec<(String, u64)>,
    /// что отправитель собирается прислать: показывается получателю до загрузки
    planned: Vec<(String, u64)>,
    from_label: String,
    to_label: String,
    /// папка-релей: Temp(отправитель-получатель)
    temp: String,
    sender_done: bool,
    /// текст, который отправитель прислал вместо файлов
    text: String,
}

/// Общее состояние: приложение (UI) и HTTP-сервер видят одни и те же данные.
pub struct Shared {
    pub root: Mutex<Option<PathBuf>>,
    pub log: Mutex<Vec<String>>,
    pub received: Mutex<(u64, u64)>, // файлов, байт
    devices: Mutex<HashMap<u64, Dev>>,
    by_ip: Mutex<HashMap<String, u64>>,
    by_token: Mutex<HashMap<String, u64>>,
    /// токен браузера -> имя: Safari на телефоне теряет SSE в фоне, а с ним и устройство,
    /// имя по IP тогда уже не найти (адрес мог смениться) — узнаём по токену
    tok_names: Mutex<HashMap<String, String>>,
    reqs: Mutex<HashMap<u64, Req>>,
    next: AtomicU64,
    /// адрес сервера: браузер, пришедший с него, и есть «Этот компьютер»
    local: Mutex<String>,
    /// ip -> имя устройства, чтобы не спрашивать имя при следующем заходе
    names: Mutex<HashMap<String, String>>,
}

/// ПК (окно приложения) в списке устройств.
pub const PC_ID: u64 = 0;

/// Мьютекс не отравляется паникой: иначе один сбой навсегда ломает все запросы.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Устройство, которому принадлежит токен запроса (ставит middleware auth).
#[derive(Clone, Copy)]
struct DevId(u64);

fn qparam<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            root: Mutex::new(None),
            log: Mutex::new(Vec::new()),
            received: Mutex::new((0, 0)),
            devices: Mutex::new(HashMap::new()),
            by_ip: Mutex::new(HashMap::new()),
            by_token: Mutex::new(HashMap::new()),
            tok_names: Mutex::new(HashMap::new()),
            reqs: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
            local: Mutex::new(String::new()),
            names: Mutex::new(load_names()),
        })
    }

    fn new_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Устройство — это адрес в сети: перезагрузка страницы или другой браузер
    /// на том же телефоне остаются тем же устройством.
    fn lookup_by_ip(&self, ip: &str) -> Option<u64> {
        let id = *lock(&self.by_ip).get(ip)?;
        lock(&self.devices).contains_key(&id).then_some(id)
    }

    /// Забываем устройства, которые давно не подавали признаков жизни.
    fn prune(&self) {
        let gone: Vec<(u64, String, Option<u64>)> = {
            let mut devs = lock(&self.devices);
            let gone: Vec<(u64, String, Option<u64>)> = devs
                .iter()
                // вкладка в фоне может не слать запросы, но живой SSE — доказательство, что она открыта
                .filter(|(_, d)| d.last.elapsed() > Duration::from_secs(9) && d.tx.receiver_count() == 0)
                .map(|(id, d)| (*id, d.label.clone(), d.room))
                .collect();
            for (id, _, _) in &gone {
                devs.remove(id);
            }
            gone
        };
        if gone.is_empty() {
            return;
        }
        {
            let mut ips = lock(&self.by_ip);
            ips.retain(|_, id| !gone.iter().any(|(gid, _, _)| gid == id));
        }
        {
            let mut toks = lock(&self.by_token);
            toks.retain(|_, id| !gone.iter().any(|(gid, _, _)| gid == id));
        }
        for (id, label, room) in gone {
            self.note(format!("устройство отключилось: {label}"));
            // отправитель закрыл страницу — передачу продолжать некому
            if let Some(rid) = room {
                let msg = {
                    let mut reqs = lock(&self.reqs);
                    match reqs.get_mut(&rid) {
                        Some(req) if req.from == id && req.state != ReqState::Done => {
                            req.state = ReqState::Abort;
                            req.err = "отправитель закрыл страницу".into();
                            req.since = Instant::now();
                            Some((req.to, state_event(rid, req)))
                        }
                        _ => None,
                    }
                };
                if let Some((to, json)) = msg {
                    self.send(to, &json);
                }
            }
        }
    }

    fn send(&self, id: u64, json: &str) {
        if let Some(d) = lock(&self.devices).get(&id) {
            let _ = d.tx.send(json.to_string());
        }
    }

    fn alive(&self, id: u64) -> Option<String> {
        let devs = lock(&self.devices);
        devs.get(&id).map(|d| d.label.clone())
    }

    fn touch(&self, id: u64) {
        if let Some(d) = lock(&self.devices).get_mut(&id) {
            d.last = Instant::now();
        }
    }

    /// Переводит комнату в новое состояние и рассказывает об этом обеим сторонам.
    fn finish_room(&self, rid: u64, new: ReqState, err: &str) -> bool {
        {
            let mut reqs = lock(&self.reqs);
            match reqs.get_mut(&rid) {
                Some(r) if r.state != ReqState::Done || new != ReqState::Abort => {
                    r.state = new;
                    r.err = err.to_string();
                    r.since = Instant::now();
                }
                _ => return false,
            }
        }
        self.push_room(rid);
        if new == ReqState::Done {
            // передача закончена: устройства свободны и могут начать новую
            self.detach_room(rid);
        } else if new == ReqState::Abort {
            // недокачанные .part после отмены уже никому не нужны
            self.cleanup_parts(rid);
        }
        true
    }

    /// Отвязывает комнату от обеих сторон: страница возвращается к выбору устройства.
    fn detach_room(&self, rid: u64) {
        let mut devs = lock(&self.devices);
        for d in devs.values_mut() {
            if d.room == Some(rid) {
                d.room = None;
            }
        }
    }

    fn cleanup_parts(&self, rid: u64) {
        let (root, temp) = {
            let root = lock(&self.root).clone();
            let reqs = lock(&self.reqs);
            match (root, reqs.get(&rid)) {
                (Some(root), Some(r)) => (root, r.temp.clone()),
                _ => return,
            }
        };
        remove_parts(&root.join(temp));
    }

    /// Передача на этот компьютер: файлы уже в папке приёма, ждать получателя нечего.
    fn maybe_finish(&self, rid: u64) {
        let done = {
            let reqs = lock(&self.reqs);
            matches!(reqs.get(&rid), Some(r)
                if r.to == PC_ID
                    && r.state == ReqState::Ok
                    && r.sender_done
                    // число файлов может не прийти (нет push) — тогда верим sender_done
                    && (r.files == 0 || r.files_done >= r.files))
        };
        if done && self.finish_room(rid, ReqState::Done, "") {
            self.note("передача завершена");
        }
    }

    /// Незавершённые передачи для панели окна: кто, кому, состояние, готово/всего файлов.
    pub fn transfers(&self) -> Vec<(String, String, &'static str, u64, u64)> {
        let reqs = lock(&self.reqs);
        let mut v: Vec<(Instant, String, String, &'static str, u64, u64)> = reqs
            .iter()
            .filter(|(_, r)| matches!(r.state, ReqState::Wait | ReqState::Ok))
            .map(|(_, r)| {
                (
                    r.since,
                    r.from_label.clone(),
                    r.to_label.clone(),
                    r.state.as_str(),
                    r.files_done,
                    r.files,
                )
            })
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v.into_iter().map(|(_, a, b, c, d, e)| (a, b, c, d, e)).collect()
    }

    /// Сколько занимают временные папки в папке приёма.
    pub fn temp_size(&self) -> u64 {
        let root = match lock(&self.root).clone() {
            Some(r) => r,
            None => return 0,
        };
        let mut total = 0;
        if let Ok(rd) = std::fs::read_dir(&root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() && e.file_name().to_string_lossy().starts_with("Temp(") {
                    total += dir_size(&p);
                }
            }
        }
        total
    }

    fn push_room(&self, rid: u64) {
        let (from, to, json) = {
            let reqs = lock(&self.reqs);
            match reqs.get(&rid) {
                Some(r) => (r.from, r.to, state_event(rid, r)),
                None => return,
            }
        };
        self.send(from, &json);
        if to != from {
            self.send(to, &json);
        }
    }

    /// Подписи устройств на связи (для окна приложения).
    pub fn devices_snapshot(&self) -> Vec<String> {
        self.prune();
        let mut list: Vec<String> = self
            .devices
            .lock()
            .unwrap()
            .values()
            .map(|d| d.label.clone())
            .collect();
        list.sort();
        list
    }

    /// Устройства, занятые активной комнатой: id устройства → id комнаты.
    fn busy_ids(&self) -> std::collections::HashMap<u64, u64> {
        let mut out = std::collections::HashMap::new();
        for (rid, r) in lock(&self.reqs).iter() {
            if matches!(r.state, ReqState::Wait | ReqState::Ok) {
                out.insert(r.from, *rid);
                out.insert(r.to, *rid);
            }
        }
        out
    }

    fn devices_json(&self, exclude: u64) -> String {
        let busy = self.busy_ids();
        let exclude_rid = busy.get(&exclude).copied();
        let mut list: Vec<(String, String)> = self
            .devices
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, d)| **id != exclude && d.named)
            .map(|(id, d)| {
                // занято, если устройство в другой комнате (своя текущая — не в счёт)
                let b = matches!(busy.get(id), Some(rid) if Some(*rid) != exclude_rid);
                (
                    d.label.to_lowercase(),
                    format!(
                        "{{\"id\":{id},\"label\":\"{}\",\"busy\":{b}}}",
                        esc(&d.label)
                    ),
                )
            })
            .collect();
        list.sort();
        list.into_iter().map(|(_, j)| j).collect::<Vec<_>>().join(",")
    }

    /// Идёт ли передача: есть незавершённая комната.
    pub fn busy(&self) -> bool {
        self.reqs
            .lock()
            .unwrap()
            .values()
            .any(|r| matches!(r.state, ReqState::Wait | ReqState::Ok))
    }

    /// Удаляет все временные папки комнат (`Temp(от-кому)`) из папки приёма.
    pub fn delete_temps(&self) -> usize {
        let root = match lock(&self.root).clone() {
            Some(r) => r,
            None => return 0,
        };
        let mut n = 0;
        if let Ok(rd) = std::fs::read_dir(&root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir()
                    && e.file_name().to_string_lossy().starts_with("Temp(")
                    && std::fs::remove_dir_all(&p).is_ok()
                {
                    n += 1;
                }
            }
        }
        self.note(format!("удалено временных папок: {n}"));
        n
    }

    pub fn note(&self, text: impl Into<String>) {
        let now = chrono::Local::now();
        let (h, m, s) = (
            chrono::Timelike::hour(&now),
            chrono::Timelike::minute(&now),
            chrono::Timelike::second(&now),
        );
        let mut log = lock(&self.log);
        log.push(format!("{h:02}:{m:02}:{s:02}  {}", text.into()));
        if log.len() > 500 {
            log.drain(0..200);
        }
    }
}

/// Имя папки-релея: Temp(отправитель-получатель-комната) — чтобы устройства
/// и две передачи между теми же устройствами не смешивались.
fn temp_name(from: &str, to: &str, rid: u64) -> String {
    let clean = |s: &str| -> String {
        let mut out = String::new();
        for ch in s.chars() {
            if (ch as u32) < 0x20 || matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                out.push('_');
            } else {
                out.push(ch);
            }
        }
        let out = out.trim().to_string();
        if out.is_empty() { "устройство".into() } else { out }
    };
    format!("Temp({}-{}-{rid})", clean(from), clean(to))
}

/// Размер папки со всем содержимым.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    for e in rd.flatten() {
        match e.metadata() {
            Ok(md) if md.is_dir() => total += dir_size(&e.path()),
            Ok(md) => total += md.len(),
            Err(_) => {}
        }
    }
    total
}

/// Удаляет недокачанные файлы `*.part` в папке и её подпапках.
fn remove_parts(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            remove_parts(&p);
        } else if p.extension().and_then(|x| x.to_str()) == Some("part") {
            let _ = std::fs::remove_file(&p);
        }
    }
}

fn names_file() -> Option<PathBuf> {    std::env::var("APPDATA")
        .ok()
        .map(|a| PathBuf::from(a).join("NeonSend").join("devices.json"))
}

fn load_names() -> HashMap<String, String> {
    names_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_names(map: &HashMap<String, String>) {
    if let Some(p) = names_file() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, serde_json::to_string(map).unwrap_or_default());
    }
}

impl Shared {
    /// Имя, которое устройство с этим адресом уже называло в прошлый раз.
    fn known_name(&self, ip: &str) -> Option<String> {
        lock(&self.names).get(ip).cloned()
    }

    /// Имя, запомненное за токеном браузера (адрес мог смениться после ухода в фон).
    fn tok_name(&self, token: &str) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        lock(&self.tok_names).get(token).cloned()
    }

    /// Сохранённые имена устройств: (адрес, имя), по имени.
    pub fn names_list(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .names
            .lock()
            .unwrap()
            .iter()
            .map(|(ip, n)| (ip.clone(), n.clone()))
            .collect();
        v.sort_by(|a, b| a.1.cmp(&b.1));
        v
    }

    /// Забыть имя: при следующем заходе устройство спросят заново.
    pub fn forget_name(&self, ip: &str) {
        let mut names = lock(&self.names);
        let gone = names.remove(ip);
        save_names(&names);
        drop(names);
        if let Some(name) = gone {
            let id = lock(&self.by_ip).get(ip).copied();
            if let Some(id) = id {
                if let Some(d) = lock(&self.devices).get_mut(&id) {
                    d.named = false;
                }
            }
            self.note(format!("забыто устройство: {name}"));
        }
    }

    /// Запоминает имя устройства по адресу, с которого пришёл запрос. false — это чужое устройство.
    fn set_name(&self, id: u64, name: &str, ip: &str) -> bool {
        let name = clean_label(name);
        if lock(&self.by_ip).get(ip).copied() != Some(id) {
            return false;
        }
        {
            let mut devs = lock(&self.devices);
            if let Some(d) = devs.get_mut(&id) {
                d.label = name.clone();
                d.named = true;
            }
        }
        {
            let mut names = lock(&self.names);
            names.insert(ip.to_string(), name.clone());
            save_names(&names);
        }
        // имя нужно помнить и на токен: в фоне Safari устройство теряет SSE
        if let Some(token) = self
            .by_token
            .lock()
            .unwrap()
            .iter()
            .find(|(_, &v)| v == id)
            .map(|(k, _)| k.clone())
        {
            lock(&self.tok_names).insert(token, name.clone());
        }
        self.note(format!("устройство назвалось: {name}"));
        true
    }

    /// Адрес сервера: браузер на этом компьютере — «Этот компьютер».
    pub fn set_local(&self, ip: &str) {
        *lock(&self.local) = ip.to_string();
    }

    fn is_local(&self, ip: &str) -> bool {
        // loopback — это точно этот компьютер, даже если показываем другой адрес
        *lock(&self.local) == ip || ip == "127.0.0.1" || ip == "::1"
    }

    fn apply_answer(&self, rid: u64, accept: bool) -> bool {
        {
            let mut reqs = lock(&self.reqs);
            match reqs.get_mut(&rid) {
                Some(r) if r.state == ReqState::Wait => {
                    if accept {
                        r.state = ReqState::Ok;
                    } else {
                        r.state = ReqState::Abort;
                        r.err = "получатель отклонил передачу".into();
                    }
                    r.since = Instant::now();
                }
                _ => return false,
            }
        }
        self.push_room(rid);
        self.note(if accept {
            "получатель принял передачу"
        } else {
            "получатель отклонил передачу"
        });
        true
    }
}

#[derive(Deserialize)]
struct PartQ {
    name: String,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    req: Option<u64>,
}

#[derive(Deserialize)]
struct UpQuery {
    name: String,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    req: Option<u64>,
    /// 0 — пропустить существующий, 1 — заменить, 2 — переименовать
    #[serde(default)]
    ow: Option<u8>,
    /// куски большого файла: смещение и признак последнего
    #[serde(default)]
    off: Option<u64>,
    #[serde(default)]
    last: Option<u8>,
}

pub fn start(ip: &str, port: u16, state: Arc<Shared>) -> Result<(), String> {
    state.set_local(ip);
    // слушаем все интерфейсы: адрес в QR можно менять без перезапуска сервера
    let listener = std::net::TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("{e}"))?;
    listener.set_nonblocking(true).map_err(|e| format!("{e}"))?;
    let origin = Origin { port };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/ping", get(ping))
        .route("/api/received", get(received))
        .route("/api/events", get(events))
        .route("/api/state", get(state_info))
        .route("/api/devices", get(devices))
        .route("/api/request", post(request))
        .route("/api/answer", post(answer))
        .route("/api/push", post(push))
        .route("/api/text", post(send_text))
        .route("/api/sender_done", post(sender_done))
        .route("/api/leave", post(leave))
        .route("/api/name", post(set_name_h))
        .route("/api/progress", post(progress))
        .route("/api/abort", post(abort))
        .route("/api/done", post(done))
        .route("/api/clear", post(clear))
        .route("/api/upload", post(upload))
        .route("/api/exists", post(exists))
        .route("/api/part", get(part_size))
        .route("/dl", get(download))
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth))
        .layer(axum::middleware::from_fn_with_state(origin, guard))
        .layer(axum::middleware::from_fn(headers))
        .layer(DefaultBodyLimit::disable())
        .with_state(state.clone());

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                state.note(format!("ошибка запуска: {e}"));
                return;
            }
        };
        let handle = rt.handle().clone();
        let sweep = state.clone();
        // подметаем старые завершённые комнаты, чтобы память не текла
        handle.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let old: Vec<u64> = {
                    let reqs = lock(&sweep.reqs);
                    reqs.iter()
                        .filter(|(_, r)| {
                            (r.state == ReqState::Done || r.state == ReqState::Abort)
                                && r.since.elapsed() > Duration::from_secs(600)
                        })
                        .map(|(id, _)| *id)
                        .collect()
                };
                if !old.is_empty() {
                    {
                        let mut reqs = lock(&sweep.reqs);
                        for id in old.iter() {
                            reqs.remove(id);
                        }
                    }
                    {
                        let mut devs = lock(&sweep.devices);
                        for id in old.iter() {
                            for d in devs.values_mut() {
                                if d.room == Some(*id) {
                                    d.room = None;
                                }
                            }
                        }
                    }
                }
            }
        });
        rt.block_on(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => l,
                Err(e) => {
                    state.note(format!("ошибка сокета: {e}"));
                    return;
                }
            };
            if let Err(e) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                state.note(format!("сервер остановлен: {e}"));
            }
        });
    });
    Ok(())
}

async fn index() -> impl IntoResponse {
    let mut res = Response::new(Body::from(include_str!("page.html")));
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn received(State(st): State<Arc<Shared>>) -> String {
    let (files, bytes) = *lock(&st.received);
    format!("{{\"files\":{files},\"bytes\":{bytes}}}")
}

#[derive(Deserialize)]
struct PingQ {
    #[serde(default)]
    id: Option<u64>,
}

async fn ping(State(st): State<Arc<Shared>>, Query(q): Query<PingQ>) -> &'static str {
    if let Some(id) = q.id {
        st.touch(id);
    }
    "ok"
}

#[derive(Deserialize)]
struct HelloQ {
    #[serde(default)]
    name: String,
    /// токен браузера из localStorage: устройство узнаётся после перезагрузки страницы
    #[serde(default)]
    token: String,
}

/// Поток событий устройства (SSE): первым приходит hello с его номером.
async fn events(
    State(st): State<Arc<Shared>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(q): Query<HelloQ>,
) -> Response<Body> {
    st.prune();
    let suggest = clean_label(&q.name);
    let ip = addr.ip().to_string();
    let known = st
        .known_name(&ip)
        .or_else(|| st.tok_name(&q.token));
    let named = known.is_some();
    let label = known.unwrap_or_else(|| suggest.clone());
    let dev_id = if st.is_local(&ip) {
        // браузер на этом же компьютере — и есть «Этот компьютер»; окно приложения
        // ничего не подтверждает, файлы принимаются здесь же, в папку приёма
        if !lock(&st.devices).contains_key(&PC_ID) {
            st.note(format!("подключилось устройство: {label}"));
        }
        lock(&st.by_ip).insert(ip, PC_ID);
        // токен нужен и «этому компьютеру»: по нему проверяются все запросы к API
        if !q.token.is_empty() {
            lock(&st.by_token).insert(q.token.clone(), PC_ID);
        }
        PC_ID
    } else {
        // тот же браузер после перезагрузки страницы узнаётся по токену, IP — запасной признак
        let by_token = if q.token.is_empty() {
            None
        } else {
            lock(&st.by_token).get(&q.token).copied()
        };
        let known_id = match by_token {
            Some(id) if lock(&st.devices).contains_key(&id) => Some(id),
            _ => st.lookup_by_ip(&ip),
        };
        let id = match known_id {
            Some(id) => id,
            None => {
                let id = st.new_id();
                st.note(format!("подключилось устройство: {label}"));
                id
            }
        };
        lock(&st.by_ip).insert(ip, id);
        if !q.token.is_empty() {
            lock(&st.by_token).insert(q.token.clone(), id);
            // имя запоминаем на токен: в фоне Safari теряет SSE, и по IP оно уже не найдётся
            if named {
                st.tok_names
                    .lock()
                    .unwrap()
                    .insert(q.token.clone(), label.clone());
            }
        }
        id
    };

    // один канал на устройство: все вкладки и браузеры видят одни и те же события
    let rx = {
        let mut devs = lock(&st.devices);
        match devs.get_mut(&dev_id) {
            Some(d) => {
                if named {
                    d.label = label.clone();
                }
                d.named = d.named || named;
                d.last = Instant::now();
                d.tx.subscribe()
            }
            None => {
                let (tx, rx) = broadcast::channel::<String>(64);
                devs.insert(
                    dev_id,
                    Dev {
                        label: label.clone(),
                        named,
                        last: Instant::now(),
                        tx,
                        room: None,
                    },
                );
                rx
            }
        }
    };

    let hello = format!(
        "{{\"t\":\"hello\",\"id\":{dev_id},\"named\":{named},\"suggest\":\"{} · №{dev_id}\"}}",
        esc(&suggest)
    );
    let stream = futures_util::stream::unfold((rx, Some(hello)), move |(mut rx, first)| async move {
        if let Some(h) = first {
            return Some((Ok::<String, std::io::Error>(sse(&h)), (rx, None)));
        }
        loop {
            match rx.recv().await {
                Ok(msg) => return Some((Ok(sse(&msg)), (rx, None))),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    let mut res = Response::new(Body::from_stream(stream));
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn sse(json: &str) -> String {
    format!("data: {json}\n\n")
}

#[derive(Deserialize)]
struct DevQ {
    id: u64,
}

async fn devices(State(st): State<Arc<Shared>>, Query(q): Query<DevQ>) -> String {
    st.prune();
    st.touch(q.id);
    format!("[{}]", st.devices_json(q.id))
}

#[derive(Deserialize)]
struct ReqQ {
    from: u64,
    to: u64,
    files: u64,
}

async fn request(State(st): State<Arc<Shared>>, Query(q): Query<ReqQ>) -> String {
    st.prune();
    let from_label = match st.alive(q.from) {
        Some(l) => l,
        None => return "{\"error\":\"нет устройства-отправителя\"}".into(),
    };
    let to_label = match st.alive(q.to) {
        Some(l) => l,
        None => return "{\"error\":\"устройство не на связи\"}".into(),
    };
    if q.from == q.to {
        return "{\"error\":\"нельзя передать самому себе\"}".into();
    }
    // потолок на число комнат: мусорные запросы не должны съедать память
    if lock(&st.reqs).len() >= 64 {
        return "{\"error\":\"слишком много передач — попробуйте позже\"}".into();
    }
    // занятое устройство не берём: у него уже идёт передача
    {
        let busy = st.busy_ids();
        if busy.contains_key(&q.from) {
            return "{\"error\":\"вы уже в передаче — завершите её\"}".into();
        }
        if busy.contains_key(&q.to) {
            return "{\"error\":\"устройство занято другой передачей\"}".into();
        }
    }
    let id = st.new_id();
    let temp = temp_name(&from_label, &to_label, id);
    lock(&st.reqs).insert(
        id,
        Req {
            from: q.from,
            to: q.to,
            state: ReqState::Wait,
            files: q.files,
            bytes: 0,
            files_done: 0,
            bytes_done: 0,
            err: String::new(),
            since: Instant::now(),
            items: Vec::new(),
            planned: Vec::new(),
            from_label: from_label.clone(),
            to_label: to_label.clone(),
            temp,
            sender_done: false,
            text: String::new(),
        },
    );
    {
        let mut devs = lock(&st.devices);
        if let Some(d) = devs.get_mut(&q.from) {
            d.room = Some(id);
        }
        if let Some(d) = devs.get_mut(&q.to) {
            d.room = Some(id);
        }
    }
    st.send(
        q.to,
        &format!(
            "{{\"t\":\"req\",\"id\":{id},\"from\":\"{}\",\"files\":{}}}",
            esc(&from_label),
            q.files
        ),
    );
    st.push_room(id);
    st.note(format!("запрос: {} файлов → {to_label}", q.files));

    let st2 = st.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            let waiting = {
                let reqs = lock(&st2.reqs);
                match reqs.get(&id) {
                    Some(r) if r.state == ReqState::Wait => r.since.elapsed() < Duration::from_secs(180),
                    _ => return,
                }
            };
            if !waiting {
                break;
            }
        }
        if st2.finish_room(id, ReqState::Abort, "никто не ответил") {
            st2.note("никто не ответил на запрос — отменено");
        }
    });

    format!("{{\"id\":{id}}}")
}

#[derive(Deserialize)]
struct NameQ {
    id: u64,
    name: String,
}

/// Устройство называет себя: имя запоминается по адресу и больше не спрашивается.
async fn set_name_h(
    State(st): State<Arc<Shared>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(q): Query<NameQ>,
) -> String {
    if q.name.trim().is_empty() {
        return "{\"error\":\"пустое имя\"}".into();
    }
    // имя пишется на адрес того, кто его прислал: чужое устройство так не переименовать
    if !st.set_name(q.id, &q.name, &addr.ip().to_string()) {
        return "{\"error\":\"это не ваше устройство\"}".into();
    }
    "{\"ok\":true}".into()
}

#[derive(Deserialize)]
struct TextQ {
    id: u64,
    req: u64,
}

/// Отправитель присылает текст: получатель видит его на экране приёма и копирует кнопкой.
async fn send_text(
    State(st): State<Arc<Shared>>,
    Query(q): Query<TextQ>,
    req: axum::extract::Request,
) -> String {
    let body = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return "{\"error\":\"текст слишком длинный (до 64 КБ)\"}".into(),
    };
    let raw = String::from_utf8_lossy(&body);
    // управляющие символы (кроме перевода строки и табуляции) выкидываем
    let text: String = raw
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || (*c as u32) >= 0x20)
        .take(20_000)
        .collect();
    if text.trim().is_empty() {
        return "{\"error\":\"пустой текст\"}".into();
    }
    let ok = {
        let mut reqs = lock(&st.reqs);
        match reqs.get_mut(&q.req) {
            Some(r) if r.from == q.id && matches!(r.state, ReqState::Wait | ReqState::Ok) => {
                r.text = text;
                true
            }
            _ => false,
        }
    };
    if !ok {
        return "{\"error\":\"передача не разрешена\"}".into();
    }
    st.push_room(q.req);
    "{\"ok\":true}".into()
}

#[derive(Deserialize)]
struct PushQ {
    id: u64,
    req: u64,
    files: u64,
    bytes: u64,
}

/// Отправитель выбрал файлы и жмёт «отправить»: у получателя появляется «принять файлы».
/// В теле — список имён с размерами (необязателен: он нужен только для показа до загрузки).
async fn push(
    State(st): State<Arc<Shared>>,
    Query(q): Query<PushQ>,
    req: axum::extract::Request,
) -> String {
    let planned: Vec<(String, u64)> = {
        #[derive(Deserialize)]
        struct Item {
            name: String,
            #[serde(default)]
            size: u64,
        }
        match axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024).await {
            Ok(b) if !b.is_empty() => serde_json::from_slice::<Vec<Item>>(&b)
                .map(|v| v.into_iter().take(2000).map(|i| (i.name, i.size)).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    };
    {
        let mut reqs = lock(&st.reqs);
        match reqs.get_mut(&q.req) {
            Some(r) if r.from == q.id && matches!(r.state, ReqState::Wait | ReqState::Ok) => {
                r.files = q.files;
                r.bytes = q.bytes;
                if !planned.is_empty() {
                    r.planned = planned;
                }
                // добавили ещё файлы к уже отправленным — передача продолжается
                r.sender_done = false;
                r.since = Instant::now();
            }
            _ => return "{\"error\":\"запрос уже неактуален\"}".into(),
        }
    }
    st.push_room(q.req);
    "{\"ok\":true}".into()
}

/// Отправитель загрузил все файлы на сервер: у получателя включается «Загрузить всё».
async fn sender_done(State(st): State<Arc<Shared>>, Query(q): Query<ByIdQ>) -> String {
    {
        let mut reqs = lock(&st.reqs);
        match reqs.get_mut(&q.req) {
            Some(r) if r.from == q.id => {
                r.sender_done = true;
                r.since = Instant::now();
            }
            _ => return "{\"error\":\"нет такой передачи\"}".into(),
        }
    }
    st.push_room(q.req);
    st.maybe_finish(q.req);
    "{\"ok\":true}".into()
}



/// Выход из сессии: другая сторона видит сообщение и останавливается.
async fn leave(State(st): State<Arc<Shared>>, Query(q): Query<ByIdQ>) -> String {
    let (is_from, label) = {
        let reqs = lock(&st.reqs);
        match reqs.get(&q.req) {
            Some(r) if r.from == q.id || r.to == q.id => (
                r.from == q.id,
                if r.from == q.id { r.from_label.clone() } else { r.to_label.clone() },
            ),
            _ => return "{\"ok\":true}".into(),
        }
    };
    let msg = if is_from {
        "отправитель вышел из сессии"
    } else {
        "получатель вышел из сессии"
    };
    st.finish_room(q.req, ReqState::Abort, msg);
    st.note(format!("{label}: {msg}"));
    "{\"ok\":true}".into()
}

#[derive(Deserialize)]
struct AnsQ {
    id: u64,
    accept: u8,
}

async fn answer(
    State(st): State<Arc<Shared>>,
    axum::extract::Extension(DevId(dev)): axum::extract::Extension<DevId>,
    Query(q): Query<AnsQ>,
) -> String {
    // согласие даёт только получатель этой комнаты
    let to = lock(&st.reqs).get(&q.id).map(|r| r.to);
    if to != Some(dev) {
        return "{\"error\":\"это не ваша передача\"}".into();
    }
    if st.apply_answer(q.id, q.accept == 1) {
        "{\"ok\":true}".into()
    } else {
        "{\"error\":\"запрос уже неактуален\"}".into()
    }
}

#[derive(Deserialize)]
struct ProgQ {
    id: u64,
    req: u64,
    files: u64,
    bytes: u64,
    files_done: u64,
    bytes_done: u64,
}

/// Прогресс сообщает отправитель: состояние одно на все экраны устройства.
async fn progress(State(st): State<Arc<Shared>>, Query(q): Query<ProgQ>) -> String {
    {
        let mut reqs = lock(&st.reqs);
        if let Some(r) = reqs.get_mut(&q.req) {
            if r.from == q.id && r.state != ReqState::Done && r.state != ReqState::Abort {
                r.files = q.files;
                r.bytes = q.bytes;
                r.files_done = q.files_done.min(q.files);
                r.bytes_done = q.bytes_done.min(q.bytes);
            }
        }
    }
    st.push_room(q.req);
    st.maybe_finish(q.req);
    "{\"ok\":true}".into()
}

#[derive(Deserialize)]
struct ByIdQ {
    id: u64,
    req: u64,
}

/// Отмена из любого браузера: комната остаётся видимой со статусом «прервано».
async fn abort(State(st): State<Arc<Shared>>, Query(q): Query<ByIdQ>) -> String {
    let owner = lock(&st.devices).get(&q.id).and_then(|d| d.room);
    if owner == Some(q.req) {
        st.finish_room(q.req, ReqState::Abort, "отменено");
        st.note("передача отменена");
    }
    "{\"ok\":true}".into()
}

async fn done(State(st): State<Arc<Shared>>, Query(q): Query<ByIdQ>) -> String {
    // завершить комнату может только её участник
    let mine = lock(&st.reqs)
        .get(&q.req)
        .map(|r| r.from == q.id || r.to == q.id)
        .unwrap_or(false);
    if mine && st.finish_room(q.req, ReqState::Done, "") {
        st.note("передача завершена");
    }
    "{\"ok\":true}".into()
}

/// Страница убирает завершённую комнату с экрана.
async fn clear(State(st): State<Arc<Shared>>, Query(q): Query<ByIdQ>) -> Response<Body> {
    let mine = {
        let mut devs = lock(&st.devices);
        match devs.get_mut(&q.id) {
            Some(d) if d.room == Some(q.req) => {
                d.room = None;
                true
            }
            _ => false,
        }
    };
    if mine {
        lock(&st.reqs).remove(&q.req);
    }
    let mut res = Response::new(Body::from("{\"ok\":true}".to_string()));
    res.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    res
}

#[derive(Deserialize)]
struct IdQ {
    id: u64,
}

/// Что сейчас с устройством: список соседей и его передача (после перезагрузки вкладки).
async fn state_info(State(st): State<Arc<Shared>>, Query(q): Query<IdQ>) -> String {
    st.prune();
    st.touch(q.id);
    let devs = st.devices_json(q.id);
    let (named, label) = {
        let d = lock(&st.devices);
        match d.get(&q.id) {
            Some(d) => (d.named, d.label.clone()),
            None => (false, String::new()),
        }
    };
    let room = {
        let rid = lock(&st.devices).get(&q.id).and_then(|d| d.room);
        match rid {
            Some(id) => st
                .reqs
                .lock()
                .unwrap()
                .get(&id)
                .map(|r| room_json(id, r))
                .unwrap_or_else(|| "null".into()),
            None => "null".into(),
        }
    };
    format!(
        "{{\"me\":{},\"named\":{named},\"suggest\":\"{}\",\"devices\":[{}],\"room\":{}}}",
        q.id,
        esc(&label),
        devs,
        room
    )
}

fn room_json(rid: u64, r: &Req) -> String {
    let list = |v: &[(String, u64)]| -> String {
        v.iter()
            .map(|(n, s)| format!("{{\"name\":\"{}\",\"size\":{s}}}", esc(n)))
            .collect::<Vec<_>>()
            .join(",")
    };
    let items = list(&r.items);
    let planned = list(&r.planned);
    format!(
        "{{\"id\":{rid},\"from\":{},\"to\":{},\"state\":\"{}\",\"files\":{},\"bytes\":{},\"files_done\":{},\"bytes_done\":{},\"err\":\"{}\",\"items\":[{}],\"planned\":[{}],\"from_label\":\"{}\",\"to_label\":\"{}\",\"temp\":\"{}\",\"sender_done\":{},\"text\":\"{}\"}}",
        r.from,
        r.to,
        r.state.as_str(),
        r.files,
        r.bytes,
        r.files_done,
        r.bytes_done,
        esc(&r.err),
        items,
        planned,
        esc(&r.from_label),
        esc(&r.to_label),
        esc(&r.temp),
        r.sender_done,
        esc_text(&r.text)
    )
}

fn state_event(rid: u64, r: &Req) -> String {
    format!("{{\"t\":\"state\",\"room\":{}}}", room_json(rid, r))
}

/// Принимает один файл: ?name=<относительный путь>. Тело — сырые байты.
fn part_of(target: &Path) -> PathBuf {
    target.with_file_name(format!(
        "{}.part",
        target.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ))
}

// Сколько байт уже лежит в .part — чтобы клиент мог продолжить с места обрыва.
async fn part_size(State(st): State<Arc<Shared>>, Query(q): Query<PartQ>) -> Response<Body> {
    let root = match lock(&st.root).clone() {
        Some(r) => r,
        None => return fail(StatusCode::PRECONDITION_FAILED, "папка для приёма не выбрана"),
    };
    let rel = match safe_rel_path(&q.name) {
        Ok(p) => p,
        Err(e) => return fail(StatusCode::BAD_REQUEST, &e),
    };
    // размер .part относится к конкретной передаче — без комнаты не отвечаем
    let rid = match q.req {
        Some(rid) => rid,
        None => return fail(StatusCode::FORBIDDEN, "передача не разрешена"),
    };
    let base = {
        let reqs = lock(&st.reqs);
        match reqs.get(&rid) {
            // размер .part видят только участники передачи
            Some(r) if q.id == Some(r.from) || q.id == Some(r.to) => {
                if r.to == PC_ID {
                    root
                } else {
                    root.join(&r.temp)
                }
            }
            _ => return fail(StatusCode::FORBIDDEN, "передача не разрешена"),
        }
    };
    let part = part_of(&base.join(&rel));
    let off = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
    Response::new(Body::from(format!("{{\"off\":{off}}}")))
}

async fn upload(
    State(st): State<Arc<Shared>>,
    Query(q): Query<UpQuery>,
    req: axum::extract::Request,
) -> Response<Body> {
    let root = match lock(&st.root).clone() {
        Some(r) => r,
        None => return fail(StatusCode::PRECONDITION_FAILED, "папка для приёма не выбрана"),
    };

    // Без подтверждённой комнаты в папку приёма ничего не пишем: иначе любой в сети
    // заливает файлы в обход диалога, а отменённая передача продолжается.
    // Передача другому устройству идёт через временную папку: ПК — только релей,
    // и после завершения её содержимое удаляется. На ПК — сразу в папку приёма.
    let rid = match q.req {
        Some(rid) => rid,
        None => return fail(StatusCode::FORBIDDEN, "передача не разрешена"),
    };
    let mut base = root;
    let (relay, temp) = {
        let reqs = lock(&st.reqs);
        match reqs.get(&rid) {
            Some(r) if r.state == ReqState::Ok && q.id == Some(r.from) && !r.sender_done => {
                (r.to != PC_ID, r.temp.clone())
            }
            _ => return fail(StatusCode::FORBIDDEN, "передача не разрешена"),
        }
    };
    if relay {
        base = base.join(temp);
    }

    let (parts, body) = req.into_parts();
    let _ = parts;
    let rel = match safe_rel_path(&q.name) {
        Ok(p) => p,
        Err(e) => return fail(StatusCode::BAD_REQUEST, &e),
    };
    let mut target = base.join(&rel);

    // Дубликаты: ow=0 пропустить, ow=1 заменить (по умолчанию), ow=2 переименовать
    let ow = q.ow.unwrap_or(1);
    if tokio::fs::metadata(&target).await.is_ok() {
        match ow {
            0 => {
                return Response::new(Body::from(
                    "{\"ok\":true,\"skip\":true,\"bytes\":0}".to_string(),
                ))
            }
            2 => target = free_name(&target).await,
            _ => {}
        }
    }

    if let Some(parent) = target.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return fail(StatusCode::CONFLICT, &format!("не удалось создать папку: {e}"));
        }
    }

    // недокачанный файл не должен выглядеть готовым: пишем в .part и переименовываем
    let part = part_of(&target);
    let off = q.off.unwrap_or(0);
    // shortcut: докачка доверяет длине .part и не сверяет содержимое — при разных файлах
    // с одинаковым именем получится битый файл; сверять хеш, если такое начнёт случаться.
    let mut file = if off == 0 {
        match tokio::fs::File::create(&part).await {
            Ok(f) => f,
            Err(e) => return fail(StatusCode::CONFLICT, &format!("не удалось открыть файл: {e}")),
        }
    } else {
        let mut f = match tokio::fs::OpenOptions::new().write(true).open(&part).await {
            Ok(f) => f,
            Err(_) => return fail(StatusCode::CONFLICT, "кусок пришёл раньше начала файла"),
        };
        let have = f.metadata().await.map(|m| m.len()).unwrap_or(0);
        if off > have {
            return fail(StatusCode::CONFLICT, "кусок не совпадает с началом файла");
        }
        if let Err(e) = tokio::io::AsyncSeekExt::seek(&mut f, std::io::SeekFrom::Start(off)).await {
            return fail(StatusCode::CONFLICT, &format!("ошибка записи: {e}"));
        }
        f
    };

    let mut body = body.into_data_stream();
    let mut written: u64 = 0;
    loop {
        // медленный клиент не должен держать файл вечно: 60 с без данных — обрыв,
        // .part остаётся и докачивается через /api/part
        let next = match tokio::time::timeout(Duration::from_secs(60), body.next()).await {
            Ok(v) => v,
            Err(_) => {
                let _ = file.flush().await;
                drop(file);
                return fail(StatusCode::REQUEST_TIMEOUT, "слишком долго нет данных");
            }
        };
        let chunk = match next {
            Some(Ok(c)) => c,
            Some(Err(_)) => {
                let _ = file.flush().await;
                drop(file);
                // .part оставляем: клиент дошлёт с места обрыва (см. /api/part)
                return fail(StatusCode::BAD_REQUEST, "обрыв соединения");
            }
            None => break,
        };
        if let Err(e) = file.write_all(&chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&part).await;
            return fail(StatusCode::CONFLICT, &format!("ошибка записи: {e}"));
        }
        written += chunk.len() as u64;
    }
    if let Err(e) = file.sync_all().await {
        drop(file);
        let _ = tokio::fs::remove_file(&part).await;
        return fail(StatusCode::CONFLICT, &format!("ошибка записи: {e}"));
    }
    drop(file);
    let last = q.last.map(|v| v == 1).unwrap_or(true);
    if !last {
        return Response::new(Body::from(format!("{{\"ok\":true,\"bytes\":{written}}}")));
    }
    if let Err(e) = tokio::fs::rename(&part, &target).await {
        let _ = tokio::fs::remove_file(&part).await;
        return fail(StatusCode::CONFLICT, &format!("файл занят другой программой: {e}"));
    }

    {
        let mut r = lock(&st.received);
        r.0 += 1;
        r.1 += written;
    }
    // какие файлы относятся к комнате — их получатель скачает автоматически
    {
        let mut reqs = lock(&st.reqs);
        if let Some(r) = reqs.get_mut(&rid) {
            let shown = target
                .strip_prefix(&base)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| q.name.clone());
            // размер файла целиком: off — начало этого куска, written — сколько в нём пришло
            let size = off + written;
            match r.items.iter_mut().find(|(n, _)| n == &shown) {
                Some(it) => it.1 = size,
                None => r.items.push((shown, size)),
            }
        }
    }
    st.note(format!("принято ({:.1} МБ)", (off + written) as f64 / 1_048_576.0));

    let mut res = Response::new(Body::from(format!("{{\"ok\":true,\"bytes\":{written}}}")));
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res
}

#[derive(Deserialize)]
struct DlQuery {
    name: String,
    /// файл комнаты отдаётся из временной папки — только её участникам
    #[serde(default)]
    req: Option<u64>,
    #[serde(default)]
    id: Option<u64>,
}

/// Каждый запрос к API должен нести токен браузера; устройство из параметра id/from
/// обязано совпадать с этим токеном — иначе любой в сети действует от чужого имени.
async fn auth(
    State(st): State<Arc<Shared>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response<Body> {
    let path = req.uri().path().to_string();
    // страница, поток событий (он и выдаёт токен) и проверка связи — без токена
    if path == "/" || path == "/api/events" || path == "/api/ping" {
        return next.run(req).await;
    }
    let query = req.uri().query().unwrap_or("").to_string();
    let dev = qparam(&query, "t")
        .and_then(|t| lock(&st.by_token).get(t).copied())
        .filter(|id| lock(&st.devices).contains_key(id));
    let dev = match dev {
        Some(d) => d,
        None => return fail(StatusCode::FORBIDDEN, "нужна авторизация"),
    };
    // у answer в id лежит номер комнаты, а не устройства: личность там проверяет сам хендлер
    if path != "/api/answer" {
        for k in ["id", "from"] {
            if let Some(v) = qparam(&query, k) {
                if v.parse::<u64>().ok() != Some(dev) {
                    return fail(StatusCode::FORBIDDEN, "это не ваше устройство");
                }
            }
        }
    }
    req.extensions_mut().insert(DevId(dev));
    next.run(req).await
}

/// Общие заголовки: MIME, запрет кэша, nosniff и запрет встраивания в чужую страницу.
async fn headers(req: axum::extract::Request, next: axum::middleware::Next) -> Response<Body> {
    let path = req.uri().path().to_string();
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let plain = h
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap_or("").starts_with("text/plain"))
        .unwrap_or(true);
    if plain {
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    }
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    if path == "/" {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
                 img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self'; \
                 frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            ),
        );
    }
    res
}

/// Хост и Origin сверяем со своим портом: иначе чужой сайт может дёргать API
/// из браузера пользователя (DNS rebinding, CSRF). Имя хоста не принимаем —
/// только IP из локальных сетей, localhost и loopback.
#[derive(Clone)]
struct Origin {
    port: u16,
}

/// Имя из Host/Origin, если это обращение к нам: `ip:порт`, `localhost:порт`.
fn local_host(h: &str, port: u16) -> Option<String> {
    let h = h.trim().to_ascii_lowercase();
    if h == format!("localhost:{port}") {
        return Some("localhost".into());
    }
    let (ip, p) = h.rsplit_once(':')?;
    if p != port.to_string() {
        return None;
    }
    let ip: std::net::IpAddr = ip.trim_matches(['[', ']']).parse().ok()?;
    let local = match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
        }
    };
    local.then(|| ip.to_string())
}

async fn guard(
    State(o): State<Origin>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response<Body> {
    let headers = req.headers();
    let host = match headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        Some(h) => match local_host(h, o.port) {
            Some(ip) => ip,
            None => return fail(StatusCode::FORBIDDEN, "чужой адрес"),
        },
        None => return fail(StatusCode::FORBIDDEN, "чужой адрес"),
    };
    if let Some(org) = headers.get(header::ORIGIN).or_else(|| headers.get(header::REFERER)) {
        if let Ok(v) = org.to_str() {
            let src = v.split("//").nth(1).unwrap_or("").split('/').next().unwrap_or("");
            if local_host(src, o.port).as_deref() != Some(host.as_str()) {
                return fail(StatusCode::FORBIDDEN, "чужой источник");
            }
        }
    }
    next.run(req).await
}

#[derive(Deserialize)]
struct ExistsQ {
    /// комната: у передачи телефон→телефон файлы лежат не в основной папке
    #[serde(default)]
    req: Option<u64>,
}

/// Какие из присланных имён уже есть в папке приёма.
async fn exists(
    State(st): State<Arc<Shared>>,
    axum::extract::Extension(DevId(dev)): axum::extract::Extension<DevId>,
    Query(q): Query<ExistsQ>,
    req: axum::extract::Request,
) -> Response<Body> {
    // имена в чужой папке приёма не показываем
    let allowed = match q.req {
        Some(rid) => lock(&st.reqs)
            .get(&rid)
            .map(|r| r.from == dev || r.to == dev)
            .unwrap_or(false),
        None => dev == PC_ID,
    };
    if !allowed {
        return fail(StatusCode::FORBIDDEN, "нет доступа к этой папке");
    }
    let root = match lock(&st.root).clone() {
        Some(r) => r,
        None => return fail(StatusCode::PRECONDITION_FAILED, "папка для приёма не выбрана"),
    };
    let dir = match q
        .req
        .and_then(|rid| lock(&st.reqs).get(&rid).map(|r| (r.to, r.temp.clone())))
    {
        Some((to, temp)) if to != PC_ID => root.join(temp),
        _ => root.clone(),
    };
    let body = match axum::body::to_bytes(req.into_body(), 8 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return fail(StatusCode::BAD_REQUEST, "слишком большой список"),
    };
    let names: Vec<String> = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return fail(StatusCode::BAD_REQUEST, "ожидался список имён"),
    };
    let mut have = Vec::new();
    for n in names.iter().take(100_000) {
        if let Ok(rel) = safe_rel_path(n) {
            if tokio::fs::metadata(dir.join(rel)).await.is_ok() {
                have.push(n.clone());
            }
        }
    }
    let have_json = have
        .iter()
        .map(|n| format!("\"{}\"", esc(n)))
        .collect::<Vec<_>>()
        .join(",");
    let mut res = Response::new(Body::from(format!("{{\"have\":[{have_json}]}}")));
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res
}

/// Отдача файла с ПК на устройство или из временной папки комнаты.
async fn download(
    State(st): State<Arc<Shared>>,
    Query(q): Query<DlQuery>,
) -> Response<Body> {
    let root = match lock(&st.root).clone() {
        Some(r) => r,
        None => return fail(StatusCode::PRECONDITION_FAILED, "папка для приёма не выбрана"),
    };
    // файлы раздаются только участникам передачи, из папки приёма напрямую — нет
    let rid = match q.req {
        Some(rid) => rid,
        None => return fail(StatusCode::FORBIDDEN, "нет доступа к этой передаче"),
    };
    let base = {
        let reqs = lock(&st.reqs);
        match reqs.get(&rid) {
            // на ПК файлы лежат прямо в папке приёма, у остальных — во временной папке комнаты
            Some(r) if q.id == Some(r.from) || q.id == Some(r.to) => {
                if r.to == PC_ID { root.clone() } else { root.join(&r.temp) }
            }
            _ => return fail(StatusCode::FORBIDDEN, "нет доступа к этой передаче"),
        }
    };
    let rel = match safe_rel_path(&q.name) {
        Ok(p) => p,
        Err(e) => return fail(StatusCode::BAD_REQUEST, &e),
    };
    let path = base.join(rel);
    let md = match tokio::fs::metadata(&path).await {
        Ok(m) if m.is_file() => m,
        _ => return fail(StatusCode::NOT_FOUND, "файл не найден"),
    };
    let f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return fail(StatusCode::CONFLICT, "файл занят другой программой"),
    };
    let stream = futures_util::stream::unfold(Some(f), |st| async move {
        let mut file = st?;
        let mut buf = vec![0u8; 64 * 1024];
        match tokio::io::AsyncReadExt::read(&mut file, &mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<Vec<u8>, std::io::Error>(buf), Some(file)))
            }
            Err(e) => Some((Err(e), None)),
        }
    });
    let mut res = Response::new(Body::from_stream(stream));
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(md.len()));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    let raw = q.name.rsplit(['/', '\\']).next().unwrap_or("file");
    let ascii: String = raw
        .chars()
        .map(|c| if c.is_ascii() && c != '"' && c != '\\' && c != ';' && c != '%' { c } else { '_' })
        .collect();
    let enc = pct(raw);
    if let Ok(v) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{enc}"
    )) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    res
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Свободное имя рядом: «фото.jpg» → «фото (1).jpg».
async fn free_name(target: &std::path::Path) -> std::path::PathBuf {
    let stem = target.file_stem().and_then(|s| s.to_str()).unwrap_or("file").to_string();
    let ext = target.extension().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let dir = target.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for n in 1..10_000 {
        let name = if ext.is_empty() {
            format!("{stem} ({n})")
        } else {
            format!("{stem} ({n}).{ext}")
        };
        let cand = dir.join(name);
        if tokio::fs::metadata(&cand).await.is_err() {
            return cand;
        }
    }
    target.to_path_buf()
}

/// Экранирование многострочного текста для JSON: переводы строк сохраняются.
fn esc_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Экранирование строки для JSON (подписи устройств уже отфильтрованы).
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Подпись устройства: только безопасные символы, максимум 40.
fn clean_label(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if out.chars().count() >= 40 {
            break;
        }
        if (ch as u32) >= 0x20 && !matches!(ch, '"' | '\\' | '<' | '>' | '`') {
            out.push(ch);
        }
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        "устройство".into()
    } else {
        out
    }
}

fn fail(code: StatusCode, msg: &str) -> Response<Body> {
    let body = format!("{{\"error\":\"{}\"}}", msg.replace('"', "'"));
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = code;
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res
}

/// Зарезервированные номера устройств Windows: COM1..COM9, LPT1..LPT9.
fn reserved_num(stem: &str) -> bool {
    if stem.len() != 4 {
        return false;
    }
    let b = stem.as_bytes();
    (stem.starts_with("COM") || stem.starts_with("LPT")) && b[3].is_ascii_digit() && b[3] != b'0'
}

/// Путь от клиента превращаем в максимально безопасный относительный путь.
fn safe_rel_path(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("пустое имя файла".into());
    }
    if raw.len() > 1000 {
        return Err("слишком длинный путь".into());
    }
    let mut out = PathBuf::new();
    for part in raw.split(['/', '\\']) {
        if part.is_empty() || part == "." || part == ".." {
            return Err("недопустимый путь".into());
        }
        let mut clean = String::with_capacity(part.len());
        for ch in part.chars() {
            if (ch as u32) < 0x20 || matches!(ch, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                clean.push('_');
            } else {
                clean.push(ch);
            }
        }
        while clean.ends_with(' ') || clean.ends_with('.') {
            clean.pop();
        }
        if clean.is_empty() {
            return Err("недопустимое имя файла".into());
        }
        // CON, NUL, COM1… — устройства Windows, а не файлы
        let stem = clean.split('.').next().unwrap_or("").to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || reserved_num(&stem) {
            clean.insert(0, '_');
        }
        out.push(clean);
    }
    if out.as_os_str().is_empty() {
        return Err("пустой путь".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Устройство с заданным id, как будто оно подключилось и открыло SSE.
    fn dev(st: &Arc<Shared>, id: u64, label: &str) -> broadcast::Receiver<String> {
        let (tx, rx) = broadcast::channel(16);
        lock(&st.devices).insert(
            id,
            Dev { label: label.into(), named: true, last: Instant::now(), tx, room: None },
        );
        lock(&st.by_ip).insert(format!("ip{id}"), id);
        rx
    }

    fn room_of(st: &Arc<Shared>, id: u64) -> Option<(u64, ReqState, String)> {
        let rid = lock(&st.devices).get(&id).and_then(|d| d.room)?;
        let reqs = lock(&st.reqs);
        let r = reqs.get(&rid)?;
        Some((rid, r.state, r.err.clone()))
    }

    /// Имя, данное устройству, должно запоминаться и на токен: Safari в фоне
    /// теряет SSE, устройство удаляется, а адрес может смениться.
    #[test]
    fn name_remembered_by_token() {
        let st = Arc::new(Shared::new());
        let _rx = dev(&st, 1, "old");
        lock(&st.by_token).insert("tok".to_string(), 1);
        assert!(st.set_name(1, "Phone-2", "ip1"));
        assert_eq!(st.tok_name("tok").as_deref(), Some("Phone-2"));
        // имя по адресу тоже обновилось
        assert_eq!(st.known_name("ip1").as_deref(), Some("Phone-2"));
        // тест пишет в настоящий devices.json — убираем за собой
        let mut names = lock(&st.names);
        names.remove("ip1");
        save_names(&names);
    }

    async fn next(rx: &mut broadcast::Receiver<String>) -> String {
        rx.recv().await.unwrap()
    }

    /// Ждёт среди ближайших событий то, что содержит подстроку (промежуточные пропускает).
    async fn next_with(rx: &mut broadcast::Receiver<String>, needle: &str) -> String {
        for _ in 0..6 {
            let m = rx.recv().await.unwrap();
            if m.contains(needle) {
                return m;
            }
        }
        panic!("событие с {needle} не пришло");
    }

    #[tokio::test]
    async fn full_flow_and_prune() {
        let st = Shared::new();
        let mut a = dev(&st, 1, "ноутбук");
        let mut b = dev(&st, 2, "телефон");

        // запрос: получатель видит его сразу, статус wait
        let json = request(
            State(st.clone()),
            Query(ReqQ { from: 1, to: 2, files: 2 }),
        )
        .await;
        let rid: u64 = json
            .split("\"id\":")
            .nth(1)
            .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
            .unwrap()
            .parse()
            .unwrap();
        let msg = next(&mut b).await;
        assert!(msg.contains("\"t\":\"req\""), "{msg}");
        assert_eq!(room_of(&st, 1).unwrap().1, ReqState::Wait);
        assert_eq!(room_of(&st, 2).unwrap().1, ReqState::Wait);

        // подтверждение: обе стороны получают state ok
        answer(
            State(st.clone()),
            axum::extract::Extension(DevId(2)),
            Query(AnsQ { id: rid, accept: 1 }),
        )
        .await;
        assert_eq!(room_of(&st, 1).unwrap().1, ReqState::Ok);
        next_with(&mut a, "\"state\":\"ok\"").await;
        next_with(&mut b, "\"state\":\"ok\"").await;

        // прогресс принимается только от отправителя и только вниз
        progress(
            State(st.clone()),
            Query(ProgQ { id: 2, req: rid, files: 2, bytes: 0, files_done: 9, bytes_done: 0 }),
        )
        .await;
        assert_eq!(room_of(&st, 1).unwrap().1, ReqState::Ok);
        let _ = next(&mut a).await;
        let _ = next(&mut b).await;
        progress(
            State(st.clone()),
            Query(ProgQ { id: 1, req: rid, files: 2, bytes: 100, files_done: 1, bytes_done: 50 }),
        )
        .await;
        {
            let reqs = lock(&st.reqs);
            let r = reqs.get(&rid).unwrap();
            assert_eq!((r.files_done, r.bytes_done), (1, 50));
        }

        // отмена: комната не удаляется, а становится abort
        abort(State(st.clone()), Query(ByIdQ { id: 2, req: rid })).await;
        assert_eq!(room_of(&st, 1).unwrap().1, ReqState::Abort);
        next_with(&mut a, "\"state\":\"abort\"").await;

        // вторая передача завершается нормально
        let json = request(State(st.clone()), Query(ReqQ { from: 2, to: 1, files: 1 })).await;
        let rid2: u64 = json
            .split("\"id\":")
            .nth(1)
            .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
            .unwrap()
            .parse()
            .unwrap();
        answer(
            State(st.clone()),
            axum::extract::Extension(DevId(1)),
            Query(AnsQ { id: rid2, accept: 1 }),
        )
        .await;
        done(State(st.clone()), Query(ByIdQ { id: 2, req: rid2 })).await;
        // завершённая передача отвязана: устройство свободно и не утащит следующий файл без вопроса
        assert_eq!(lock(&st.reqs).get(&rid2).unwrap().state, ReqState::Done);
        assert!(room_of(&st, 1).is_none() && room_of(&st, 2).is_none());

        // send to unknown device → ошибка
        let err = request(State(st.clone()), Query(ReqQ { from: 1, to: 99, files: 1 })).await;
        assert!(err.contains("error"), "{err}");
    }

    #[tokio::test]
    async fn sender_leaves_aborts_room() {
        let st = Shared::new();
        let a = dev(&st, 1, "ноутбук");
        let _b = dev(&st, 2, "телефон");
        request(State(st.clone()), Query(ReqQ { from: 1, to: 2, files: 1 })).await;
        let rid = room_of(&st, 1).unwrap().0;
        answer(
            State(st.clone()),
            axum::extract::Extension(DevId(2)),
            Query(AnsQ { id: rid, accept: 1 }),
        )
        .await;
        drop(a); // отправитель закрыл все вкладки
        lock(&st.devices).get_mut(&1).unwrap().last =
            Instant::now() - Duration::from_secs(10);
        st.prune();
        let (_, state, err) = room_of(&st, 2).unwrap();
        assert_eq!(state, ReqState::Abort);
        assert!(err.contains("отправитель"), "{err}");
        assert!(lock(&st.devices).get(&1).is_none());
    }

    #[tokio::test]
    async fn answer_twice_keeps_first() {
        let st = Shared::new();
        let _a = dev(&st, 1, "ноутбук");
        let mut b = dev(&st, 2, "телефон");
        let json = request(State(st.clone()), Query(ReqQ { from: 1, to: 2, files: 1 })).await;
        let rid: u64 = json
            .split("\"id\":")
            .nth(1)
            .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
            .unwrap()
            .parse()
            .unwrap();
        answer(
            State(st.clone()),
            axum::extract::Extension(DevId(2)),
            Query(AnsQ { id: rid, accept: 1 }),
        )
        .await;
        answer(
            State(st.clone()),
            axum::extract::Extension(DevId(2)),
            Query(AnsQ { id: rid, accept: 0 }),
        )
        .await;
        assert_eq!(room_of(&st, 1).unwrap().1, ReqState::Ok);
        let _ = next(&mut b).await;
    }

    /// Чанковая загрузка: в комнате размер файла целиком, а не последнего куска;
    /// кусок за концом .part отклоняется.
    #[tokio::test]
    async fn chunked_upload_reports_full_size() {
        let st = Shared::new();
        let _a = dev(&st, 1, "ноутбук");
        let _b = dev(&st, 2, "телефон");
        let dir = std::env::temp_dir().join(format!("neonsend-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        *lock(&st.root) = Some(dir.clone());
        request(State(st.clone()), Query(ReqQ { from: 1, to: 2, files: 1 })).await;
        let rid = room_of(&st, 1).unwrap().0;
        answer(
            State(st.clone()),
            axum::extract::Extension(DevId(2)),
            Query(AnsQ { id: rid, accept: 1 }),
        )
        .await;
        let up = |name: &str, off: u64, last: u8, n: usize| {
            upload(
                State(st.clone()),
                Query(UpQuery {
                    name: name.to_string(),
                    id: Some(1),
                    req: Some(rid),
                    ow: None,
                    off: Some(off),
                    last: Some(last),
                }),
                axum::http::Request::builder()
                    .body(axum::body::Body::from(vec![7u8; n]))
                    .unwrap(),
            )
        };
        up("a.bin", 0, 0, 10).await;
        up("a.bin", 10, 1, 10).await;
        assert_eq!(lock(&st.reqs).get(&rid).unwrap().items[0].1, 20);
        up("b.bin", 0, 0, 5).await;
        assert_eq!(up("b.bin", 999, 1, 1).await.status(), StatusCode::CONFLICT);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Чужое устройство не может ни завершить, ни прервать комнату.
    #[tokio::test]
    async fn stranger_cannot_finish_or_leave_room() {
        let st = Shared::new();
        let _a = dev(&st, 1, "ноутбук");
        let _b = dev(&st, 2, "телефон");
        request(State(st.clone()), Query(ReqQ { from: 1, to: 2, files: 1 })).await;
        let rid = room_of(&st, 1).unwrap().0;
        done(State(st.clone()), Query(ByIdQ { id: 99, req: rid })).await;
        leave(State(st.clone()), Query(ByIdQ { id: 99, req: rid })).await;
        assert_eq!(room_of(&st, 1).unwrap().1, ReqState::Wait);
    }

    /// Передача на ПК завершается сама: файлы уже в папке приёма, получателю скачивать нечего.
    #[tokio::test]
    async fn pc_room_finishes_after_sender_done() {
        let st = Shared::new();
        let _a = dev(&st, 1, "телефон");
        let _b = dev(&st, PC_ID, "пк");
        let json = request(State(st.clone()), Query(ReqQ { from: 1, to: PC_ID, files: 1 })).await;
        let rid: u64 = json
            .split("\"id\":")
            .nth(1)
            .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
            .unwrap()
            .parse()
            .unwrap();
        answer(
            State(st.clone()),
            axum::extract::Extension(DevId(PC_ID)),
            Query(AnsQ { id: rid, accept: 1 }),
        )
        .await;
        // тело push — список имён и размеров: он должен доехать до получателя
        let body = axum::http::Request::builder()
            .body(axum::body::Body::from(r#"[{"name":"a.txt","size":10}]"#.to_string()))
            .unwrap();
        push(State(st.clone()), Query(PushQ { id: 1, req: rid, files: 1, bytes: 10 }), body).await;
        {
            let reqs = lock(&st.reqs);
            assert_eq!(reqs.get(&rid).unwrap().planned, vec![("a.txt".to_string(), 10)]);
        }
        progress(
            State(st.clone()),
            Query(ProgQ { id: 1, req: rid, files: 1, bytes: 10, files_done: 1, bytes_done: 10 }),
        )
        .await;
        sender_done(State(st.clone()), Query(ByIdQ { id: 1, req: rid })).await;
        assert_eq!(lock(&st.reqs).get(&rid).unwrap().state, ReqState::Done);
        assert!(room_of(&st, 1).is_none() && room_of(&st, PC_ID).is_none());
    }
}
