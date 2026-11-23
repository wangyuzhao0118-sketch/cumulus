//! 本机控制接口。
//!
//! 掌管 VM 的进程（开着窗口时是主程序，否则是后台守护进程 `--guard`）监听一个 unix socket，
//! 命令行（`cumulus status` 等）把请求发给它。Colab 每台 VM 只允许一条 ssh 连接，
//! 所以命令行自己不建连接：开关机、状态、模型、任务都经过掌管进程；
//! 需要在 VM 上跑命令时复用它的 ssh 主连接。
//!
//! 协议：每个连接发一行 JSON 请求 `{"cmd": "...", ...}`，收一行 JSON 回复
//! `{"ok": true, ...}` 或 `{"ok": false, "error": "..."}`。
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::config::{self, Config, GPUS};
use crate::engine::{DlStatus, Engine, Phase};

pub fn sock_path() -> PathBuf {
    let p = config::dir().join("control.sock");
    // unix socket 路径最长 104 字节；设置目录很深时退到 /tmp，按设置目录区分
    if p.as_os_str().len() < 100 {
        return p;
    }
    let h = p.to_string_lossy().bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    PathBuf::from(format!("/tmp/cumulus-{}-{:08x}.sock", unsafe { libc_getuid() }, h as u32))
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

static OWNER: OnceLock<&'static str> = OnceLock::new();
static BUILD: OnceLock<u64> = OnceLock::new();

/// 本进程是哪一次编译出来的（用可执行文件的修改时间区分，进程启动时记下）
fn build_id() -> u64 {
    *BUILD.get_or_init(|| {
        std::env::current_exe()
            .and_then(std::fs::metadata)
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
            .unwrap_or(0)
    })
}
/// 最近一次收到请求的时刻（unix 秒），守护进程据此判断能不能退出
static LAST_REQUEST: AtomicU64 = AtomicU64::new(0);

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 距上次收到请求过了多少秒
pub fn idle_secs() -> u64 {
    now().saturating_sub(LAST_REQUEST.load(Ordering::SeqCst))
}

// ------------------------------------------------------------------ 服务端

/// 开始监听。`owner` 是 "app"（主程序）或 "guard"（后台守护进程）。
/// 主程序会等旧的掌管进程退出后抢过来；守护进程发现已经有人掌管就返回错误
pub fn serve(engine: Engine, owner: &'static str) -> Result<()> {
    let _ = OWNER.set(owner);
    build_id();
    LAST_REQUEST.store(now(), Ordering::SeqCst);
    let p = sock_path();
    std::fs::create_dir_all(config::dir())?;
    let t = Instant::now();
    while UnixStream::connect(&p).is_ok() {
        if owner != "app" {
            bail!("已经有进程在掌管（{}）", p.display());
        }
        if t.elapsed() > Duration::from_secs(3) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = std::fs::remove_file(&p);
    let listener = UnixListener::bind(&p)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let e = engine.clone();
            std::thread::spawn(move || handle_conn(&e, s));
        }
    });
    Ok(())
}

fn handle_conn(e: &Engine, s: UnixStream) {
    let mut line = String::new();
    let Ok(w) = s.try_clone() else { return };
    if BufReader::new(s).read_line(&mut line).is_err() {
        return;
    }
    LAST_REQUEST.store(now(), Ordering::SeqCst);
    let reply = match serde_json::from_str::<Value>(&line) {
        Ok(req) => match dispatch(e, &req) {
            Ok(Value::Object(mut m)) => {
                m.insert("ok".into(), Value::Bool(true));
                Value::Object(m)
            }
            Ok(v) => json!({"ok": true, "result": v}),
            Err(err) => json!({"ok": false, "error": err.to_string()}),
        },
        Err(err) => json!({"ok": false, "error": format!("请求不是 JSON：{err}")}),
    };
    let mut w = w;
    let _ = writeln!(w, "{reply}");
}

