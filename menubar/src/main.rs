// GripApprover メニューバーアプリ
//
// ESP32 と BLE か USB シリアルでつなぎ、hook からの HTTP リクエストを握った回数で待たせる。
// USB のポートがあればシリアルを使い、なければ BLE で探す（同時には使わない）。
// メニューバーの「オン」を外している間は、hook に off を返して何もさせない。
// オフにしてもシリアルのポートは閉じない（macOS の CP2102 ドライバが、閉じている間に
// 届いたデータを開き直したときに繰り返し渡すことがあるため）。
use std::error::Error;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use serde_json::json;
use serialport::SerialPort;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
use uuid::Uuid;

const BAUD: u32 = 115_200;
const PORT_PREFIXES: [&str; 3] = ["cu.usbserial-", "cu.SLAB_USBtoUART", "cu.wchusbserial"];
const HTTP_ADDR: &str = "127.0.0.1:8766";
const BAR_WIDTH: u32 = 30;
const BLE_SERVICE: Uuid = Uuid::from_u128(0x8f1e0001_4b1a_4c3e_9d2a_6a7b6e0c1a01);
const BLE_COUNT: Uuid = Uuid::from_u128(0x8f1e0002_4b1a_4c3e_9d2a_6a7b6e0c1a01);
const BLE_CMD: Uuid = Uuid::from_u128(0x8f1e0003_4b1a_4c3e_9d2a_6a7b6e0c1a01);

enum UserEvent {
    Menu(MenuEvent),
    Refresh,
}

struct Waiting {
    done: u32,
    reps: u32,
    remaining: f64,
}

struct State {
    enabled: bool,
    connected: bool,
    port: String,
    total: u64,    // 起動後の総握り回数
    conn_gen: u64, // 接続ごとに増える番号。待機中の切断を見分ける
    busy: bool,    // 承認待ちは同時に1件だけ
    waiting: Option<Waiting>,
}

/// ESP32 へコマンドを送る先。つながっている方だけを持つ
enum Link {
    Serial(Box<dyn SerialPort>),
    Ble(tokio::sync::mpsc::UnboundedSender<u8>),
}

struct App {
    state: Mutex<State>,
    cond: Condvar,
    writer: Mutex<Option<Link>>,
    proxy: Mutex<EventLoopProxy<UserEvent>>,
}

impl App {
    fn refresh(&self) {
        let _ = self.proxy.lock().unwrap().send_event(UserEvent::Refresh);
    }

    fn send_command(&self, cmd: &[u8]) {
        match self.writer.lock().unwrap().as_mut() {
            Some(Link::Serial(port)) => {
                if let Err(e) = port.write_all(cmd) {
                    log(&format!("ESP32 へのコマンド送信に失敗しました: {e}"));
                }
            }
            // BLE の書き込みは非同期なので、BLE の処理側に渡して書いてもらう
            Some(Link::Ble(tx)) => cmd.iter().for_each(|&c| {
                let _ = tx.send(c);
            }),
            None => {}
        }
    }

    /// ESP32 から届いた累計回数を、前回との差分にして総握り回数に足す
    fn add_count(&self, last: &mut u64, value: u64) {
        // 値が減ったら ESP32 が再起動したとみなし、新しい値をそのまま加算する
        let delta = if value >= *last { value - *last } else { value };
        *last = value;
        if delta > 0 {
            self.state.lock().unwrap().total += delta;
            self.cond.notify_all();
        }
    }
}

fn log(msg: &str) {
    let mut out = io::stdout();
    let _ = write!(out, "\r\x1b[2K{msg}\n");
    let _ = out.flush();
}

fn show_progress(label: &str, done: u32, reps: u32, remaining: f64) {
    let filled = if reps == 0 { BAR_WIDTH } else { (BAR_WIDTH * done / reps).min(BAR_WIDTH) };
    let bar = "#".repeat(filled as usize) + &"-".repeat((BAR_WIDTH - filled) as usize);
    let mut out = io::stdout();
    let _ = write!(out, "\r\x1b[2K{label} [{bar}] {done}/{reps}回 残り{remaining:.0}秒");
    let _ = out.flush();
}

// ---- 承認待ち ----

