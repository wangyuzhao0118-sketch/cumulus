//! 后台引擎：开机 / 关机流程、状态轮询、自动关机、出图同步、模型下载。
//!
//! 所有耗时操作都在后台线程里做，结果写进共享的 `State`；界面定时读取 `State::version`，
//! 变了就重绘。界面只调用这里的方法发指令，不直接碰 colab / ssh。
use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::remote::{self, Ssh, Usage, fmt_bytes, sh_quote};

const BOOT_SH: &str = include_str!("../kit/boot.sh");
const POLL_PY: &str = include_str!("../kit/poll.py");
const DL_PY: &str = include_str!("../kit/dl.py");
const CP_PY: &str = include_str!("../kit/cp.py");
/// VM 本地 SSD 上的模型目录（boot.sh 把它排在 Drive 前面，同名模型优先从这里读）
pub const SSD_ROOT: &str = "/content/cumulus_ssd";
/// 同时复制到 SSD 的文件数。cp.py 走 Drive API 时一个文件就有约 300 MB/s，再多也快不了多少；
/// 退回从挂载目录复制时（每个文件 35–50 MB/s）多几个会合计更快
const SSD_PARALLEL: usize = 2;

enum DriveResult {
    Mounted,
    AuthFailed(String),
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// 启动时查询 colab 上有没有已经在跑的会话
    Checking,
    Off,
    /// colab new
    Creating,
    /// 建立 ssh 连接和端口转发
    Connecting,
    /// 挂载 Google Drive（可能要在浏览器里授权）
    Mounting,
    /// 运行 boot.sh：安装、启动 ComfyUI
    Booting,
    Running,
    Stopping,
    Failed(String),
}

impl Phase {
    pub fn label(&self) -> String {
        match self {
            Phase::Checking => "正在查询…".into(),
            Phase::Off => "未开机".into(),
            Phase::Creating => "正在分配 VM…".into(),
            Phase::Connecting => "正在连接…".into(),
            Phase::Mounting => "正在挂载 Google Drive…".into(),
            Phase::Booting => "正在启动 ComfyUI…".into(),
            Phase::Running => "运行中".into(),
            Phase::Stopping => "正在关机…".into(),
            Phase::Failed(m) => format!("出错：{m}"),
        }
    }

    /// VM 是否（可能）在计费
    pub fn busy(&self) -> bool {
        matches!(self, Phase::Creating | Phase::Connecting | Phase::Mounting | Phase::Booting | Phase::Running | Phase::Stopping)
    }
}