pub fn phase_key(p: &Phase) -> &'static str {
    match p {
        Phase::Checking => "checking",
        Phase::Off => "off",
        Phase::Creating => "creating",
        Phase::Connecting => "connecting",
        Phase::Mounting => "mounting",
        Phase::Booting => "booting",
        Phase::Running => "running",
        Phase::Stopping => "stopping",
        Phase::Failed(_) => "failed",
    }
}

fn wait_checked(e: &Engine) -> Phase {
    let t = Instant::now();
    loop {
        let p = e.state().phase();
        if p != Phase::Checking || t.elapsed() > Duration::from_secs(120) {
            return p;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 要求 VM 在运行。刚起来的守护进程正在重新接上 VM（连接、确认 ComfyUI）时等它一会儿，而不是直接报错
fn need_running(e: &Engine) -> Result<()> {
    let t = Instant::now();
    loop {
        let (p, auth) = {
            let s = e.state();
            (s.phase(), s.drive_auth_url.is_some())
        };
        match p {
            Phase::Running => return Ok(()),
            Phase::Mounting if auth => bail!("正在等 Google Drive 授权（浏览器里点「允许」后运行 cumulus drive-continue）"),
            Phase::Checking | Phase::Connecting | Phase::Mounting | Phase::Booting if t.elapsed() < Duration::from_secs(180) => {
                std::thread::sleep(Duration::from_millis(500))
            }
            p => bail!("VM 没有在运行（{}），先运行 cumulus up", p.label()),
        }
    }
}

fn split_key(key: &str) -> Result<(String, String)> {
    match key.split_once('/') {
        Some((c, n)) if !c.is_empty() && !n.is_empty() && !key.split('/').any(|x| x == "..") => Ok((c.into(), n.into())),
        _ => bail!("模型要写成 类别/文件名，例如 checkpoints/sdxl.safetensors（得到的是：{key}）"),
    }
}

fn str_arg<'a>(req: &'a Value, k: &str) -> Result<&'a str> {
    req.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("缺少参数 {k}"))
}