fn wait_for_grips(app: &App, reps: u32, timeout: f64, label: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let reply = |result: &str, done: u32| json!({"result": result, "done": done, "reps": reps});

    let mut st = app.state.lock().unwrap();
    if !st.enabled {
        return reply("off", 0);
    }
    if !st.connected {
        return reply("offline", 0);
    }

    // 締め切りは要求が届いた時刻から数える。順番待ちの時間も含めて timeout に収める
    while st.busy {
        if remaining().is_zero() {
            return reply("timeout", 0);
        }
        st = app.cond.wait_timeout(st, remaining()).unwrap().0;
    }
    st.busy = true;
    let base = st.total;
    let gen_at_start = st.conn_gen;

    let (result, done) = loop {
        let done = (st.total - base).min(reps as u64) as u32;
        let left = remaining();
        if done >= reps {
            break ("approved", done);
        }
        if !st.enabled {
            break ("off", done);
        }
        if !st.connected || st.conn_gen != gen_at_start || left.is_zero() {
            break ("timeout", done);
        }
        st.waiting = Some(Waiting { done, reps, remaining: left.as_secs_f64() });
        app.refresh();
        show_progress(label, done, reps, left.as_secs_f64());
        st = app.cond.wait_timeout(st, left.min(Duration::from_millis(500))).unwrap().0;
    };

    st.busy = false;
    st.waiting = None;
    app.cond.notify_all();
    drop(st);
    app.refresh();

    show_progress(label, done, reps, remaining().as_secs_f64());
    match result {
        "approved" => {
            log("  → 承認");
            app.send_command(b"A");
        }
        "timeout" => {
            log("  → 時間切れ");
            app.send_command(b"D");
        }
        _ => log("  → オフにしたため中断"),
    }
    reply(result, done)
}

// ---- HTTP ----

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (percent_decode(k) == key).then(|| percent_decode(v))
    })
}

fn http_loop(app: Arc<App>) {
    let server = match tiny_http::Server::http(HTTP_ADDR) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("http://{HTTP_ADDR} で待ち受けられません: {e}"));
            std::process::exit(1);
        }
    };
    log(&format!("http://{HTTP_ADDR} で待ち受けています"));
    for request in server.incoming_requests() {
        let app = app.clone();
        thread::spawn(move || {
            let url = request.url().to_string();
            let (path, query) = url.split_once('?').unwrap_or((&url, ""));
            if path != "/wait" {
                let _ = request.respond(tiny_http::Response::empty(404));
                return;
            }
            let reps = query_param(query, "reps").unwrap_or_else(|| "0".into()).parse::<u32>();
            let timeout = query_param(query, "timeout").unwrap_or_else(|| "60".into()).parse::<f64>();
            let (Ok(reps), Ok(timeout)) = (reps, timeout) else {
                let _ = request.respond(tiny_http::Response::empty(400));
                return;
            };
            let label = query_param(query, "label").unwrap_or_else(|| "承認待ち".into());
            let body = wait_for_grips(&app, reps, timeout, &label).to_string();
            let header = tiny_http::Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap();
            let _ = request.respond(tiny_http::Response::from_string(body).with_header(header));
        });
    }
}

// ---- シリアル ----

fn find_port() -> Option<String> {
    if let Ok(port) = std::env::var("GRIP_PORT") {
        return std::path::Path::new(&port).exists().then_some(port);
    }
    let mut names: Vec<String> = std::fs::read_dir("/dev")
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| PORT_PREFIXES.iter().any(|p| n.starts_with(p)))
        .collect();
    names.sort();
    names.into_iter().next().map(|n| format!("/dev/{n}"))
}

/// "grip N" または "count N" なら (種類, N) を返す。ドライバが混ぜる NULL バイトは無視する
fn parse_line(raw: &[u8]) -> Option<(String, u64)> {
    let text = String::from_utf8_lossy(raw);
    let mut parts = text.trim_matches(|c: char| c == '\0' || c.is_whitespace()).split_whitespace();
    let (kind, n, rest) = (parts.next()?, parts.next()?, parts.next());
    if rest.is_some() || !(kind == "grip" || kind == "count") {
        return None;
    }
    Some((kind.to_string(), n.parse().ok()?))
}

/// 1行読む。タイムアウトなら None を返し、途中まで読んだ分は buf に残す
fn read_line(reader: &mut BufReader<Box<dyn SerialPort>>, buf: &mut Vec<u8>) -> io::Result<Option<Vec<u8>>> {
    match reader.read_until(b'\n', buf) {
        Ok(0) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "ポートが閉じられました")),
        Ok(_) if buf.ends_with(b"\n") => Ok(Some(std::mem::take(buf))),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(None),
        Err(e) => Err(e),
    }
}