/// 开机流程的步骤，界面上画成进度条
pub const STEPS: &[(&str, &str)] = &[
    ("create", "分配 VM"),
    ("connect", "连接"),
    ("drive", "挂载 Drive"),
    ("install", "安装 ComfyUI"),
    ("nodes", "自定义节点"),
    ("start", "启动"),
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gpu {
    pub name: String,
    pub util: f32,
    pub mem_used: f32,
    pub mem_total: f32,
    pub temp: f32,
    pub power: f32,
}

#[derive(Clone, Debug)]
pub struct Output {
    pub rel: String,
    pub size: u64,
    pub local: PathBuf,
    pub at: SystemTime,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DlStatus {
    /// 排队等前面的任务（复制到 SSD 同时最多做 SSD_PARALLEL 个）
    Queued,
    Running,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct Download {
    pub id: u64,
    pub url: String,
    pub category: String,
    pub name: String,
    pub done: u64,
    pub total: u64,
    pub speed: f64,
    pub status: DlStatus,
    /// true = 从 Drive 复制到本地 SSD；false = 从网上下载
    pub to_ssd: bool,
    pub(crate) cancel: Arc<AtomicBool>,
}

impl Download {
    pub fn active(&self) -> bool {
        matches!(self.status, DlStatus::Queued | DlStatus::Running)
    }
}

#[derive(Clone, Debug)]
pub struct Model {
    pub category: String,
    pub name: String,
    pub size: u64,
    /// 已经复制到本地 SSD
    pub ssd: bool,
}

impl Model {
    pub fn key(&self) -> String {
        format!("{}/{}", self.category, self.name)
    }
}

#[derive(Default)]
pub struct State {
    pub version: u64,
    pub phase: Option<Phase>,
    /// 当前进行到的步骤 key，以及已完成的步骤和耗时
    pub step: Option<String>,
    pub steps_done: Vec<(String, f32)>,
    pub step_started: Option<Instant>,
    pub boot_started: Option<Instant>,
    pub boot_secs: Option<f32>,
    pub hardware: String,
    /// 本次开机时间（本机时钟）；重新接上已有会话时为接上的时间
    pub up_since: Option<Instant>,
    pub usage: Usage,
    pub usage_at: Option<Instant>,
    pub gpu: Option<Gpu>,
    pub mem: (u64, u64),
    pub disk: (u64, u64),
    pub comfy_up: bool,
    pub queue: (u32, u32),
    /// 最近一次有活动（排队、GPU 在算）的时间
    pub last_active: Option<Instant>,
    pub drive_auth_url: Option<String>,
    pub drive_ok: bool,
    pub log: VecDeque<String>,
    pub comfy_log: Vec<String>,
    pub outputs: Vec<Output>,
    pub syncing: Option<String>,
    pub downloads: Vec<Download>,
    pub models: Vec<Model>,
    pub models_loading: bool,
    pub models_error: Option<String>,
    pub notices: Vec<(String, bool)>,
    /// 自动关机的原因（关机后在界面上显示）
    pub auto_stopped: Option<String>,
    pub local_port: u16,
}

impl State {
    pub fn phase(&self) -> Phase {
        self.phase.clone().unwrap_or(Phase::Checking)
    }
}

#[derive(Clone)]
pub struct Engine {
    st: Arc<Mutex<State>>,
    cfg: Arc<Mutex<Config>>,
    ssh: Arc<Mutex<Option<Arc<Ssh>>>>,
    master: Arc<Mutex<Option<Child>>>,
    drive_stdin: Arc<Mutex<Option<ChildStdin>>>,
    /// 每次开机 / 关机递增；后台线程发现代数变了就退出
    gen_: Arc<AtomicU64>,
    want_log: Arc<AtomicBool>,
    seq: Arc<AtomicU64>,
    /// 复制到 SSD 的并发名额
    ssd_slots: Arc<(Mutex<usize>, Condvar)>,
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl Engine {
    pub fn new(cfg: Config) -> Self {
        let e = Engine {
            st: Arc::new(Mutex::new(State::default())),
            cfg: Arc::new(Mutex::new(cfg)),
            ssh: Arc::new(Mutex::new(None)),
            master: Arc::new(Mutex::new(None)),
            drive_stdin: Arc::new(Mutex::new(None)),
            gen_: Arc::new(AtomicU64::new(0)),
            want_log: Arc::new(AtomicBool::new(false)),
            seq: Arc::new(AtomicU64::new(1)),
            ssd_slots: Arc::new((Mutex::new(SSD_PARALLEL), Condvar::new())),
        };
        e.load_local_outputs();
        e
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap()
    }

    pub fn config(&self) -> Config {
        self.cfg.lock().unwrap().clone()
    }

    pub fn set_config(&self, c: Config) {
        c.save();
        *self.cfg.lock().unwrap() = c;
        self.load_local_outputs();
    }

    fn edit(&self, f: impl FnOnce(&mut State)) {
        let mut s = self.st.lock().unwrap();
        f(&mut s);
        s.version += 1;
    }

    fn set_phase(&self, p: Phase) {
        // 调试：CUMULUS_TRACE=1 时把状态变化打到终端（窗口不在屏幕上时界面不重绘，用它确认进度）
        if std::env::var_os("CUMULUS_TRACE").is_some() {
            eprintln!("[phase] {}", p.label());
        }
        self.edit(|s| s.phase = Some(p));
    }

    pub fn notify(&self, text: impl Into<String>, err: bool) {
        let t = text.into();
        self.edit(|s| s.notices.push((t, err)));
    }

    fn log(&self, line: impl Into<String>) {
        let l = line.into();
        self.edit(|s| {
            s.log.push_back(l);
            while s.log.len() > 3000 {
                s.log.pop_front();
            }
        });
    }

    fn step(&self, key: &str) {
        let k = key.to_string();
        self.edit(|s| {
            if let (Some(prev), Some(t)) = (s.step.take(), s.step_started) {
                if prev != k {
                    s.steps_done.push((prev, t.elapsed().as_secs_f32()));
                } else {
                    s.step = Some(prev);
                }
            }
            if s.step.is_none() {
                s.step = Some(k);
                s.step_started = Some(Instant::now());
            }
        });
    }

    fn finish_steps(&self) {
        self.edit(|s| {
            if let (Some(prev), Some(t)) = (s.step.take(), s.step_started) {
                s.steps_done.push((prev, t.elapsed().as_secs_f32()));
            }
            s.boot_secs = s.boot_started.map(|t| t.elapsed().as_secs_f32());
        });
    }

    fn alive(&self, g: u64) -> bool {
        self.gen_.load(Ordering::SeqCst) == g
    }

    pub fn set_want_log(&self, on: bool) {
        self.want_log.store(on, Ordering::SeqCst);
    }

    fn ssh(&self) -> Option<Arc<Ssh>> {
        self.ssh.lock().unwrap().clone()
    }

    // ------------------------------------------------------------ 生命周期

    /// 启动时：看 colab 上是不是已经有我们的会话在跑，有就接上
    pub fn check_existing(&self) {
        let e = self.clone();
        let g = self.gen_.fetch_add(1, Ordering::SeqCst) + 1;
        self.set_phase(Phase::Checking);
        std::thread::spawn(move || {
            e.refresh_usage();
            if !remote::colab_installed() {
                e.set_phase(Phase::Failed("没有找到 colab 命令，请先安装：uv tool install google-colab-cli".into()));
                return;
            }
            let name = e.config().session;
            match remote::sessions() {
                Ok(list) => match list.into_iter().find(|s| s.name == name) {
                    Some(info) if e.alive(g) => {
                        e.edit(|s| {
                            s.hardware = info.hardware.clone();
                            s.up_since = Some(Instant::now());
                        });
                        e.log(format!("发现正在运行的会话 {name}（{}），重新接上", info.hardware));
                        e.boot(g, false);
                    }
                    _ => e.set_phase(Phase::Off),
                },
                Err(err) => {
                    let m = err.to_string();
                    if m.contains("auth") || m.contains("credential") || m.contains("token") {
                        e.set_phase(Phase::Failed(format!("colab 未登录：在终端运行一次 colab sessions 完成登录（{m}）")));
                    } else {
                        e.log(format!("查询会话失败：{m}"));
                        e.set_phase(Phase::Off);
                    }
                }
            }
        });
    }

    /// 开机：分配 VM → 连接 → 挂载 Drive → 安装并启动 ComfyUI
    pub fn start(&self) {
        let p = self.state().phase();
        if p.busy() && p != Phase::Stopping {
            return;
        }
        let g = self.gen_.fetch_add(1, Ordering::SeqCst) + 1;
        let e = self.clone();
        self.edit(|s| {
            s.phase = Some(Phase::Creating);
            s.step = None;
            s.steps_done.clear();
            s.boot_started = Some(Instant::now());
            s.boot_secs = None;
            s.auto_stopped = None;
            s.drive_ok = false;
            s.drive_auth_url = None;
            s.log.clear();
            s.comfy_up = false;
            s.gpu = None;
        });
        std::thread::spawn(move || {
            let cfg = e.config();
            e.step("create");
            e.log(format!("$ colab new -s {} {}", cfg.session, if cfg.gpu == "CPU" { String::new() } else { format!("--gpu {}", cfg.gpu) }));
            let existing = remote::sessions().ok().and_then(|l| l.into_iter().find(|s| s.name == cfg.session));
            if let Some(info) = existing {
                e.log(format!("会话已存在（{}），直接使用", info.hardware));
                e.edit(|s| s.hardware = info.hardware);
            } else if let Err(err) = remote::new_session(&cfg.session, &cfg.gpu) {
                let m = err.to_string();
                e.log(m.clone());
                let hint = if m.contains("quota") || m.contains("unavailable") || m.contains("capacity") || m.contains("Unavailable") {
                    format!("{} 暂时分配不到，可以换一种 GPU 再试", cfg.gpu)
                } else {
                    remote::last_lines(&m, 2)
                };
                e.set_phase(Phase::Failed(hint));
                return;
            } else {
                e.edit(|s| s.hardware = cfg.gpu.clone());
            }
            e.edit(|s| s.up_since = Some(Instant::now()));
            if !e.alive(g) {
                return;
            }
            e.refresh_usage();
            e.boot(g, true);
        });
    }

    /// 连接 + 挂载 + 启动 ComfyUI（新开机和重新接上都走这里），成功后进入监控循环
    fn boot(&self, g: u64, fresh: bool) {
        let cfg = self.config();
        if !fresh {
            self.edit(|s| {
                s.boot_started = Some(Instant::now());
                s.steps_done.clear();
                s.step = None;
            });
        }
        self.set_phase(Phase::Connecting);
        self.step("connect");
        let ssh = match Ssh::new(&cfg.session) {
            Ok(s) => Arc::new(s),
            Err(err) => return self.set_phase(Phase::Failed(err.to_string())),
        };
        // 先关掉之前留下的连接（它可能还占着本机端口），再选端口：被别的程序占用时往后找
        ssh.cleanup();
        let mut port = cfg.local_port;
        while !remote::port_free(port) && port < cfg.local_port + 50 {
            port += 1;
        }
        if port != cfg.local_port {
            self.log(format!("本机端口 {} 被占用，改用 {port}", cfg.local_port));
        }
        let mut last_err = String::new();
        let mut master = None;
        for attempt in 1..=4 {
            if !self.alive(g) {
                return;
            }
            self.log(format!("ssh 连接（第 {attempt} 次）…"));
            match ssh.connect(port) {
                Ok(c) => {
                    master = Some(c);
                    break;
                }
                Err(err) => {
                    last_err = err.to_string();
                    self.log(last_err.clone());
                    std::thread::sleep(Duration::from_secs(4));
                }
            }
        }
        let Some(master) = master else {
            return self.set_phase(Phase::Failed(format!("连不上 VM：{last_err}")));
        };
        *self.master.lock().unwrap() = Some(master);
        *self.ssh.lock().unwrap() = Some(ssh.clone());
        self.edit(|s| s.local_port = port);
        self.log(format!("已连接，ComfyUI 将转发到 http://127.0.0.1:{port}"));

        if cfg.mount_drive {
            self.set_phase(Phase::Mounting);
            self.step("drive");
            if !self.mount_drive(g, &ssh) {
                return;
            }
        }
        if !self.alive(g) {
            return;
        }

        self.set_phase(Phase::Booting);
        self.step("install");
        let nodes = cfg.custom_nodes.split_whitespace().collect::<Vec<_>>().join("\n");
        let mut script = String::new();
        let drive_root = if cfg.mount_drive { cfg.drive_root.clone() } else { String::new() };
        script.push_str(&format!("export DRIVE_ROOT={}\n", sh_quote(&drive_root)));
        script.push_str(&format!("export CUSTOM_NODES={}\n", sh_quote(&nodes)));
        script.push_str(&format!("export EXTRA_ARGS={}\n", sh_quote(&cfg.extra_args)));
        script.push_str(&format!("export OUTPUT_DRIVE={}\n", if cfg.output_to_drive { 1 } else { 0 }));
        // 记录开机时刻，出图同步只拿这之后的文件（重新接上时沿用原来的）
        script.push_str(
            "mkdir -p /content/.cumulus; [ -f /content/.cumulus/since ] || cp /content/.nimbus/since /content/.cumulus/since 2>/dev/null || date +%s > /content/.cumulus/since\n",
        );
        script.push_str(BOOT_SH);
        let mut ready = false;
        let mut fail = None;
        let me = self.clone();
        let res = ssh.stream("bash -s", Some(script.into_bytes()), |line| {
            if let Some(rest) = line.strip_prefix("@@STAGE ") {
                let (key, label) = rest.split_once(' ').unwrap_or((rest, ""));
                me.step(key);
                me.log(format!("── {label}"));
            } else if line.starts_with("@@READY") {
                ready = true;
            } else if let Some(m) = line.strip_prefix("@@FAIL ") {
                fail = Some(m.to_string());
            } else {
                me.log(line.to_string());
            }
        });
        if !self.alive(g) {
            return;
        }
        if !ready {
            let m = fail.or_else(|| res.err().map(|e| e.to_string())).unwrap_or_else(|| "启动脚本中断".into());
            self.log(format!("失败：{m}"));
            return self.set_phase(Phase::Failed(m));
        }
        self.finish_steps();
        self.edit(|s| {
            s.comfy_up = true;
            s.last_active = Some(Instant::now());
            if s.up_since.is_none() {
                s.up_since = Some(Instant::now());
            }
        });
        self.set_phase(Phase::Running);
        let secs = self.state().boot_secs.unwrap_or(0.0);
        self.log(format!("ComfyUI 就绪，用时 {secs:.0} 秒"));
        self.notify("ComfyUI 已就绪", false);
        let cfg = self.config();
        if cfg.ssd_enabled() && cfg.ssd_auto && !cfg.ssd_models.is_empty() {
            self.log(format!("把选中的 {} 个模型复制到本地 SSD", cfg.ssd_models.len()));
            self.ssd_sync();
        }
        self.monitor(g);
    }

    /// 挂载 Drive。需要授权时把地址交给界面，等用户点「我已授权」；
    /// 授权没完成（Colab 返回 400）就重新请求一次授权，而不是直接失败
    fn mount_drive(&self, g: u64, ssh: &Ssh) -> bool {
        let cfg = self.config();
        let mounted = || ssh.run("mountpoint -q /content/drive && echo yes", None, Duration::from_secs(30)).map(|o| o.contains("yes")).unwrap_or(false);
        if mounted() {
            self.log("Google Drive 已挂载");
            self.edit(|s| s.drive_ok = true);
            return true;
        }
        let mut last_err = String::from("没有完成授权");
        for attempt in 1..=4 {
            if !self.alive(g) {
                return false;
            }
            if attempt > 1 {
                self.log(format!("授权还没完成，重新请求授权（第 {attempt} 次）"));
            }
            match self.drivemount_once(&cfg.session) {
                DriveResult::Mounted => break,
                DriveResult::AuthFailed(m) => last_err = m,
                DriveResult::Error(m) => {
                    last_err = m;
                    break;
                }
            }
        }
        if !self.alive(g) {
            return false;
        }
        if mounted() {
            self.edit(|s| s.drive_ok = true);
            self.log("Google Drive 已挂载");
            let root = cfg.drive_root.clone();
            if ssh.run(&format!("test -d {}", sh_quote(&root)), None, Duration::from_secs(30)).is_err() {
                self.log(format!("注意：Drive 上没有 {root}，会自动创建；模型请放到它下面的 models/<类别>/"));
                let _ = ssh.run(&format!("mkdir -p {}/models/checkpoints", sh_quote(&root)), None, Duration::from_secs(30));
            }
            true
        } else {
            self.set_phase(Phase::Failed(format!("挂载 Google Drive 失败：{last_err}。可以点「开机」重试（不会重开 VM），或在设置里关掉 Drive")));
            false
        }
    }

    /// 跑一次 `colab drivemount`
    fn drivemount_once(&self, session: &str) -> DriveResult {
        self.log(format!("$ colab drivemount -s {session}"));
        let mut child = match remote::drivemount(session) {
            Ok(c) => c,
            Err(err) => return DriveResult::Error(err.to_string()),
        };
        *self.drive_stdin.lock().unwrap() = child.stdin.take();
        // stderr 也要读掉，不然缓冲区满了会卡住
        if let Some(err) = child.stderr.take() {
            let me = self.clone();
            std::thread::spawn(move || {
                for l in BufReader::new(err).lines().map_while(Result::ok) {
                    let l = remote::strip_ansi(&l);
                    if !l.trim().is_empty() {
                        me.log(l);
                    }
                }
            });
        }
        let mut mounted = false;
        let mut auth_failed = None;
        let mut error = None;
        if let Some(out) = child.stdout.take() {
            // CLI 打印 "Press Enter after you have granted access... " 不带换行，按字节读
            let mut r = BufReader::new(out);
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                match std::io::Read::read(&mut r, &mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if byte[0] != b'\n' {
                    line.push(byte[0]);
                    if !line.ends_with(b"... ") {
                        continue;
                    }
                }
                let l = remote::strip_ansi(&String::from_utf8_lossy(&line)).trim().to_string();
                line.clear();
                if l.is_empty() {
                    continue;
                }
                if l.starts_with("https://accounts.google.com") {
                    self.edit(|s| s.drive_auth_url = Some(l.clone()));
                    self.log("需要在浏览器里授权 Google Drive（已自动打开授权页）");
                    continue;
                }
                if l.contains("Mounted at") {
                    mounted = true;
                }
                if l.contains("Error propagating") {
                    auth_failed = Some("浏览器里还没点「允许」".to_string());
                    // 后面是一大段 HTML 错误页，不显示
                    self.log(l);
                    break;
                }
                if l.contains("ValueError") || l.contains("Error:") {
                    error = Some(l.clone());
                }
                if l.starts_with('<') || l.contains("{margin:") {
                    continue;
                }
                self.log(l);
            }
        }
        if auth_failed.is_some() {
            let _ = child.kill();
        }
        let _ = child.wait();
        *self.drive_stdin.lock().unwrap() = None;
        self.edit(|s| s.drive_auth_url = None);
        if mounted {
            DriveResult::Mounted
        } else if let Some(m) = auth_failed {
            DriveResult::AuthFailed(m)
        } else {
            DriveResult::Error(error.unwrap_or_else(|| "colab drivemount 没有成功".into()))
        }
    }

    /// 用户在浏览器里授权完成
    pub fn drive_continue(&self) {
        if let Some(i) = self.drive_stdin.lock().unwrap().as_mut() {
            let _ = i.write_all(b"\n");
            let _ = i.flush();
        }
        self.edit(|s| s.drive_auth_url = None);
        self.log("继续挂载…");
    }

    /// 关机：先把最后的出图同步回来，再释放 VM
    pub fn stop(&self, reason: Option<String>) {
        let g = self.gen_.fetch_add(1, Ordering::SeqCst) + 1;
        let e = self.clone();
        self.edit(|s| {
            s.phase = Some(Phase::Stopping);
            s.auto_stopped = reason.clone();
            s.drive_auth_url = None;
        });
        if let Some(r) = &reason {
            self.log(format!("自动关机：{r}"));
        }
        std::thread::spawn(move || {
            if let Some(ssh) = e.ssh() {
                if ssh.alive() {
                    e.log("关机前同步出图…");
                    let deadline = Instant::now() + Duration::from_secs(90);
                    let _ = e.sync_outputs(&ssh, Some(deadline));
                }
            }
            if let Some(mut i) = e.drive_stdin.lock().unwrap().take() {
                let _ = i.write_all(b"\n");
            }
            let cfg = e.config();
            e.log(format!("$ colab stop -s {}", cfg.session));
            let res = remote::stop_session(&cfg.session);
            e.disconnect();
            if !e.alive(g) {
                return;
            }
            match res {
                Ok(()) => {
                    e.edit(|s| {
                        s.phase = Some(Phase::Off);
                        s.comfy_up = false;
                        s.gpu = None;
                        s.up_since = None;
                        s.queue = (0, 0);
                    });
                    e.log("VM 已释放");
                    e.notify(if reason.is_some() { "已自动关机" } else { "已关机" }, false);
                }
                Err(err) => e.set_phase(Phase::Failed(format!("关机失败：{err}。请再点一次关机，或运行 colab stop -s {}", cfg.session))),
            }
            e.refresh_usage();
        });
    }

    fn disconnect(&self) {
        if let Some(ssh) = self.ssh.lock().unwrap().take() {
            ssh.disconnect();
        }
        if let Some(mut m) = self.master.lock().unwrap().take() {
            let _ = m.kill();
            let _ = m.wait();
        }
    }

    /// 退出程序时调用：只断开连接，不关 VM
    pub fn shutdown_local(&self) {
        self.gen_.fetch_add(1, Ordering::SeqCst);
        self.disconnect();
    }

    /// 推迟自动关机（重新计算闲置时间）
    pub fn extend(&self) {
        self.edit(|s| s.last_active = Some(Instant::now()));
    }

    pub fn refresh_usage(&self) {
        if let Ok(u) = remote::usage() {
            self.edit(|s| {
                s.usage = u;
                s.usage_at = Some(Instant::now());
            });
        }
    }

    // ------------------------------------------------------------ 监控

    fn monitor(&self, g: u64) {
        // 出图同步单独一个线程，不拖慢状态刷新
        {
            let e = self.clone();
            std::thread::spawn(move || {
                while e.alive(g) {
                    if let Some(ssh) = e.ssh() {
                        if let Err(err) = e.sync_outputs(&ssh, None) {
                            e.log(format!("同步出图失败：{err}"));
                        }
                    }
                    std::thread::sleep(Duration::from_secs(4));
                }
            });
        }
        let mut last_usage = Instant::now();
        let mut last_beat = Instant::now();
        let mut fails = 0;
        let mut warned = false;
        while self.alive(g) {
            std::thread::sleep(Duration::from_secs(3));
            if !self.alive(g) {
                break;
            }
            let Some(ssh) = self.ssh() else { break };
            // 主连接断了就重连
            let master_dead = self.master.lock().unwrap().as_mut().map(|m| m.try_wait().ok().flatten().is_some()).unwrap_or(true);
            if master_dead || !ssh.alive() {
                fails += 1;
                self.log("连接断开，正在重连…");
                self.edit(|s| s.comfy_up = false);
                let port = self.state().local_port;
                match ssh.connect(port) {
                    Ok(c) => {
                        *self.master.lock().unwrap() = Some(c);
                        self.log("已重新连接");
                        fails = 0;
                    }
                    Err(err) => {
                        self.log(format!("重连失败：{err}"));
                        if fails >= 5 {
                            // 可能 VM 已经被 Colab 回收
                            let still = remote::sessions().map(|l| l.iter().any(|s| s.name == ssh.session)).unwrap_or(true);
                            if !still {
                                self.disconnect();
                                self.edit(|s| {
                                    s.phase = Some(Phase::Off);
                                    s.comfy_up = false;
                                    s.gpu = None;
                                    s.up_since = None;
                                });
                                self.notify("VM 已被 Colab 回收", true);
                                break;
                            }
                        }
                        continue;
                    }
                }
            }
            self.poll(&ssh);
            if last_usage.elapsed() > Duration::from_secs(60) {
                last_usage = Instant::now();
                self.refresh_usage();
            }
            if last_beat.elapsed() > Duration::from_secs(600) {
                last_beat = Instant::now();
                let name = ssh.session.clone();
                let me = self.clone();
                std::thread::spawn(move || {
                    if let Err(err) = remote::heartbeat(&name) {
                        me.log(format!("内核心跳失败：{err}"));
                    }
                });
            }
            // 自动关机
            if let Some(reason) = self.auto_stop_reason() {
                self.stop(Some(reason));
                break;
            }
            let left = self.idle_left();
            match left {
                Some(l) if l <= Duration::from_secs(120) && !warned => {
                    warned = true;
                    self.notify(format!("闲置太久，{} 后自动关机（点「延长」可以推迟）", remote::fmt_duration(l.as_secs())), true);
                }
                Some(l) if l > Duration::from_secs(120) => warned = false,
                _ => {}
            }
        }
    }

    fn poll(&self, ssh: &Ssh) {
        // 出图在单独的线程里同步，这里给一个不存在的目录，不列文件
        let want_log = self.want_log.load(Ordering::SeqCst);
        let cmd = format!("python3 - /nonexistent 0 {}", if want_log { 1 } else { 0 });
        let Ok(out) = ssh.run(&cmd, Some(POLL_PY.as_bytes()), Duration::from_secs(20)) else { return };
        let p = parse_poll(&out);
        self.edit(|s| {
            if let Some(g) = p.gpu.clone() {
                s.gpu = Some(g);
            }
            if p.mem.1 > 0 {
                s.mem = p.mem;
            }
            if p.disk.1 > 0 {
                s.disk = p.disk;
            }
            s.comfy_up = p.comfy_up;
            s.queue = p.queue;
            let busy = p.queue.0 + p.queue.1 > 0 || p.gpu.as_ref().map(|g| g.util >= 20.0).unwrap_or(false);
            if busy || s.last_active.is_none() {
                s.last_active = Some(Instant::now());
            }
            if want_log {
                s.comfy_log = p.log;
            }
        });
    }

    /// 离闲置关机还剩多久
    pub fn idle_left(&self) -> Option<Duration> {
        let cfg = self.config();
        let s = self.state();
        if cfg.idle_min == 0 || s.phase() != Phase::Running {
            return None;
        }
        let limit = Duration::from_secs(cfg.idle_min as u64 * 60);
        Some(limit.saturating_sub(s.last_active?.elapsed()))
    }

    /// 离最长开机时间还剩多久
    pub fn max_left(&self) -> Option<Duration> {
        let cfg = self.config();
        let s = self.state();
        if cfg.max_hours <= 0.0 || !s.phase().busy() {
            return None;
        }
        let limit = Duration::from_secs_f32(cfg.max_hours * 3600.0);
        Some(limit.saturating_sub(s.up_since?.elapsed()))
    }

    fn auto_stop_reason(&self) -> Option<String> {
        let cfg = self.config();
        if self.idle_left() == Some(Duration::ZERO) {
            return Some(format!("闲置超过 {} 分钟", cfg.idle_min));
        }
        if self.max_left() == Some(Duration::ZERO) {
            return Some(format!("开机超过 {} 小时", cfg.max_hours));
        }
        let bal = self.state().usage.balance;
        if let Some(b) = bal {
            if cfg.min_cu > 0.0 && b < cfg.min_cu {
                return Some(format!("CU 余额 {b:.1} 低于 {}", cfg.min_cu));
            }
        }
        None
    }

    // ------------------------------------------------------------ 出图同步

    fn session_dir(&self) -> PathBuf {
        PathBuf::from(self.config().output_local)
    }

    /// 本机出图目录里已有的文件（最新的在前）
    pub fn load_local_outputs(&self) {
        let root = self.session_dir();
        let mut list = Vec::new();
        walk(&root, &root, &mut list, 0);
        list.sort_by(|a, b| b.at.cmp(&a.at));
        list.truncate(2000);
        self.edit(|s| s.outputs = list);
    }

    /// 把 VM 上新生成的文件拉回本机：~/Pictures/Cumulus/<开机日期时间>/<相对路径>
    fn sync_outputs(&self, ssh: &Ssh, deadline: Option<Instant>) -> anyhow::Result<()> {
        let cfg = self.config();
        let since: f64 = ssh
            .run("cat /content/.cumulus/since 2>/dev/null", None, Duration::from_secs(20))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0.0);
        let cmd = format!("python3 - {} {} 0", sh_quote(&cfg.remote_output()), since);
        let out = ssh.run(&cmd, Some(POLL_PY.as_bytes()), Duration::from_secs(30))?;
        let folder = session_folder(since);
        let root = PathBuf::from(&cfg.output_local).join(&folder);
        let mut files: Vec<(String, u64, f64)> = out
            .lines()
            .filter_map(|l| l.strip_prefix("out "))
            .filter_map(|l| {
                let mut it = l.split('\t');
                Some((it.next()?.to_string(), it.next()?.parse().ok()?, it.next()?.parse().ok()?))
            })
            .collect();
        files.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        let have: HashSet<PathBuf> = self.state().outputs.iter().map(|o| o.local.clone()).collect();
        for (rel, size, mtime) in files {
            if deadline.is_some_and(|d| Instant::now() > d) {
                break;
            }
            // 刚写出来的文件可能还没写完，等 2 秒后再拿
            if now_secs() - mtime < 2.0 && deadline.is_none() {
                continue;
            }
            let local = root.join(&rel);
            if have.contains(&local) || std::fs::metadata(&local).map(|m| m.len() == size).unwrap_or(false) {
                continue;
            }
            self.edit(|s| s.syncing = Some(rel.clone()));
            let remote_path = format!("{}/{}", cfg.remote_output().trim_end_matches('/'), rel);
            let res = ssh.download(&remote_path, &local);
            self.edit(|s| s.syncing = None);
            match res {
                Ok(n) => {
                    self.edit(|s| {
                        s.outputs.retain(|o| o.local != local);
                        s.outputs.insert(0, Output { rel: format!("{folder}/{rel}"), size: n, local: local.clone(), at: SystemTime::now() });
                    });
                }
                Err(err) => self.log(format!("下载 {rel} 失败：{err}")),
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ 模型

    pub fn load_models(&self) {
        let Some(ssh) = self.ssh() else {
            self.edit(|s| s.models_error = Some("开机后才能查看 Drive 上的模型".into()));
            return;
        };
        let root = self.config().models_root();
        self.edit(|s| {
            s.models_loading = true;
            s.models_error = None;
        });
        let e = self.clone();
        std::thread::spawn(move || {
            let cmd = format!(
                "mkdir -p {r}; cd {r} && for d in */; do echo \"dir ${{d%/}}\"; done; find -L . -mindepth 2 -maxdepth 3 -type f ! -name '.*' -printf 'f %P\\t%s\\n' 2>/dev/null; {ssd}",
                r = sh_quote(&root),
                ssd = SSD_LIST
            );
            let res = ssh.run(&cmd, None, Duration::from_secs(120));
            e.edit(|s| {
                s.models_loading = false;
                match res {
                    Ok(out) => {
                        let on_ssd = parse_ssd_list(&out);
                        let mut list = Vec::new();
                        for l in out.lines() {
                            if let Some(d) = l.strip_prefix("dir ") {
                                list.push(Model { category: d.to_string(), name: String::new(), size: 0, ssd: false });
                            } else if let Some(f) = l.strip_prefix("f ") {
                                let Some((path, size)) = f.split_once('\t') else { continue };
                                let Some((cat, name)) = path.split_once('/') else { continue };
                                list.push(Model { category: cat.into(), name: name.into(), size: size.parse().unwrap_or(0), ssd: on_ssd.contains(path) });
                            }
                        }
                        list.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
                        s.models = list;
                    }
                    Err(err) => s.models_error = Some(err.to_string()),
                }
            });
        });
    }

    /// 在 VM 上把模型下载到 models/<类别>/（接了 Drive 就是 Drive 上，永久保存）
    pub fn download_model(&self, url: String, category: String, name: String) {
        let Some(ssh) = self.ssh() else {
            self.notify("开机后才能下载模型", true);
            return;
        };
        let cfg = self.config();
        let id = self.seq.fetch_add(1, Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));
        let shown = if name.is_empty() { url.rsplit('/').next().unwrap_or(&url).split('?').next().unwrap_or("").to_string() } else { name.clone() };
        self.edit(|s| {
            s.downloads.insert(
                0,
                Download { id, url: url.clone(), category: category.clone(), name: shown, done: 0, total: 0, speed: 0.0, status: DlStatus::Queued, to_ssd: false, cancel: cancel.clone() },
            )
        });
        let e = self.clone();
        std::thread::spawn(move || {
            let dest = format!("{}/{}", cfg.models_root(), category);
            let mut script = String::new();
            script.push_str(&format!("export HF_TOKEN={} CIVITAI_TOKEN={}\n", sh_quote(&cfg.hf_token), sh_quote(&cfg.civitai_token)));
            script.push_str(&format!("cat > /content/.cumulus/dl.py <<'CUMULUS_EOF'\n{DL_PY}\nCUMULUS_EOF\n"));
            script.push_str(&format!(
                "exec python3 /content/.cumulus/dl.py {} {} {}\n",
                sh_quote(&url),
                sh_quote(&dest),
                sh_quote(&name)
            ));
            if e.run_task(id, &ssh, script, &cancel) {
                let name = e.state().downloads.iter().find(|d| d.id == id).map(|d| d.name.clone()).unwrap_or_default();
                e.notify(format!("{name} 下载完成（{category}）"), false);
                e.load_models();
            }
        });
    }

    /// 在 VM 上跑一个带 @@NAME / @@DL / @@DONE / @@FAIL 输出的任务，把进度写进对应的 Download。
    /// 成功返回 true
    fn run_task(&self, id: u64, ssh: &Ssh, script: String, cancel: &AtomicBool) -> bool {
        let upd = |f: &dyn Fn(&mut Download)| {
            self.edit(|s| {
                if let Some(d) = s.downloads.iter_mut().find(|d| d.id == id) {
                    f(d);
                }
            })
        };
        upd(&|d| d.status = DlStatus::Running);
        let mut last = (Instant::now(), 0u64);
        let mut started = false;
        let mut result: Option<Result<String, String>> = None;
        let res = ssh.stream("mkdir -p /content/.cumulus && bash -s", Some(script.into_bytes()), |line| {
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            if let Some(n) = line.strip_prefix("@@NAME ") {
                let n = n.to_string();
                upd(&|d| d.name = n.clone());
            } else if let Some(p) = line.strip_prefix("@@DL ") {
                let mut it = p.split_whitespace();
                let done: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let total: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                // 第一条进度只当起点（续传时 done 一开始就很大，不能算进速度）
                if !started {
                    started = true;
                    last = (Instant::now(), done);
                    upd(&|d| {
                        d.done = done;
                        d.total = total;
                    });
                    return;
                }
                let dt = last.0.elapsed().as_secs_f64();
                let speed = if dt > 0.2 { (done.saturating_sub(last.1)) as f64 / dt } else { -1.0 };
                if dt > 0.2 {
                    last = (Instant::now(), done);
                }
                upd(&|d| {
                    d.done = done;
                    d.total = total;
                    if speed >= 0.0 {
                        d.speed = if d.speed > 0.0 { d.speed * 0.6 + speed * 0.4 } else { speed };
                    }
                });
            } else if let Some(p) = line.strip_prefix("@@DONE ") {
                result = Some(Ok(p.to_string()));
            } else if let Some(m) = line.strip_prefix("@@FAIL ") {
                result = Some(Err(m.to_string()));
            }
        });
        if cancel.load(Ordering::SeqCst) {
            upd(&|d| d.status = DlStatus::Cancelled);
            return false;
        }
        match result {
            Some(Ok(_)) => {
                upd(&|d| {
                    d.status = DlStatus::Done;
                    d.done = d.done.max(d.total);
                });
                true
            }
            Some(Err(m)) => {
                upd(&|d| d.status = DlStatus::Failed(m.clone()));
                false
            }
            None => {
                let m = res.err().map(|e| e.to_string()).unwrap_or_else(|| "连接中断".into());
                upd(&|d| d.status = DlStatus::Failed(m.clone()));
                false
            }
        }
    }

    pub fn cancel_download(&self, id: u64) {
        let Some(d) = self.state().downloads.iter().find(|d| d.id == id).cloned() else { return };
        d.cancel.store(true, Ordering::SeqCst);
        self.edit(|s| {
            if let Some(x) = s.downloads.iter_mut().find(|x| x.id == id) {
                x.status = DlStatus::Cancelled;
            }
        });
        if d.status == DlStatus::Queued {
            return;
        }
        if let Some(ssh) = self.ssh() {
            let url = d.url.clone();
            let pat = if d.to_ssd { format!("cp.py {url}") } else { format!("dl.py {}", url.split('?').next().unwrap_or(&url)) };
            std::thread::spawn(move || {
                // 按地址找到进程结束掉（留下的 .part 下次可以接着下 / 接着复制）
                let _ = ssh.run(&format!("pkill -f -- {}", sh_quote(&regex_escape(&pat))), None, Duration::from_secs(20));
            });
        }
    }

    pub fn clear_finished_downloads(&self) {
        self.edit(|s| s.downloads.retain(|d| d.active()));
    }

    pub fn delete_model(&self, category: String, name: String) {
        let Some(ssh) = self.ssh() else { return };
        let path = format!("{}/{}/{}", self.config().models_root(), category, name);
        let ssd = format!("{SSD_ROOT}/models/{category}/{name}");
        self.ssd_select(&category, &name, false);
        let e = self.clone();
        std::thread::spawn(move || {
            match ssh.run(&format!("rm -f -- {} {}", sh_quote(&path), sh_quote(&ssd)), None, Duration::from_secs(60)) {
                Ok(_) => e.notify(format!("已删除 {name}（在 Drive 回收站里还能找回）"), false),
                Err(err) => e.notify(format!("删除失败：{err}"), true),
            }
            e.load_models();
        });
    }
}

impl Engine {
    // ------------------------------------------------------------ 本地 SSD

    /// 勾选 / 取消一个模型放 SSD。勾选：记进设置并开始复制；取消：停掉复制、删掉 SSD 上的副本
    pub fn ssd_select(&self, category: &str, name: &str, on: bool) {
        let key = format!("{category}/{name}");
        let mut cfg = self.config();
        let had = cfg.ssd_models.contains(&key);
        if on && !had {
            cfg.ssd_models.push(key.clone());
        } else if !on && had {
            cfg.ssd_models.retain(|k| *k != key);
        }
        if had != on {
            self.set_config(cfg);
        }
        if on {
            self.copy_to_ssd(category.to_string(), name.to_string());
            return;
        }
        // 取消：停掉排队 / 进行中的复制，删除副本
        let active: Vec<u64> = self
            .state()
            .downloads
            .iter()
            .filter(|d| d.to_ssd && d.active() && d.category == category && d.name == name)
            .map(|d| d.id)
            .collect();
        for id in active {
            self.cancel_download(id);
        }
        self.edit(|s| {
            if let Some(m) = s.models.iter_mut().find(|m| m.category == category && m.name == name) {
                m.ssd = false;
            }
        });
        if let Some(ssh) = self.ssh() {
            let dir = format!("{SSD_ROOT}/models/{category}");
            let base = name.rsplit('/').next().unwrap_or(name);
            let sub = name.rsplit_once('/').map(|x| format!("/{}", x.0)).unwrap_or_default();
            let files = [format!("{dir}/{name}"), format!("{dir}{sub}/.{base}.part")];
            std::thread::spawn(move || {
                // 等 pkill 先生效，避免删完又被写出来
                std::thread::sleep(Duration::from_secs(2));
                let _ = ssh.run(&format!("rm -f -- {} {}", sh_quote(&files[0]), sh_quote(&files[1])), None, Duration::from_secs(30));
            });
        }
    }

    /// 选中但还不在 SSD 上的模型全部排队复制（开机后自动调用）
    pub fn ssd_sync(&self) {
        let Some(ssh) = self.ssh() else { return };
        let e = self.clone();
        std::thread::spawn(move || {
            let have = match ssh.run(SSD_LIST, None, Duration::from_secs(60)) {
                Ok(out) => parse_ssd_list(&out),
                Err(err) => {
                    e.notify(format!("读取 SSD 上的模型失败：{err}"), true);
                    return;
                }
            };
            for key in e.config().ssd_models {
                if have.contains(&key) {
                    continue;
                }
                if let Some((cat, name)) = key.split_once('/') {
                    e.copy_to_ssd(cat.to_string(), name.to_string());
                }
            }
        });
    }

    fn copy_to_ssd(&self, category: String, name: String) {
        let Some(ssh) = self.ssh() else { return };
        let busy = self.state().downloads.iter().any(|d| d.to_ssd && d.active() && d.category == category && d.name == name);
        if busy {
            return;
        }
        let src = format!("{}/{}/{}", self.config().models_root(), category, name);
        let dst = format!("{SSD_ROOT}/models/{category}/{name}");
        let id = self.seq.fetch_add(1, Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));
        let size = self.state().models.iter().find(|m| m.category == category && m.name == name).map(|m| m.size).unwrap_or(0);
        self.edit(|s| {
            s.downloads.insert(
                0,
                Download {
                    id,
                    url: src.clone(),
                    category: category.clone(),
                    name: name.clone(),
                    done: 0,
                    total: size,
                    speed: 0.0,
                    status: DlStatus::Queued,
                    to_ssd: true,
                    cancel: cancel.clone(),
                },
            )
        });
        let e = self.clone();
        std::thread::spawn(move || {
            // 等一个并发名额
            {
                let (n, cv) = &*e.ssd_slots;
                let mut free = n.lock().unwrap();
                while *free == 0 {
                    free = cv.wait(free).unwrap();
                }
                *free -= 1;
            }
            let _slot = SlotGuard(e.ssd_slots.clone());
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            let script = format!(
                "cat > /content/.cumulus/cp.py <<'CUMULUS_EOF'\n{CP_PY}\nCUMULUS_EOF\nexec python3 /content/.cumulus/cp.py {} {}\n",
                sh_quote(&src),
                sh_quote(&dst)
            );
            if e.run_task(id, &ssh, script, &cancel) {
                e.edit(|s| {
                    if let Some(m) = s.models.iter_mut().find(|m| m.category == category && m.name == name) {
                        m.ssd = true;
                    }
                });
                e.notify(format!("{name} 已复制到本地 SSD"), false);
            }
        });
    }
}

/// 用完（包括取消、失败）归还 SSD 复制名额
struct SlotGuard(Arc<(Mutex<usize>, Condvar)>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let (n, cv) = &*self.0;
        *n.lock().unwrap() += 1;
        cv.notify_one();
    }
}

/// pkill -f 的参数是正则；模型名里的 ( ) + [ 等要转义
fn regex_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// 列出 SSD 上已经复制好的模型：每行 "s 类别/文件名"
const SSD_LIST: &str = "cd /content/cumulus_ssd/models 2>/dev/null && find . -mindepth 2 -maxdepth 3 -type f ! -name '.*' -printf 's %P\\n' 2>/dev/null; true";

fn parse_ssd_list(out: &str) -> HashSet<String> {
    out.lines().filter_map(|l| l.strip_prefix("s ")).map(|x| x.trim_end().to_string()).collect()
}

/// 开机时刻 → 本机出图子文件夹名，如 2026-11-23_1605
fn session_folder(since: f64) -> String {
    let t = if since > 0.0 { since as i64 } else { now_secs() as i64 };
    let out = std::process::Command::new("/bin/date")
        .args(["-r", &t.to_string(), "+%Y-%m-%d_%H%M"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if out.is_empty() { format!("session-{t}") } else { out }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Output>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let p = ent.path();
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let Ok(md) = ent.metadata() else { continue };
        if md.is_dir() {
            walk(root, &p, out, depth + 1);
        } else if is_media(&name) {
            out.push(Output {
                rel: p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned(),
                size: md.len(),
                local: p.clone(),
                at: md.modified().unwrap_or(UNIX_EPOCH),
            });
        }
    }
}

pub fn is_media(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif", ".mp4", ".webm", ".mov", ".mkv", ".wav", ".mp3", ".flac", ".glb", ".obj"].iter().any(|e| n.ends_with(e))
}

pub fn is_image(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp"].iter().any(|e| n.ends_with(e))
}

#[derive(Default, Debug)]
pub struct Poll {
    pub gpu: Option<Gpu>,
    pub mem: (u64, u64),
    pub disk: (u64, u64),
    pub comfy_up: bool,
    pub queue: (u32, u32),
    pub log: Vec<String>,
}

pub fn parse_poll(out: &str) -> Poll {
    let mut p = Poll::default();
    let two = |s: &str| -> (u64, u64) {
        let mut it = s.split('\t');
        (it.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0), it.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
    };
    for l in out.lines() {
        if let Some(g) = l.strip_prefix("gpu ") {
            let f: Vec<&str> = g.split('\t').collect();
            let n = |i: usize| f.get(i).and_then(|v| v.trim().parse::<f32>().ok()).unwrap_or(0.0);
            p.gpu = Some(Gpu { util: n(0), mem_used: n(1), mem_total: n(2), temp: n(3), power: n(4), name: f.get(5).map(|s| s.trim().to_string()).unwrap_or_default() });
        } else if let Some(m) = l.strip_prefix("mem ") {
            p.mem = two(m);
        } else if let Some(d) = l.strip_prefix("disk ") {
            p.disk = two(d);
        } else if let Some(c) = l.strip_prefix("comfy ") {
            p.comfy_up = c.trim() == "up";
        } else if let Some(q) = l.strip_prefix("queue ") {
            let (a, b) = two(q);
            p.queue = (a as u32, b as u32);
        } else if let Some(x) = l.strip_prefix("log ") {
            p.log.push(x.to_string());
        } else if l == "log" {
            p.log.push(String::new());
        }
    }
    p
}

pub fn fmt_speed(bps: f64) -> String {
    format!("{}/s", fmt_bytes(bps.max(0.0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_parse() {
        let p = parse_poll("gpu 37\t1200\t23034\t51\t72.5\tNVIDIA L4\nmem 100\t200\ndisk 5\t10\ncomfy up\nqueue 1\t2\nlog hello\n");
        assert_eq!(p.gpu.as_ref().unwrap().util, 37.0);
        assert_eq!(p.gpu.as_ref().unwrap().name, "NVIDIA L4");
        assert_eq!(p.mem, (100, 200));
        assert!(p.comfy_up);
        assert_eq!(p.queue, (1, 2));
        assert_eq!(p.log, vec!["hello"]);
        assert!(!parse_poll("comfy down\n").comfy_up);
    }

    #[test]
    fn escape() {
        assert_eq!(regex_escape("cp.py /a/b (1)+x.safetensors"), "cp\\.py /a/b \\(1\\)\\+x\\.safetensors");
        assert!(parse_ssd_list("s vae/ae.safetensors\nother\ns loras/x y.safetensors\n").contains("loras/x y.safetensors"));
    }

    #[test]
    fn media() {
        assert!(is_media("a/ComfyUI_00001_.PNG"));
        assert!(!is_media("x.json"));
        assert!(is_image("x.webp") && !is_image("x.mp4"));
    }
}