fn dispatch(e: &Engine, req: &Value) -> Result<Value> {
    let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("");
    match cmd {
        "ping" => Ok(json!({"owner": OWNER.get().copied().unwrap_or("?"), "pid": std::process::id(), "build": build_id()})),
        // 只有守护进程会退出（命令行发现它是旧版本时用）；VM 照常运行，新进程会重新接上
        "quit" => {
            if OWNER.get().copied() != Some("guard") {
                bail!("窗口里的程序不能这样退出");
            }
            let e = e.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                let _ = std::fs::remove_file(sock_path());
                e.shutdown_local();
                std::process::exit(0);
            });
            Ok(json!({}))
        }
        "status" => Ok(status(e)),
        "wait_running" => {
            need_running(e)?;
            Ok(status(e))
        }
        "logs" => {
            let s = e.state();
            let first = s.log_seq - s.log.len() as u64;
            let since = req.get("since").and_then(Value::as_u64);
            let n = req.get("n").and_then(Value::as_u64).unwrap_or(50) as usize;
            let lines: Vec<&String> = match since {
                Some(x) => s.log.iter().skip(x.saturating_sub(first) as usize).collect(),
                None => s.log.iter().skip(s.log.len().saturating_sub(n)).collect(),
            };
            Ok(json!({"lines": lines, "next": s.log_seq}))
        }
        "up" => {
            let mut cfg = e.config();
            let phase = wait_checked(e);
            if let Some(g) = req.get("gpu").and_then(Value::as_str) {
                let g = g.to_ascii_uppercase();
                if !GPUS.contains(&g.as_str()) {
                    bail!("不认识的 GPU：{g}（可选 {}）", GPUS.join(" / "));
                }
                if g != cfg.gpu {
                    if phase.busy() {
                        bail!("VM 已经开着（{}），要换 GPU 先 cumulus down", e.state().hardware);
                    }
                    cfg.gpu = g;
                    e.set_config(cfg);
                }
            }
            match phase {
                Phase::Running => Ok(json!({"started": false, "note": "已经在运行"})),
                p if p.busy() && p != Phase::Stopping => Ok(json!({"started": false, "note": format!("正在进行：{}", p.label())})),
                _ => {
                    e.start();
                    Ok(json!({"started": true}))
                }
            }
        }
        "down" => match wait_checked(e) {
            Phase::Off => Ok(json!({"stopping": false, "note": "本来就没开机"})),
            _ => {
                e.stop(None);
                Ok(json!({"stopping": true}))
            }
        },
        "drive_continue" => {
            e.drive_continue();
            Ok(json!({}))
        }
        "extend" => {
            e.extend();
            Ok(json!({"idle_left_s": e.idle_left().map(|d| d.as_secs())}))
        }
        "models" => {
            need_running(e)?;
            let empty = e.state().models.is_empty();
            if empty || req.get("refresh").and_then(Value::as_bool).unwrap_or(false) {
                e.load_models();
                let t = Instant::now();
                std::thread::sleep(Duration::from_millis(200));
                while e.state().models_loading && t.elapsed() < Duration::from_secs(150) {
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
            let cfg = e.config();
            let s = e.state();
            if let Some(err) = &s.models_error {
                bail!("读取模型列表失败：{err}");
            }
            let categories: Vec<&str> = s.models.iter().filter(|m| m.name.is_empty()).map(|m| m.category.as_str()).collect();
            let models: Vec<Value> = s
                .models
                .iter()
                .filter(|m| !m.name.is_empty())
                .map(|m| json!({"key": m.key(), "category": m.category, "name": m.name, "size": m.size, "on_ssd": m.ssd, "ssd_selected": cfg.ssd_models.contains(&m.key())}))
                .collect();
            Ok(json!({"root": cfg.models_root(), "categories": categories, "models": models}))
        }
        "download" => {
            need_running(e)?;
            let url = str_arg(req, "url")?;
            let cat = str_arg(req, "category")?;
            if cat.contains("..") || cat.starts_with('/') {
                bail!("类别名不对：{cat}");
            }
            let name = req.get("name").and_then(Value::as_str).unwrap_or("");
            let id = e.download_model(url.into(), cat.into(), name.into()).ok_or_else(|| anyhow!("没有连接 VM"))?;
            Ok(json!({"id": id}))
        }
        "tasks" => Ok(json!({"tasks": tasks(e)})),
        "cancel" => {
            let id = req.get("id").and_then(Value::as_u64).ok_or_else(|| anyhow!("缺少参数 id"))?;
            if !e.state().downloads.iter().any(|d| d.id == id) {
                bail!("没有编号为 {id} 的任务");
            }
            e.cancel_download(id);
            Ok(json!({}))
        }
        "clear_tasks" => {
            e.clear_finished_downloads();
            Ok(json!({}))
        }
        "ssd" => {
            let cfg = e.config();
            if !cfg.ssd_enabled() {
                bail!("没有接 Google Drive，模型本来就在本地，不需要复制到 SSD");
            }
            let (cat, name) = split_key(str_arg(req, "key")?)?;
            let on = req.get("on").and_then(Value::as_bool).unwrap_or(true);
            e.ssd_select(&cat, &name, on);
            let id = e.state().downloads.iter().find(|d| d.to_ssd && d.active() && d.category == cat && d.name == name).map(|d| d.id);
            let note = match (on, id) {
                (true, None) if e.state().phase() != Phase::Running => "已记住，开机后自动复制",
                _ => "",
            };
            Ok(json!({"id": id, "note": note}))
        }
        "workflows" => {
            need_running(e)?;
            let list = e.workflows_now().map_err(|m| anyhow!("读取工作流失败：{m}"))?;
            let v: Vec<Value> = list.iter().map(|(n, t)| json!({"name": n, "mtime": *t as u64})).collect();
            Ok(json!({"dir": e.config().workflows_dir(), "workflows": v}))
        }
        // 工作流里用到的模型 → 复制到 SSD。text = 工作流 JSON 原文，或 name = ComfyUI 里保存的工作流
        "ssd_from_workflow" => {
            if !e.config().ssd_enabled() {
                bail!("没有接 Google Drive，模型本来就在本地，不需要复制到 SSD");
            }
            need_running(e)?;
            let text = match (req.get("text").and_then(Value::as_str), req.get("name").and_then(Value::as_str)) {
                (Some(t), _) => t.to_string(),
                (None, Some(n)) => {
                    let names = e.workflows_now().unwrap_or_default();
                    // 可以省略 .json，也可以只写文件名
                    let pick = names
                        .iter()
                        .map(|x| x.0.clone())
                        .find(|x| *x == n || *x == format!("{n}.json") || x.rsplit('/').next() == Some(n) || x.rsplit('/').next().map(|b| b == format!("{n}.json")).unwrap_or(false))
                        .ok_or_else(|| anyhow!("ComfyUI 里没有叫 {n} 的工作流（cumulus workflows 查看）"))?;
                    e.read_workflow(&pick)?
                }
                _ => bail!("缺少参数 text 或 name"),
            };
            let plan = e.plan_workflow(&text)?;
            let mut v = plan.to_json();
            if req.get("apply").and_then(Value::as_bool).unwrap_or(false) {
                let ids = e.apply_plan(&plan)?;
                v["tasks"] = json!(ids);
            }
            Ok(v)
        }
        "ssd_sync" => {
            need_running(e)?;
            e.ssd_sync();
            Ok(json!({}))
        }
        "delete_model" => {
            need_running(e)?;
            let (cat, name) = split_key(str_arg(req, "key")?)?;
            if !e.state().models.iter().any(|m| m.category == cat && m.name == name) {
                bail!("模型列表里没有 {cat}/{name}（先运行 cumulus models --refresh）");
            }
            e.delete_model(cat, name);
            Ok(json!({}))
        }
        "outputs" => {
            if req.get("rescan").and_then(Value::as_bool).unwrap_or(false) {
                e.load_local_outputs();
            }
            let n = req.get("n").and_then(Value::as_u64).unwrap_or(20) as usize;
            let s = e.state();
            let list: Vec<Value> = s
                .outputs
                .iter()
                .take(n)
                .map(|o| {
                    let t = o.at.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    json!({"path": o.local.to_string_lossy(), "rel": o.rel, "size": o.size, "mtime": t})
                })
                .collect();
            Ok(json!({"dir": e.config().output_local, "outputs": list, "syncing": s.syncing}))
        }
        "config_get" => Ok(config_json(&e.config())),
        "config_set" => {
            let key = str_arg(req, "key")?;
            let value = req.get("value").and_then(Value::as_str).unwrap_or("");
            let c = config_set(&e.config(), key, value, e.state().phase().busy())?;
            e.set_config(c.clone());
            Ok(config_json(&c))
        }
        "" => bail!("缺少 cmd"),
        other => bail!("不认识的命令：{other}"),
    }
}