/// ? を送り、count N の返事で基準値を得る。ポートを開くと ESP32 が再起動することがあるので数回試す
fn read_baseline(app: &App, reader: &mut BufReader<Box<dyn SerialPort>>) -> io::Result<Option<u64>> {
    let mut buf = Vec::new();
    for _ in 0..5 {
        app.send_command(b"?");
        let end = Instant::now() + Duration::from_secs(1);
        while Instant::now() < end {
            if let Some(line) = read_line(reader, &mut buf)? {
                if let Some((kind, n)) = parse_line(&line) {
                    if kind == "count" {
                        return Ok(Some(n));
                    }
                }
            }
        }
    }
    Ok(None)
}

fn set_connected(app: &App, connected: bool, port: &str) {
    let mut st = app.state.lock().unwrap();
    st.connected = connected;
    st.port = port.to_string();
    if connected {
        st.conn_gen += 1;
    }
    app.cond.notify_all();
    drop(st);
    app.refresh();
}

fn run_serial(app: &App, port_name: &str) -> io::Result<()> {
    let port = serialport::new(port_name, BAUD).timeout(Duration::from_millis(500)).open()?;
    *app.writer.lock().unwrap() = Some(Link::Serial(port.try_clone()?));
    let mut reader = BufReader::new(port);

    let Some(mut last) = read_baseline(app, &mut reader)? else {
        return Err(io::Error::other("ESP32 から返事がありません（GripApprover のファームウェアか確認してください）"));
    };
    set_connected(app, true, port_name);
    log(&format!("接続しました（{port_name}）"));

    let mut buf = Vec::new();
    loop {
        let Some(line) = read_line(&mut reader, &mut buf)? else { continue };
        let Some((_, value)) = parse_line(&line) else { continue };
        app.add_count(&mut last, value);
    }
}

// ---- BLE ----

fn le_u32(bytes: &[u8]) -> Option<u64> {
    Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as u64)
}

/// GripApprover のサービス UUID で探す。見つからなければ None
async fn find_ble(adapter: &Adapter, wait: Duration) -> btleplug::Result<Option<Peripheral>> {
    adapter.start_scan(ScanFilter { services: vec![BLE_SERVICE] }).await?;
    let end = Instant::now() + wait;
    let mut found = None;
    while found.is_none() && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(300)).await;
        found = adapter.peripherals().await?.into_iter().next();
    }
    adapter.stop_scan().await?;
    Ok(found)
}

/// BLE の各手順に時間の上限を付ける。止まったまま待ち続けると、ESP32 側は接続中のまま広告をやめてしまうため
async fn step<T>(what: &str, fut: impl Future<Output = btleplug::Result<T>>) -> Result<T, Box<dyn Error>> {
    match tokio::time::timeout(Duration::from_secs(10), fut).await {
        Ok(r) => Ok(r?),
        Err(_) => Err(format!("{what}が時間切れになりました").into()),
    }
}

async fn run_ble(app: &App, adapter: &Adapter, p: &Peripheral) -> Result<(), Box<dyn Error>> {
    log("BLE で見つけました。接続します…");
    step("接続", p.connect()).await?;
    step("サービスの取得", p.discover_services()).await?;
    let chars = p.characteristics();
    let find = |uuid| chars.iter().find(|c| c.uuid == uuid).cloned().ok_or("GripApprover のサービスが見つかりません");
    let (count, cmd) = (find(BLE_COUNT)?, find(BLE_CMD)?);

    let mut events = adapter.events().await?;
    step("通知の登録", p.subscribe(&count)).await?;
    let mut notes = p.notifications().await?;
    let mut last = le_u32(&step("回数の読み取り", p.read(&count)).await?).ok_or("回数を読めませんでした")?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    *app.writer.lock().unwrap() = Some(Link::Ble(tx));
    set_connected(app, true, "BLE");
    log("接続しました（BLE）");

    // macOS では切断しても通知の流れが終わらないことがあるので、切断のイベントと定期的な確認の両方で見る
    let mut check = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            Some(n) = notes.next() => {
                if let (true, Some(value)) = (n.uuid == BLE_COUNT, le_u32(&n.value)) {
                    app.add_count(&mut last, value);
                }
            }
            Some(c) = rx.recv() => step("コマンドの送信", p.write(&cmd, &[c], WriteType::WithoutResponse)).await?,
            Some(ev) = events.next() => {
                if matches!(ev, CentralEvent::DeviceDisconnected(id) if id == p.id()) {
                    return Err("切断されました".into());
                }
            }
            _ = check.tick() => {
                if !p.is_connected().await.unwrap_or(false) {
                    return Err("切断されました".into());
                }
            }
        }
    }
}