fn opt_secs(d: Option<Duration>) -> Value {
    d.map(|d| json!(d.as_secs())).unwrap_or(Value::Null)
}

fn status(e: &Engine) -> Value {
    let cfg = e.config();
    let idle = e.idle_left();
    let max = e.max_left();
    let s = e.state();
    let phase = s.phase();
    let running = phase == Phase::Running;
    json!({
        "owner": OWNER.get().copied().unwrap_or("?"),
        "session": cfg.session,
        "phase": phase_key(&phase),
        "phase_label": phase.label(),
        "error": match &phase { Phase::Failed(m) => json!(m), _ => Value::Null },
        "billing": phase.busy(),
        "step": s.step,
        "hardware": s.hardware,
        "gpu_setting": cfg.gpu,
        "comfy_up": s.comfy_up,
        "comfy_url": if running { json!(format!("http://127.0.0.1:{}", s.local_port)) } else { Value::Null },
        "local_port": s.local_port,
        "drive": {"enabled": cfg.mount_drive, "mounted": s.drive_ok, "root": cfg.drive_root, "auth_url": s.drive_auth_url},
        "uptime_s": opt_secs(s.up_since.map(|t| t.elapsed())),
        "idle_shutdown_in_s": opt_secs(idle),
        "max_shutdown_in_s": opt_secs(max),
        "cu": {"balance": s.usage.balance, "rate_per_hour": s.usage.rate},
        "gpu": s.gpu.as_ref().map(|g| json!({"name": g.name, "util": g.util, "mem_used_mb": g.mem_used, "mem_total_mb": g.mem_total, "temp_c": g.temp, "power_w": g.power})),
        "mem": {"used": s.mem.0, "total": s.mem.1},
        "disk": {"used": s.disk.0, "total": s.disk.1},
        "queue": {"running": s.queue.0, "pending": s.queue.1},
        "tasks_active": s.downloads.iter().filter(|d| d.active()).count(),
        "auto_stopped": s.auto_stopped,
        "output_dir": cfg.output_local,
        "log_seq": s.log_seq,
    })
}

fn tasks(e: &Engine) -> Vec<Value> {
    e.state()
        .downloads
        .iter()
        .map(|d| {
            let (st, err) = match &d.status {
                DlStatus::Queued => ("queued", None),
                DlStatus::Running => ("running", None),
                DlStatus::Done => ("done", None),
                DlStatus::Failed(m) => ("failed", Some(m.clone())),
                DlStatus::Cancelled => ("cancelled", None),
            };
            json!({
                "id": d.id,
                "kind": if d.to_ssd { "ssd" } else { "download" },
                "key": format!("{}/{}", d.category, d.name),
                "source": d.url,
                "status": st,
                "error": err,
                "done": d.done,
                "total": d.total,
                "speed_bps": d.speed as u64,
            })
        })
        .collect()
}

/// 设置项：令牌不回传，只说有没有设
const SECRET: &[&str] = &["hf_token", "civitai_token"];

fn config_json(c: &Config) -> Value {
    let mut m = Map::new();
    for l in c.serialize().lines() {
        if let Some((k, v)) = l.split_once('=') {
            let v = if SECRET.contains(&k) { json!(if v.is_empty() { "" } else { "（已设置）" }) } else { json!(v) };
            m.insert(k.into(), v);
        }
    }
    json!({"config": m, "path": config::dir().join("config").to_string_lossy()})
}

fn config_set(old: &Config, key: &str, value: &str, busy: bool) -> Result<Config> {
    let keys: Vec<String> = old.serialize().lines().filter_map(|l| l.split_once('=').map(|x| x.0.to_string())).collect();
    if !keys.iter().any(|k| k == key) {
        bail!("没有这个设置项：{key}（可选：{}）", keys.join(", "));
    }
    if value.contains('\n') {
        bail!("值里不能有换行");
    }
    if busy && matches!(key, "session" | "local_port") {
        bail!("VM 开着的时候不能改 {key}，先 cumulus down");
    }
    let v = match value.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" if matches!(key, "mount_drive" | "output_to_drive" | "ssd_auto") => "1".to_string(),
        "false" | "off" | "no" if matches!(key, "mount_drive" | "output_to_drive" | "ssd_auto") => "0".to_string(),
        _ if key == "gpu" => value.to_ascii_uppercase(),
        _ => value.to_string(),
    };
    let c = Config::parse(&format!("{}{key}={v}\n", old.serialize()));
    // 解析时不合法的值会被忽略：对比一下有没有真的变成想要的值
    let got = c.serialize().lines().find_map(|l| l.strip_prefix(&format!("{key}="))).unwrap_or("").to_string();
    let same = got == v || matches!((got.parse::<f64>(), v.parse::<f64>()), (Ok(a), Ok(b)) if a == b);
    if !same {
        bail!("{key} 的值不合法：{value}");
    }
    Ok(c)
}