// ---- 接続の管理 ----

/// USB のポートがあればシリアルで、なければ BLE でつなぐ。切れたら探し直す
fn device_loop(app: Arc<App>) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("非同期の実行環境を作れませんでした");
    let adapter = rt.block_on(async { Manager::new().await.ok()?.adapters().await.ok()?.into_iter().next() });
    if adapter.is_none() {
        log("Bluetooth が使えないため、USB だけで探します");
    }
    let disconnected = || {
        *app.writer.lock().unwrap() = None;
        set_connected(&app, false, "");
    };

    let mut searching_logged = false;
    loop {
        if let Some(port) = find_port() {
            searching_logged = false;
            if let Err(e) = run_serial(&app, &port) {
                log(&format!("シリアルの接続が切れました: {e}"));
            }
            disconnected();
            thread::sleep(Duration::from_secs(1));
            continue;
        }
        if !searching_logged {
            log("ESP32 を探しています…（電池のスイッチを入れるか、USB でつないでください）");
            searching_logged = true;
        }
        let Some(adapter) = &adapter else {
            thread::sleep(Duration::from_secs(1));
            continue;
        };
        match rt.block_on(find_ble(adapter, Duration::from_secs(3))) {
            Ok(Some(p)) => {
                searching_logged = false;
                if let Err(e) = rt.block_on(run_ble(&app, adapter, &p)) {
                    log(&format!("BLE の接続が切れました: {e}"));
                }
                disconnected();
                let _ = rt.block_on(p.disconnect());
            }
            Ok(None) => {}
            Err(e) => {
                log(&format!("BLE で探せませんでした: {e}"));
                thread::sleep(Duration::from_secs(3));
            }
        }
    }
}

// ---- メニューバー ----

fn title_and_status(st: &State) -> (String, String) {
    let status = if st.connected { format!("ESP32: 接続中（{}）", st.port) } else { "ESP32: 未接続".to_string() };
    let title = match (&st.waiting, st.enabled, st.connected) {
        (Some(w), _, _) => format!("💪 {}/{} {:.0}s", w.done, w.reps, w.remaining),
        (None, false, _) => "💪 OFF".to_string(),
        (None, true, true) => "💪 ON".to_string(),
        (None, true, false) => "💪 未接続".to_string(),
    };
    (title, status)
}

fn main() {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory); // Dock にアイコンを出さない

    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = menu_proxy.send_event(UserEvent::Menu(e));
    }));

    let app = Arc::new(App {
        state: Mutex::new(State {
            enabled: false,
            connected: false,
            port: String::new(),
            total: 0,
            conn_gen: 0,
            busy: false,
            waiting: None,
        }),
        cond: Condvar::new(),
        writer: Mutex::new(None),
        proxy: Mutex::new(event_loop.create_proxy()),
    });

    let toggle = CheckMenuItem::new("オン", true, false, None);
    let status = MenuItem::new("ESP32: 未接続", false, None);
    let quit = MenuItem::new("終了", true, None);
    let menu = Menu::new();
    menu.append_items(&[&toggle, &status, &PredefinedMenuItem::separator(), &quit]).unwrap();
    let mut tray: Option<TrayIcon> = None;

    {
        let app = app.clone();
        thread::spawn(move || http_loop(app));
    }
    {
        let app = app.clone();
        thread::spawn(move || device_loop(app));
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // メニューバーのアイコンは、イベントループが動き出してから作る
            Event::NewEvents(StartCause::Init) => {
                tray = Some(
                    TrayIconBuilder::new()
                        .with_menu(Box::new(menu.clone()))
                        .with_title("💪 OFF")
                        .with_tooltip("GripApprover")
                        .build()
                        .expect("メニューバーのアイコンを作れませんでした"),
                );
                app.refresh();
            }
            Event::UserEvent(UserEvent::Menu(e)) if e.id == *toggle.id() => {
                let mut st = app.state.lock().unwrap();
                st.enabled = !st.enabled;
                log(if st.enabled { "オンにしました" } else { "オフにしました" });
                app.cond.notify_all();
                drop(st);
                app.refresh();
            }
            Event::UserEvent(UserEvent::Menu(e)) if e.id == *quit.id() => {
                tray.take();
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::Refresh) => {
                let st = app.state.lock().unwrap();
                let (title, status_text) = title_and_status(&st);
                toggle.set_checked(st.enabled);
                drop(st);
                status.set_text(status_text);
                if let Some(t) = &tray {
                    t.set_title(Some(title));
                }
            }
            _ => {}
        }
    });
}