// ------------------------------------------------------------------ 客户端

fn try_connect() -> Option<UnixStream> {
    UnixStream::connect(sock_path()).ok()
}

/// 起一个后台守护进程（无窗口），由它掌管 VM：自动关机、出图同步、响应命令行
pub fn spawn_guard() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(config::dir())?;
    let log = std::fs::File::create(config::dir().join("guard.log"))?;
    let log2 = log.try_clone()?;
    std::process::Command::new(exe).arg("--guard").stdin(std::process::Stdio::null()).stdout(log).stderr(log2).process_group(0).spawn()?;
    Ok(())
}

fn ask(mut s: UnixStream, req: &Value) -> Result<Value> {
    s.set_read_timeout(Some(Duration::from_secs(300)))?;
    writeln!(s, "{req}")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(|e| anyhow!("回复不是 JSON（{e}）：{line}"))
}

/// 后台守护进程是旧版本编译的（更新程序之后）：空闲时换成新的。VM 不受影响，新进程会重新接上
fn replace_stale_guard() {
    static DONE: OnceLock<()> = OnceLock::new();
    if DONE.set(()).is_err() {
        return;
    }
    let Some(s) = try_connect() else { return };
    let Ok(p) = ask(s, &json!({"cmd": "ping"})) else { return };
    if p["owner"] != "guard" || p["build"].as_u64() == Some(build_id()) {
        return;
    }
    let Some(Ok(st)) = try_connect().map(|s| ask(s, &json!({"cmd": "status"}))) else { return };
    let idle = matches!(st["phase"].as_str(), Some("off" | "running" | "failed")) && st["tasks_active"] == 0;
    if !idle {
        eprintln!("（后台进程是旧版本，正在忙，等它空闲后再更新）");
        return;
    }
    if try_connect().map(|s| ask(s, &json!({"cmd": "quit"}))).is_none() {
        return;
    }
    let t = Instant::now();
    while try_connect().is_some() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
}

/// 找到掌管进程；没有就起一个后台守护进程
fn connect() -> Result<UnixStream> {
    replace_stale_guard();
    if let Some(s) = try_connect() {
        return Ok(s);
    }
    spawn_guard()?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(150));
        if let Some(s) = try_connect() {
            return Ok(s);
        }
    }
    bail!("后台进程没有起来，看看 {}", config::dir().join("guard.log").display())
}

/// 发一个请求，返回回复（ok=false 时返回错误）
pub fn call(req: Value) -> Result<Value> {
    let v = ask(connect()?, &req)?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(v)
    } else {
        bail!("{}", v.get("error").and_then(Value::as_str).unwrap_or("未知错误"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_values() {
        let c = Config::default();
        assert_eq!(config_set(&c, "gpu", "a100", false).unwrap().gpu, "A100");
        assert!(config_set(&c, "gpu", "X1", false).is_err());
        assert_eq!(config_set(&c, "idle_min", "15", false).unwrap().idle_min, 15);
        assert!(config_set(&c, "idle_min", "abc", false).is_err());
        assert_eq!(config_set(&c, "max_hours", "6.0", false).unwrap().max_hours, 6.0);
        assert!(!config_set(&c, "ssd_auto", "off", false).unwrap().ssd_auto);
        assert!(config_set(&c, "nope", "1", false).is_err());
        assert!(config_set(&c, "session", "x", true).is_err());
        let j = config_json(&Config { hf_token: "hf_secret".into(), ..c });
        assert_eq!(j["config"]["hf_token"], "（已设置）");
        assert!(split_key("loras/../x").is_err());
        assert_eq!(split_key("loras/sub/x.safetensors").unwrap(), ("loras".into(), "sub/x.safetensors".into()));
    }
}
