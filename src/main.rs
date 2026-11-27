mod accounts;
#[allow(dead_code)]
mod anim;
mod cli;
mod config;
mod control;
mod engine;
mod input;
mod remote;
mod workflow;
#[allow(dead_code)]
mod theme;

use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt, AnyElement, App, Application, Bounds, ClipboardItem, Context, Entity, FontWeight, Hsla, KeyBinding, Menu,
    MenuItem, ObjectFit, ScrollStrategy, SharedString, Stateful, Task, TitlebarOptions, UniformListScrollHandle, Window, WindowBounds,
    WindowControlArea, WindowOptions, actions, div, img, point, prelude::*, px, size, uniform_list,
};

use config::{Config, GPUS};
use engine::{DlStatus, Engine, Phase, STEPS};
use input::TextInput;
use remote::{fmt_bytes, fmt_duration};

actions!(cumulus, [Quit, ToggleTheme, Refresh]);

const LEFT_W: f32 = 212.0;
const TOPBAR_H: f32 = 40.0;
const DEFAULT_CATS: &[&str] =
    &["checkpoints", "diffusion_models", "loras", "vae", "text_encoders", "controlnet", "upscale_models", "clip_vision", "embeddings"];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Page {
    Home,
    Models,
    Outputs,
    Logs,
    Settings,
}

impl Page {
    const ALL: [Page; 5] = [Page::Home, Page::Models, Page::Outputs, Page::Logs, Page::Settings];
    fn label(self) -> &'static str {
        match self {
            Page::Home => "控制台",
            Page::Models => "模型",
            Page::Outputs => "出图",
            Page::Logs => "日志",
            Page::Settings => "设置",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Page::Home => "◉",
            Page::Models => "▤",
            Page::Outputs => "▦",
            Page::Logs => "≡",
            Page::Settings => "⚙",
        }
    }
    fn from_key(s: &str) -> Page {
        match s {
            "models" => Page::Models,
            "outputs" => Page::Outputs,
            "logs" => Page::Logs,
            "settings" => Page::Settings,
            _ => Page::Home,
        }
    }
}

struct Inputs {
    drive_root: Entity<TextInput>,
    idle_min: Entity<TextInput>,
    max_hours: Entity<TextInput>,
    min_cu: Entity<TextInput>,
    local_port: Entity<TextInput>,
    output_local: Entity<TextInput>,
    extra_args: Entity<TextInput>,
    custom_nodes: Entity<TextInput>,
    hf_token: Entity<TextInput>,
    civitai_token: Entity<TextInput>,
    dl_url: Entity<TextInput>,
    dl_name: Entity<TextInput>,
    profile_name: Entity<TextInput>,
    account_name: Entity<TextInput>,
    login_code: Entity<TextInput>,
}

struct Notice {
    text: String,
    err: bool,
    at: Instant,
    id: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum QuitDialog {
    Ask,
    Stopping,
}

struct Cumulus {
    engine: Engine,
    page: Page,
    seen: u64,
    inputs: Inputs,
    /// 设置页的开关（保存前的草稿）
    draft_mount: bool,
    draft_out_drive: bool,
    dl_category: String,
    model_cat: Option<String>,
    confirm_delete: Option<(String, String)>,
    /// 「从工作流导入 SSD」面板是否展开
    wf_open: bool,
    /// 切换方案会删掉 SSD 上的模型时，先问一下
    confirm_profile: Option<String>,
    /// 删除账号前先问一下（账号 id）
    confirm_account_rm: Option<String>,
    /// 已经替用户在浏览器里打开过登录页的那次登录（账号 id）
    login_opened: Option<String>,
    comfy_log_tab: bool,
    log_scroll: UniformListScrollHandle,
    log_follow: bool,
    out_scroll: UniformListScrollHandle,
    center_w: f32,
    notice: Option<Notice>,
    notice_seq: u64,
    quit: Option<QuitDialog>,
    _ticker: Task<()>,
}

fn label(text: impl Into<SharedString>) -> gpui::Div {
    div().text_xs().text_color(theme::faint()).child(text.into())
}

fn button(id: impl Into<gpui::ElementId>, text: impl Into<SharedString>, enabled: bool) -> Stateful<gpui::Div> {
    let d = div().id(id).px_2p5().py_1().rounded_md().border_1().border_color(theme::border()).text_xs().child(text.into());
    if enabled { d.text_color(theme::text()).cursor_pointer().hover(|s| s.bg(theme::hover())) } else { d.text_color(theme::faint()) }
}

/// 强调色的主按钮
fn primary(id: impl Into<gpui::ElementId>, text: impl Into<SharedString>, color: Hsla) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .px_4()
        .py_1p5()
        .rounded_md()
        .bg(color)
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::on_accent())
        .cursor_pointer()
        .hover(|s| s.opacity(0.88))
        .child(text.into())
}

fn chip(id: impl Into<gpui::ElementId>, text: impl Into<SharedString>, on: bool, enabled: bool) -> Stateful<gpui::Div> {
    let d = div().id(id).px_2p5().py_1().rounded_full().border_1().text_xs().child(text.into());
    let d = if on {
        d.bg(theme::accent().opacity(0.16)).border_color(theme::accent()).text_color(theme::accent())
    } else {
        d.border_color(theme::border()).text_color(if enabled { theme::muted() } else { theme::faint() })
    };
    if enabled { d.cursor_pointer().hover(|s| s.bg(theme::hover())) } else { d.opacity(0.6) }
}

fn bar(key: &str, pct_v: f32, color: Hsla) -> gpui::Div {
    let pct_v = anim::smooth(key, pct_v.clamp(0.0, 100.0));
    div()
        .h(px(5.))
        .w_full()
        .rounded_full()
        .bg(theme::track())
        .child(div().h_full().rounded_full().bg(color).w(gpui::relative((pct_v / 100.0).clamp(0.0, 1.0))))
}

fn card() -> gpui::Div {
    div().p_4().rounded_lg().bg(theme::card()).border_1().border_color(theme::border()).flex().flex_col()
}

fn mono() -> gpui::Div {
    div().font_family("Menlo").text_xs()
}

fn pct(a: f32, b: f32) -> f32 {
    if b <= 0.0 { 0.0 } else { a / b * 100.0 }
}

fn mmss(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

fn phase_color(p: &Phase) -> Hsla {
    match p {
        Phase::Running => theme::ok(),
        Phase::Failed(_) => theme::bad(),
        Phase::Off | Phase::Checking => theme::faint(),
        _ => theme::warn(),
    }
}

/// 「12.3 GB / 可放约 180 GB」
fn budget_label(b: &engine::Budget) -> String {
    let mut t = fmt_bytes(b.bytes);
    if b.unknown > 0 {
        t.push_str(&format!("（{} 个大小未知）", b.unknown));
    }
    match b.room {
        Some(r) => {
            t.push_str(&format!(" / SSD 可放约 {}", fmt_bytes(r.saturating_sub(engine::SSD_RESERVE))));
            if !b.fits() {
                t.push_str("，放不下");
            }
        }
        None => t.push_str("（这种 GPU 的 SSD 还没量过，开一次机就知道）"),
    }
    t
}

fn open_path(p: &std::path::Path) {
    let _ = std::process::Command::new("/usr/bin/open").arg(p).spawn();
}

impl Cumulus {
    fn new(engine: Engine, cx: &mut Context<Self>) -> Self {
        let cfg = engine.config();
        let mk = |ph: &str, val: &str, cx: &mut Context<Self>| {
            let e = cx.new(|cx| TextInput::new(ph.to_string(), cx));
            let v = val.to_string();
            e.update(cx, |t, cx| t.set_text(v, None, cx));
            e
        };
        let inputs = Inputs {
            drive_root: mk("/content/drive/MyDrive/…", &cfg.drive_root, cx),
            idle_min: mk("30", &cfg.idle_min.to_string(), cx),
            max_hours: mk("6", &cfg.max_hours.to_string(), cx),
            min_cu: mk("5", &cfg.min_cu.to_string(), cx),
            local_port: mk("8188", &cfg.local_port.to_string(), cx),
            output_local: mk("~/Pictures/Cumulus", &cfg.output_local, cx),
            extra_args: mk("例如 --highvram --fast", &cfg.extra_args, cx),
            custom_nodes: mk("git 地址，空格分隔", &cfg.custom_nodes, cx),
            hf_token: mk("hf_…（可选）", &cfg.hf_token, cx),
            civitai_token: mk("（可选）", &cfg.civitai_token, cx),
            dl_url: mk("粘贴 Hugging Face / Civitai / 直链地址", "", cx),
            dl_name: mk("文件名（可选，默认自动识别）", "", cx),
            profile_name: mk("方案名，如「视频 Wan」", "", cx),
            account_name: mk("新账号的名字，如「副号」", "", cx),
            login_code: mk("粘贴授权码", "", cx),
        };
        // 定时看一下引擎状态有没有变化；开机中、运行中每秒刷新一次（倒计时、计时器）
        let ticker = cx.spawn(async move |this, cx| {
            let mut n = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                n = n.wrapping_add(1);
                if this.update(cx, |app, cx| app.tick(n % 4 == 0, cx)).is_err() {
                    break;
                }
            }
        });
        Cumulus {
            draft_mount: cfg.mount_drive,
            draft_out_drive: cfg.output_to_drive,
            engine,
            page: Page::Home,
            seen: 0,
            inputs,
            dl_category: "checkpoints".into(),
            model_cat: None,
            confirm_delete: None,
            wf_open: false,
            confirm_profile: None,
            confirm_account_rm: None,
            login_opened: None,
            comfy_log_tab: false,
            log_scroll: UniformListScrollHandle::new(),
            log_follow: true,
            out_scroll: UniformListScrollHandle::new(),
            center_w: 900.0,
            notice: None,
            notice_seq: 0,
            quit: None,
            _ticker: ticker,
        }
    }

    fn tick(&mut self, second: bool, cx: &mut Context<Self>) {
        let (ver, busy, notices) = {
            let mut s = self.engine.state();
            (s.version, s.phase().busy(), std::mem::take(&mut s.notices))
        };
        for (t, e) in notices {
            self.toast(t, e);
        }
        // 登录地址出来了：替用户在浏览器里打开一次
        let login_url = self.engine.state().login.as_ref().and_then(|l| Some((l.id.clone(), l.url.clone()?)));
        if let Some((id, url)) = login_url {
            if self.login_opened.as_deref() != Some(id.as_str()) {
                self.login_opened = Some(id);
                cx.open_url(&url);
            }
        }
        if ver != self.seen || (second && busy) || self.notice.is_some() {
            if ver != self.seen && self.log_follow && self.page == Page::Logs {
                let n = self.log_len();
                if n > 0 {
                    self.log_scroll.scroll_to_item(n - 1, ScrollStrategy::Bottom);
                }
            }
            self.seen = ver;
            cx.notify();
        }
        // 正在关机准备退出：关完就退
        if self.quit == Some(QuitDialog::Stopping) {
            let p = self.engine.state().phase();
            if matches!(p, Phase::Off | Phase::Failed(_)) {
                self.engine.shutdown_local();
                cx.quit();
            }
        }
    }

    fn toast(&mut self, text: impl Into<String>, err: bool) {
        self.notice_seq += 1;
        self.notice = Some(Notice { text: text.into(), err, at: Instant::now(), id: self.notice_seq });
    }

    fn set_page(&mut self, p: Page, cx: &mut Context<Self>) {
        self.page = p;
        self.engine.set_want_log(p == Page::Logs && self.comfy_log_tab);
        let need_models = {
            let s = self.engine.state();
            s.models.is_empty() && s.phase() == Phase::Running
        };
        if p == Page::Models && need_models {
            self.engine.load_models();
        }
        if p == Page::Outputs {
            self.engine.load_local_outputs();
        }
        if p == Page::Logs {
            self.log_follow = true;
        }
        cx.notify();
    }

    fn log_len(&self) -> usize {
        let s = self.engine.state();
        if self.comfy_log_tab { s.comfy_log.len() } else { s.log.len() }
    }

    fn request_quit(&mut self, cx: &mut Context<Self>) {
        let busy = self.engine.state().phase().busy();
        if busy {
            self.quit = Some(QuitDialog::Ask);
            cx.notify();
        } else {
            self.engine.shutdown_local();
            cx.quit();
        }
    }

    /// 退出程序但让 VM 继续跑：起一个后台守护进程，继续负责自动关机和出图同步
    fn quit_keep_running(&mut self, cx: &mut Context<Self>) {
        self.engine.shutdown_local();
        // 先让出控制接口，守护进程才能接手（命令行之后找它）
        let _ = std::fs::remove_file(control::sock_path());
        let _ = control::spawn_guard();
        cx.quit();
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        let t = |e: &Entity<TextInput>, cx: &Context<Self>| e.read(cx).text().trim().to_string();
        let mut c = self.engine.config();
        let i = &self.inputs;
        let mut errs = Vec::new();
        c.drive_root = t(&i.drive_root, cx).trim_end_matches('/').to_string();
        match t(&i.idle_min, cx).parse() {
            Ok(v) => c.idle_min = v,
            Err(_) => errs.push("闲置分钟"),
        }
        match t(&i.max_hours, cx).parse::<f32>() {
            Ok(v) if v >= 0.0 => c.max_hours = v,
            _ => errs.push("最长小时"),
        }
        match t(&i.min_cu, cx).parse::<f32>() {
            Ok(v) if v >= 0.0 => c.min_cu = v,
            _ => errs.push("最低 CU"),
        }
        match t(&i.local_port, cx).parse::<u16>() {
            Ok(v) if v >= 1024 => c.local_port = v,
            _ => errs.push("本机端口"),
        }
        let out = t(&i.output_local, cx);
        let out = if let Some(rest) = out.strip_prefix("~/") {
            dirs::home_dir().map(|h| h.join(rest).to_string_lossy().into_owned()).unwrap_or(out.clone())
        } else {
            out
        };
        if !out.is_empty() {
            c.output_local = out;
        }
        c.extra_args = t(&i.extra_args, cx);
        c.custom_nodes = t(&i.custom_nodes, cx);
        c.hf_token = t(&i.hf_token, cx);
        c.civitai_token = t(&i.civitai_token, cx);
        c.mount_drive = self.draft_mount;
        c.output_to_drive = self.draft_out_drive;
        if !errs.is_empty() {
            self.toast(format!("这些值不对：{}", errs.join("、")), true);
        } else {
            let busy = self.engine.state().phase().busy();
            self.engine.set_config(c);
            self.toast(if busy { "已保存。Drive、节点、启动参数下次开机生效；自动关机规则立即生效" } else { "已保存" }, false);
        }
        cx.notify();
    }

    /// 选本机的工作流文件分析
    fn pick_workflow_file(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("分析".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(p) = paths.into_iter().next() else { return };
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let text = std::fs::read_to_string(&p);
            let _ = this.update(cx, |app, cx| {
                match text {
                    Ok(t) => app.engine.preview_workflow(name, Some(t)),
                    Err(e) => app.toast(format!("读不了 {name}：{e}"), true),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 「从工作流导入 SSD」面板：选工作流 → 预览要复制 / 缺少的模型 → 一键复制
    fn render_wf_import(&self, s: &engine::State, cx: &mut Context<Self>) -> gpui::Div {
        let mut panel = div().mt_1().p_3().rounded_md().border_1().border_color(theme::border()).flex().flex_col().gap_2().child(
            div().flex().items_center().gap_2()
                .child(div().flex_1().text_xs().text_color(theme::muted()).child("选一个工作流，把它用到的模型都复制到 SSD"))
                .child(button("wf-file", "本机文件…", true).on_click(cx.listener(|this, _, _, cx| this.pick_workflow_file(cx))))
                .child(button("wf-reload", "刷新", true).on_click(cx.listener(|this, _, _, cx| {
                    this.engine.load_workflows();
                    cx.notify();
                }))),
        );
        // ComfyUI 里保存的工作流（最新的在前）
        panel = match &s.workflows {
            None => panel.child(label("读取 ComfyUI 里保存的工作流…")),
            Some(Err(e)) => panel.child(label(format!("读取工作流失败：{e}"))),
            Some(Ok(list)) if list.is_empty() => panel.child(label("ComfyUI 里还没有保存的工作流，可以选本机文件。")),
            Some(Ok(list)) => {
                let current = s.wf_plan.as_ref().map(|p| p.0.clone());
                panel.child(div().flex().flex_wrap().gap_1p5().children(list.iter().take(40).enumerate().map(|(i, (name, _))| {
                    let n = name.clone();
                    let shown = name.trim_end_matches(".json").to_string();
                    chip(("wf", i), shown, current.as_deref() == Some(name.as_str()), true).on_click(cx.listener(move |this, _, _, cx| {
                        this.engine.preview_workflow(n.clone(), None);
                        cx.notify();
                    }))
                })))
            }
        };
        if s.wf_busy {
            return panel.child(label("分析中…"));
        }
        let Some((name, res)) = &s.wf_plan else { return panel };
        let plan = match res {
            Err(e) => return panel.child(div().text_xs().text_color(theme::bad()).child(format!("{name}：{e}"))),
            Ok(p) => p.clone(),
        };
        let copying: std::collections::HashSet<String> =
            s.downloads.iter().filter(|d| d.to_ssd && d.active()).map(|d| format!("{}/{}", d.category, d.name)).collect();
        let todo = plan.to_copy().filter(|f| !copying.contains(&f.0)).count();
        let todo_bytes: u64 = plan.to_copy().filter(|f| !copying.contains(&f.0)).map(|f| f.1).sum();
        panel = panel.child(
            div().pt_1().text_sm().font_weight(FontWeight::MEDIUM).child(format!(
                "{name}：用到 {} 个模型{}",
                plan.found.len() + plan.missing.len(),
                if plan.missing.is_empty() { String::new() } else { format!("，Drive 上缺 {} 个", plan.missing.len()) }
            )),
        );
        let row = || div().flex().items_center().gap_3().py_1().border_t_1().border_color(theme::border());
        for (i, (key, size, on_ssd, _)) in plan.found.iter().enumerate() {
            let (text, color) = if *on_ssd {
                ("已在 SSD", theme::ok())
            } else if copying.contains(key) {
                ("复制中", theme::accent())
            } else {
                ("将复制", theme::muted())
            };
            panel = panel.child(
                row()
                    .id(("wff", i))
                    .child(div().flex_1().min_w(px(0.)).text_xs().overflow_hidden().whitespace_nowrap().text_ellipsis().child(key.clone()))
                    .child(div().w(px(70.)).text_xs().text_color(theme::muted()).text_right().child(fmt_bytes(*size)))
                    .child(div().w(px(56.)).text_xs().text_color(color).text_right().child(text)),
            );
        }
        for (i, m) in plan.missing.iter().enumerate() {
            let dl = match (&m.url, &m.dir) {
                (Some(u), Some(d)) => {
                    let (u, d) = (u.clone(), d.clone());
                    Some(button(("wfdl", i), format!("下载到 {d}"), true).on_click(cx.listener(move |this, _, _, cx| {
                        this.engine.download_model(u.clone(), d.clone(), String::new());
                        this.toast("已开始下载，下完后再点一次这个工作流就能复制到 SSD", false);
                        cx.notify();
                    })))
                }
                _ => None,
            };
            panel = panel.child(
                row()
                    .child(div().flex_1().min_w(px(0.)).text_xs().text_color(theme::warn()).overflow_hidden().whitespace_nowrap().text_ellipsis().child(format!("缺少 {}", m.name)))
                    .child(label(m.node.clone()))
                    .children(dl),
            );
        }
        let free = s.disk.1.saturating_sub(s.disk.0);
        let keys: Vec<String> = plan.found.iter().map(|f| f.0.clone()).collect();
        let pname = name.rsplit('/').next().unwrap_or(name).trim_end_matches(".json").to_string();
        panel.child(
            div().flex().items_center().gap_2().pt_1()
                .child(div().flex_1().text_xs().text_color(theme::faint()).child(if s.disk.1 > 0 { format!("SSD 剩余 {}", fmt_bytes(free)) } else { String::new() }))
                .when(!keys.is_empty(), |d| {
                    d.child(button("wf-save", "存为方案", true).on_click(cx.listener(move |this, _, _, cx| {
                        match this.engine.profile_save(&pname, keys.clone()) {
                            Ok(()) => this.toast(format!("已存方案「{pname}」（{} 个模型），以后点一下就能整组换上", keys.len()), false),
                            Err(e) => this.toast(e.to_string(), true),
                        }
                        cx.notify();
                    })))
                })
                .child(if todo > 0 {
                    primary("wf-apply", format!("复制 {todo} 个到 SSD（{}）", fmt_bytes(todo_bytes)), theme::accent()).on_click(cx.listener(move |this, _, _, cx| {
                        match this.engine.apply_plan(&plan) {
                            Ok(ids) => this.toast(format!("开始复制 {} 个模型到 SSD", ids.len().max(todo)), false),
                            Err(e) => this.toast(e.to_string(), true),
                        }
                        cx.notify();
                    }))
                } else {
                    button("wf-apply", if plan.found.is_empty() { "Drive 上没有对应的模型" } else { "都已在 SSD 或正在复制" }, false)
                }),
        )
    }

    /// SSD 方案：点一下整组换上；把当前选择存成方案；删除当前方案
    fn render_profiles(&self, profiles: &[(String, Vec<String>, engine::Budget, bool)], s: &engine::State, running: bool, cx: &mut Context<Self>) -> gpui::Div {
        let mut d = div().flex().flex_col().gap_2().pt_1().child(
            div().flex().items_center().gap_1p5().flex_wrap()
                .child(label("方案").mr_1())
                .when(profiles.is_empty(), |d| d.child(label("还没有方案：在下面存当前选择，或在「从工作流导入」里存")))
                .children(profiles.iter().enumerate().map(|(i, (name, keys, b, active))| {
                    let n = name.clone();
                    let active = *active;
                    let text = format!("{}{name} · {} 个 · {}", if active { "✓ " } else { "" }, keys.len(), fmt_bytes(b.bytes));
                    let mut c = chip(("prof", i), text, active, true);
                    if !b.fits() {
                        c = c.text_color(theme::warn());
                    }
                    c.on_click(cx.listener(move |this, _, _, cx| {
                        if active {
                            return;
                        }
                        // 开着时切换会删掉 SSD 上不在方案里的模型：先确认
                        let removes = if this.engine.state().phase() == Phase::Running {
                            let keys = this.engine.config().ssd_profiles.get(&n).cloned().unwrap_or_default();
                            this.engine.state().models.iter().filter(|m| m.ssd && !keys.contains(&m.key())).count()
                        } else {
                            0
                        };
                        if removes > 0 && this.confirm_profile.as_deref() != Some(n.as_str()) {
                            this.confirm_profile = Some(n.clone());
                        } else {
                            this.confirm_profile = None;
                            this.engine.profile_use_bg(n.clone());
                        }
                        cx.notify();
                    }))
                })),
        );
        if let Some(n) = &self.confirm_profile {
            let keys = profiles.iter().find(|p| p.0 == *n).map(|p| p.1.clone()).unwrap_or_default();
            let removes = s.models.iter().filter(|m| m.ssd && !keys.contains(&m.key())).count();
            let n2 = n.clone();
            d = d.child(
                div().flex().items_center().gap_2()
                    .child(div().flex_1().text_xs().text_color(theme::warn()).child(format!("切换到「{n}」会从 SSD 删掉 {removes} 个不在方案里的模型（Drive 上的不动），腾出地方给方案里的")))
                    .child(button("prof-yes", "确认切换", true).on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm_profile = None;
                        this.engine.profile_use_bg(n2.clone());
                        cx.notify();
                    })))
                    .child(button("prof-no", "取消", true).on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_profile = None;
                        cx.notify();
                    }))),
            );
        }
        let active = profiles.iter().find(|p| p.3).map(|p| p.0.clone());
        let has_sel = !self.engine.config().ssd_models.is_empty();
        d.child(
            div().flex().items_center().gap_2()
                .child(div().w(px(220.)).child(self.inputs.profile_name.clone()))
                .child(button("prof-save", "把当前选择存为方案", has_sel).when(has_sel, |b| {
                    b.on_click(cx.listener(|this, _, _, cx| {
                        let name = this.inputs.profile_name.read(cx).text().trim().to_string();
                        let keys = this.engine.config().ssd_models;
                        match this.engine.profile_save(&name, keys) {
                            Ok(()) => {
                                this.toast(format!("已存方案「{name}」"), false);
                                this.inputs.profile_name.update(cx, |t, cx| t.set_text(String::new(), None, cx));
                            }
                            Err(e) => this.toast(e.to_string(), true),
                        }
                        cx.notify();
                    }))
                }))
                .when_some(active, |d, a| {
                    d.child(button("prof-del", format!("删除方案「{a}」"), true).on_click(cx.listener(move |this, _, _, cx| {
                        this.engine.profile_delete(&a);
                        this.toast(format!("已删除方案「{a}」（SSD 上的模型不动）"), false);
                        cx.notify();
                    })))
                })
                .when(!running && !profiles.is_empty(), |d| d.child(label("关机时切换只改选择，开机后复制"))),
        )
    }

    fn submit_download(&mut self, cx: &mut Context<Self>) {
        let url = self.inputs.dl_url.read(cx).text().trim().to_string();
        let name = self.inputs.dl_name.read(cx).text().trim().to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            self.toast("请粘贴 http(s) 下载地址", true);
            cx.notify();
            return;
        }
        if name.contains('/') {
            self.toast("文件名不能包含 /", true);
            cx.notify();
            return;
        }
        self.engine.download_model(url, self.dl_category.clone(), name);
        self.inputs.dl_url.update(cx, |t, cx| t.set_text("", None, cx));
        self.inputs.dl_name.update(cx, |t, cx| t.set_text("", None, cx));
        cx.notify();
    }
}

// ==================================================================== 渲染

impl Cumulus {
    fn render_left(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = remote::account();
        let (phase, hw, bal, acct) = {
            let s = self.engine.state();
            let acct = (s.accounts.len() > 1).then(|| s.accounts.iter().find(|a| a.id == active).map(|a| a.name.clone())).flatten();
            (s.phase(), s.hardware.clone(), s.usage.balance, acct)
        };
        div()
            .w(px(LEFT_W))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(theme::sidebar())
            .border_r_1()
            .border_color(theme::border())
            .child(div().h(px(TOPBAR_H)).flex_shrink_0().window_control_area(WindowControlArea::Drag))
            .child(
                div().px_4().pb_4().flex().flex_col().gap_0p5()
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Cumulus"))
                    .child(label("Colab 上的 ComfyUI")),
            )
            .child(div().px_2().flex().flex_col().gap_0p5().children(Page::ALL.iter().map(|&p| {
                let on = p == self.page;
                div()
                    .id(SharedString::from(format!("nav-{}", p.label())))
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(on, |d| d.bg(theme::selected()).text_color(theme::text()))
                    .when(!on, |d| d.text_color(theme::muted()).hover(|s| s.bg(theme::hover())))
                    .child(div().w(px(16.)).text_color(if on { theme::accent() } else { theme::faint() }).child(p.icon()))
                    .child(p.label())
                    .on_click(cx.listener(move |this, _, _, cx| this.set_page(p, cx)))
            })))
            .child(div().flex_1())
            .child(
                div().m_3().p_3().rounded_lg().bg(theme::card()).border_1().border_color(theme::border()).flex().flex_col().gap_1()
                    .child(
                        div().flex().items_center().gap_2().text_xs()
                            .child(div().size(px(7.)).rounded_full().bg(phase_color(&phase)))
                            .child(div().text_color(theme::text()).child(match &phase {
                                Phase::Failed(_) => "出错".to_string(),
                                p => p.label(),
                            }))
                            .when(!hw.is_empty() && phase.busy(), |d| d.child(div().text_color(theme::faint()).child(hw.clone()))),
                    )
                    .child(label(match bal {
                        Some(b) => format!("CU 余额 {b:.1}"),
                        None => "CU 余额 —".into(),
                    }))
                    .when_some(acct, |d, a| d.child(label(format!("账号：{a}")))),
            )
    }

    fn render_topbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(TOPBAR_H))
            .flex_shrink_0()
            .flex()
            .items_center()
            .px_5()
            .border_b_1()
            .border_color(theme::border())
            .window_control_area(WindowControlArea::Drag)
            .child(div().flex_1().text_sm().font_weight(FontWeight::MEDIUM).child(self.page.label()))
            .child(
                div().id("toggle-theme").w(px(22.)).h(px(22.)).flex().items_center().justify_center().rounded_md()
                    .text_sm().text_color(theme::faint()).cursor_pointer()
                    .hover(|s| s.bg(theme::hover()).text_color(theme::text()))
                    .child(if theme::current().is_dark { "☀" } else { "☾" })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_theme(cx))),
            )
    }

    fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        let p = theme::next();
        let mut c = self.engine.config();
        c.theme = p.key.into();
        self.engine.set_config(c);
        cx.notify();
    }

    // ---------------------------------------------------------------- 控制台

    fn render_home(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = self.engine.config();
        let idle_left = self.engine.idle_left();
        let max_left = self.engine.max_left();
        let s = self.engine.state();
        let phase = s.phase();
        let busy = phase.busy();
        let running = phase == Phase::Running;
        let port = s.local_port;
        let url = format!("http://127.0.0.1:{port}");

        // ---- 顶部：状态 + 开关机
        let status = div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_w(px(0.))
            .child(
                div().flex().items_center().gap_2()
                    .child(div().size(px(10.)).rounded_full().bg(phase_color(&phase)))
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(match &phase {
                        Phase::Failed(_) => "出错了".to_string(),
                        p => p.label(),
                    })),
            )
            .child(match &phase {
                Phase::Failed(m) => div().text_xs().text_color(theme::bad()).child(m.clone()),
                Phase::Running => div().text_xs().text_color(theme::muted()).child(format!(
                    "{} · 已开机 {} · ComfyUI 在 {url}",
                    if s.hardware.is_empty() { cfg.gpu.clone() } else { s.hardware.clone() },
                    s.up_since.map(|t| fmt_duration(t.elapsed().as_secs())).unwrap_or_default()
                )),
                Phase::Off => div().text_xs().text_color(theme::muted()).child(match &s.auto_stopped {
                    Some(r) => format!("上次因为「{r}」自动关机。选好 GPU 后点「开机」"),
                    None => "选好 GPU 后点「开机」，大约 1~3 分钟就能用".to_string(),
                }),
                _ => div().text_xs().text_color(theme::muted()).child(
                    s.boot_started.map(|t| format!("已用时 {}", fmt_duration(t.elapsed().as_secs()))).unwrap_or_default(),
                ),
            });

        let mut actions = div().flex().items_center().gap_2().flex_shrink_0();
        if running {
            let u2 = url.clone();
            actions = actions
                .child(primary("open-comfy", "打开 ComfyUI ↗", theme::accent()).on_click(cx.listener(move |_, _, _, cx| cx.open_url(&u2))))
                .child(button("copy-url", "复制地址", true).on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    this.toast("已复制", false);
                })));
        }
        actions = match &phase {
            Phase::Off | Phase::Failed(_) => actions.child(primary("power-on", "开机", theme::accent()).on_click(cx.listener(|this, _, _, cx| {
                this.engine.start();
                cx.notify();
            }))),
            Phase::Checking | Phase::Stopping => actions.child(button("power-wait", phase.label(), false)),
            _ => actions.child(
                div().id("power-off").px_4().py_1p5().rounded_md().border_1().border_color(theme::bad()).text_sm()
                    .text_color(theme::bad()).cursor_pointer().hover(|s| s.bg(theme::bad().opacity(0.1)))
                    .child(if running { "关机" } else { "取消并关机" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.engine.stop(None);
                        cx.notify();
                    })),
            ),
        };

        let gpu_row = div().flex().items_center().gap_1p5().flex_wrap()
            .child(label("GPU").mr_1())
            .children(GPUS.iter().map(|&g| {
                let on = cfg.gpu == g;
                chip(SharedString::from(format!("gpu-{g}")), g, on, !busy).when(!busy, |d| {
                    d.on_click(cx.listener(move |this, _, _, cx| {
                        let mut c = this.engine.config();
                        c.gpu = g.into();
                        this.engine.set_config(c);
                        cx.notify();
                    }))
                })
            }))
            .child(label(match cfg.gpu.as_str() {
                "T4" => "16 GB 显存，最省 CU",
                "L4" => "24 GB 显存，性价比高",
                "A100" => "40/80 GB 显存，大模型、视频",
                "H100" => "80 GB 显存，最快最贵",
                "G4" => "RTX PRO 6000，96 GB 显存",
                _ => "没有 GPU，只适合测试",
            }).ml_2());

        let active = remote::account();
        let acct_row = (s.accounts.len() > 1).then(|| {
            div().flex().items_center().gap_1p5().flex_wrap()
                .child(label("账号").mr_1())
                .children(s.accounts.iter().map(|a| {
                    let on = a.id == active;
                    let id = a.id.clone();
                    let text = match a.balance {
                        Some(b) => format!("{} · {b:.1} CU", a.name),
                        None => format!("{} · —", a.name),
                    };
                    chip(SharedString::from(format!("acct-{}", a.id)), text, on, !busy || on).when(!busy && !on, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            if let Err(e) = this.engine.account_use(&id) {
                                this.toast(e.to_string(), true);
                            }
                            cx.notify();
                        }))
                    })
                }))
                .child(label(if busy { "关机后才能换账号" } else { "点一下换成这个账号开机" }).ml_2())
        });

        let head = card()
            .gap_4()
            .child(div().flex().items_center().gap_4().child(status).child(actions))
            .child(gpu_row)
            .children(acct_row);

        // ---- 开机步骤
        let show_steps = !matches!(phase, Phase::Off | Phase::Checking) || !s.steps_done.is_empty();
        let steps = show_steps.then(|| {
            let cur = s.step.clone();
            let failed = matches!(phase, Phase::Failed(_));
            div().flex().gap_1().children(STEPS.iter().filter(|(k, _)| *k != "drive" || cfg.mount_drive).map(|(k, name)| {
                let done = s.steps_done.iter().find(|(d, _)| d == k).map(|(_, t)| *t);
                let active = cur.as_deref() == Some(*k);
                let (mark, color) = if done.is_some() {
                    ("✓", theme::ok())
                } else if active && failed {
                    ("✕", theme::bad())
                } else if active {
                    ("●", theme::warn())
                } else {
                    ("○", theme::faint())
                };
                let t = match (done, active) {
                    (Some(t), _) => format!("{t:.0} 秒"),
                    (None, true) => s.step_started.map(|t| format!("{} 秒", t.elapsed().as_secs())).unwrap_or_default(),
                    _ => String::new(),
                };
                div().flex_1().min_w(px(0.)).flex().flex_col().gap_1p5()
                    .child(div().h(px(3.)).rounded_full().bg(if done.is_some() { theme::ok() } else if active { color } else { theme::track() }))
                    .child(
                        div().flex().items_center().gap_1().text_xs()
                            .child(div().text_color(color).child(mark))
                            .child(div().text_color(if active || done.is_some() { theme::text() } else { theme::faint() }).child(*name)),
                    )
                    .child(label(t))
            }))
        });

        // ---- Drive 授权
        let auth = s.drive_auth_url.clone().map(|u| {
            let u2 = u.clone();
            card()
                .border_color(theme::warn())
                .gap_3()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("需要授权 Google Drive"))
                .child(div().text_xs().text_color(theme::muted()).child(
                    "已经在浏览器里打开了 Google 授权页。登录并点「允许」后，回到这里点「我已授权」。每次新开 VM 都要授权一次，这是 Colab 的规定。",
                ))
                .child(
                    div().flex().gap_2()
                        .child(primary("drive-done", "我已授权，继续", theme::accent()).on_click(cx.listener(|this, _, _, cx| {
                            this.engine.drive_continue();
                            cx.notify();
                        })))
                        .child(button("drive-open", "重新打开授权页", true).on_click(cx.listener(move |_, _, _, cx| cx.open_url(&u2)))),
                )
        });

        // ---- 数据卡片
        let rate = s.usage.rate.filter(|r| *r > 0.0);
        let est_rate = rate.unwrap_or(match cfg.gpu.as_str() {
            "T4" => 1.2,
            "L4" => 1.7,
            "A100" => 5.4,
            "H100" => 9.8,
            "G4" => 6.0,
            _ => 0.1,
        });
        let spent = s.up_since.filter(|_| busy).map(|t| t.elapsed().as_secs_f32() / 3600.0 * est_rate);
        let cu_card = card()
            .flex_1()
            .min_w(px(200.))
            .gap_1()
            .child(label("CU 余额"))
            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child(s.usage.balance.map(|b| format!("{b:.1}")).unwrap_or("—".into())))
            .child(label(match rate {
                Some(r) => format!("当前消耗 {r:.2} CU/小时"),
                None if busy => format!("{} 约 {est_rate:.1} CU/小时（估算）", cfg.gpu),
                None => "未开机，不消耗".into(),
            }))
            .when_some(spent, |d, sp| d.child(label(format!("本次约用了 {sp:.2} CU"))))
            .when_some(s.usage.balance.filter(|_| busy), |d, b| d.child(label(format!("按当前速度还能用约 {:.0} 小时", b / est_rate))));

        let idle_card = card()
            .flex_1()
            .min_w(px(200.))
            .gap_1()
            .child(label("自动关机"))
            .child(match idle_left {
                Some(l) => div()
                    .text_2xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(if l < Duration::from_secs(180) { theme::warn() } else { theme::text() })
                    .child(mmss(l)),
                None => div().text_2xl().font_weight(FontWeight::SEMIBOLD).text_color(theme::faint()).child("—"),
            })
            .child(label(if cfg.idle_min == 0 { "闲置不关机".to_string() } else { format!("闲置 {} 分钟后关机（排队或 GPU 在算都算活动）", cfg.idle_min) }))
            .when_some(max_left, |d, l| d.child(label(format!("最长开机还剩 {}", fmt_duration(l.as_secs())))))
            .when(running && cfg.idle_min > 0, |d| {
                d.child(div().pt_1().child(button("extend", "延长（重新计时）", true).on_click(cx.listener(|this, _, _, cx| {
                    this.engine.extend();
                    this.toast("已重新计时", false);
                    cx.notify();
                }))))
            });

        let gpu_card = s.gpu.clone().map(|g| {
            card()
                .flex_1()
                .min_w(px(220.))
                .gap_2()
                .child(div().flex().justify_between().child(label("GPU")).child(label(g.name.clone())))
                .child(div().flex().justify_between().text_xs().child("利用率").child(format!("{:.0}%", g.util)))
                .child(bar("gpu-util", g.util, theme::level(g.util)))
                .child(div().flex().justify_between().text_xs().child("显存").child(format!(
                    "{:.1} / {:.1} GB",
                    g.mem_used / 1024.0,
                    g.mem_total / 1024.0
                )))
                .child(bar("gpu-mem", pct(g.mem_used, g.mem_total), theme::level(pct(g.mem_used, g.mem_total))))
                .child(label(format!("{:.0}°C · {:.0} W", g.temp, g.power)))
        });

        let sys_card = (running && s.mem.1 > 0).then(|| {
            let mp = pct(s.mem.0 as f32, s.mem.1 as f32);
            let dp = pct(s.disk.0 as f32, s.disk.1 as f32);
            card()
                .flex_1()
                .min_w(px(220.))
                .gap_2()
                .child(div().flex().justify_between().child(label("VM")).child(label(if s.comfy_up { "ComfyUI 正常" } else { "ComfyUI 无响应" })))
                .child(div().flex().justify_between().text_xs().child("内存").child(format!("{} / {}", fmt_bytes(s.mem.0), fmt_bytes(s.mem.1))))
                .child(bar("vm-mem", mp, theme::level(mp)))
                .child(div().flex().justify_between().text_xs().child("磁盘").child(format!("{} / {}", fmt_bytes(s.disk.0), fmt_bytes(s.disk.1))))
                .child(bar("vm-disk", dp, theme::level(dp)))
                .child(label(match s.queue {
                    (0, 0) => "队列空闲".to_string(),
                    (r, p) => format!("正在生成 {r} 个，排队 {p} 个"),
                }))
        });

        // ---- 最近出图
        let recent: Vec<engine::Output> = s.outputs.iter().take(8).cloned().collect();
        let syncing = s.syncing.clone();
        let recent_card = (!recent.is_empty()).then(|| {
            card()
                .gap_3()
                .child(
                    div().flex().items_center().justify_between()
                        .child(div().flex().gap_2().items_center().child(label("最近出图")).when_some(syncing.clone(), |d, f| d.child(label(format!("正在同步 {f}…")))))
                        .child(button("all-out", "全部 →", true).on_click(cx.listener(|this, _, _, cx| this.set_page(Page::Outputs, cx)))),
                )
                .child(div().flex().gap_2().children(recent.into_iter().map(|o| thumb(&o, 112.0, cx))))
        });

        // ---- 日志尾部
        let show_log = !matches!(phase, Phase::Running | Phase::Off | Phase::Checking) || matches!(phase, Phase::Failed(_));
        let log_tail: Vec<String> = if show_log { s.log.iter().rev().take(14).rev().cloned().collect() } else { vec![] };
        drop(s);
        let log_card = (!log_tail.is_empty()).then(|| {
            card()
                .gap_1()
                .child(
                    div().flex().justify_between().items_center().pb_1()
                        .child(label("日志"))
                        .child(button("full-log", "完整日志 →", true).on_click(cx.listener(|this, _, _, cx| {
                            this.comfy_log_tab = false;
                            this.set_page(Page::Logs, cx)
                        }))),
                )
                .children(log_tail.into_iter().map(|l| mono().text_color(theme::muted()).overflow_hidden().whitespace_nowrap().text_ellipsis().child(l)))
        });

        div()
            .id("home")
            .size_full()
            .overflow_y_scroll()
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(head.children(steps))
            .children(auth)
            .child(div().flex().flex_wrap().gap_4().child(cu_card).child(idle_card).children(gpu_card).children(sys_card))
            .children(recent_card)
            .children(log_card)
            .into_any_element()
    }

    // ---------------------------------------------------------------- 模型

    fn render_models(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = self.engine.config();
        let sel_budget = self.engine.budget(&cfg.ssd_models);
        let cur: std::collections::BTreeSet<&String> = cfg.ssd_models.iter().collect();
        let profiles: Vec<(String, Vec<String>, engine::Budget, bool)> = cfg
            .ssd_profiles
            .iter()
            .map(|(n, k)| (n.clone(), k.clone(), self.engine.budget(k), k.iter().collect::<std::collections::BTreeSet<_>>() == cur))
            .collect();
        let s = self.engine.state();
        let running = s.phase() == Phase::Running;
        let mut cats: Vec<String> = s.models.iter().map(|m| m.category.clone()).collect();
        cats.dedup();
        if cats.is_empty() {
            cats = DEFAULT_CATS.iter().map(|c| c.to_string()).collect();
        }
        let dl_cat = self.dl_category.clone();

        let form = card()
            .gap_3()
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("下载模型"))
            .child(label(format!(
                "在 Colab VM 上直接下载（机房带宽，通常几百 MB/s），保存到 {}/<类别>/{}",
                cfg.models_root(),
                if cfg.mount_drive { "，存在 Drive 里，下次开机还在" } else { "（没接 Drive，关机后会丢失）" }
            )))
            .child(div().child(self.inputs.dl_url.clone()))
            .child(div().flex().items_center().gap_1p5().flex_wrap().child(label("类别").mr_1()).children(cats.iter().map(|c| {
                let c2 = c.clone();
                chip(SharedString::from(format!("dlcat-{c}")), c.clone(), *c == dl_cat, true).on_click(cx.listener(move |this, _, _, cx| {
                    this.dl_category = c2.clone();
                    cx.notify();
                }))
            })))
            .child(
                div().flex().items_center().gap_2()
                    .child(div().flex_1().child(self.inputs.dl_name.clone()))
                    .child(if running {
                        primary("dl-go", "下载", theme::accent()).on_click(cx.listener(|this, _, _, cx| this.submit_download(cx)))
                    } else {
                        button("dl-go", "开机后可下载", false)
                    }),
            )
            .child(label("需要登录的模型（如 FLUX dev、部分 Civitai 模型）请先在「设置」里填 Hugging Face / Civitai 令牌。"));

        let dls = s.downloads.clone();
        let has_finished = dls.iter().any(|d| !d.active());
        let dl_card = (!dls.is_empty()).then(|| {
            card()
                .gap_3()
                .child(
                    div().flex().justify_between().items_center().child(label("下载 / 传输任务")).when(has_finished, |d| {
                        d.child(button("dl-clear", "清除已结束", true).on_click(cx.listener(|this, _, _, cx| {
                            this.engine.clear_finished_downloads();
                            cx.notify();
                        })))
                    }),
                )
                .children(dls.into_iter().map(|d| {
                    let p = if d.total > 0 { d.done as f32 / d.total as f32 * 100.0 } else { 0.0 };
                    let (st, color) = match &d.status {
                        DlStatus::Running if d.total > 0 => (
                            format!("{} / {} · {} · {:.0}%", fmt_bytes(d.done), fmt_bytes(d.total), engine::fmt_speed(d.speed), p),
                            theme::accent(),
                        ),
                        DlStatus::Running => (if d.done > 0 { format!("{} · {}", fmt_bytes(d.done), engine::fmt_speed(d.speed)) } else { "连接中…".into() }, theme::accent()),
                        DlStatus::Queued => ("排队中…".into(), theme::faint()),
                        DlStatus::Done => (format!("完成 · {}", fmt_bytes(d.done)), theme::ok()),
                        DlStatus::Failed(m) => (format!("失败：{m}"), theme::bad()),
                        DlStatus::Cancelled if d.to_ssd => ("已取消".into(), theme::faint()),
                        DlStatus::Cancelled => ("已取消（再次下载同一地址可以续传）".into(), theme::faint()),
                    };
                    let id = d.id;
                    div().flex().flex_col().gap_1p5()
                        .child(
                            div().flex().items_center().gap_2()
                                .child(div().flex_1().min_w(px(0.)).text_sm().overflow_hidden().whitespace_nowrap().text_ellipsis().child(d.name.clone()))
                                .child(label(if d.to_ssd { format!("{} → SSD", d.category) } else { d.category.clone() }))
                                .when(d.active(), |x| {
                                    x.child(button(("dl-cancel", id as usize), "取消", true).on_click(cx.listener(move |this, _, _, cx| {
                                        this.engine.cancel_download(id);
                                        cx.notify();
                                    })))
                                }),
                        )
                        .child(bar(&format!("dl-{id}"), if d.status == DlStatus::Done { 100.0 } else { p }, color))
                        .child(div().text_xs().text_color(if matches!(d.status, DlStatus::Failed(_)) { theme::bad() } else { theme::faint() }).child(st))
                }))
        });

        // 已有模型
        let sel = self.model_cat.clone();
        let mut groups: Vec<(String, usize, u64)> = Vec::new();
        for m in s.models.iter() {
            if groups.last().map(|g| g.0 != m.category).unwrap_or(true) {
                groups.push((m.category.clone(), 0, 0));
            }
            if !m.name.is_empty() {
                let g = groups.last_mut().unwrap();
                g.1 += 1;
                g.2 += m.size;
            }
        }
        let total: u64 = groups.iter().map(|g| g.2).sum();
        let rows: Vec<engine::Model> =
            s.models.iter().filter(|m| !m.name.is_empty() && sel.as_ref().map(|c| *c == m.category).unwrap_or(true)).cloned().collect();
        let confirm = self.confirm_delete.clone();
        let ssd_on = cfg.ssd_enabled();
        let chosen: std::collections::HashSet<String> = cfg.ssd_models.iter().cloned().collect();
        // 每个模型当前的 SSD 复制任务（排队 / 进行中）
        let copying: std::collections::HashMap<String, engine::Download> =
            s.downloads.iter().filter(|d| d.to_ssd && d.active()).map(|d| (format!("{}/{}", d.category, d.name), d.clone())).collect();
        let sizes: std::collections::HashMap<String, (u64, bool)> = s.models.iter().filter(|m| !m.name.is_empty()).map(|m| (m.key(), (m.size, m.ssd))).collect();
        let ready = chosen.iter().filter(|k| sizes.get(*k).map(|x| x.1).unwrap_or(false)).count();
        let free = s.disk.1.saturating_sub(s.disk.0);
        let ssd_card = ssd_on.then(|| {
            card()
                .gap_2()
                .child(
                    div().flex().items_center().gap_2()
                        .child(div().flex_1().text_sm().font_weight(FontWeight::MEDIUM).child("本地 SSD 加速"))
                        .child(chip("ssd-auto", "开机自动传输", cfg.ssd_auto, true).on_click(cx.listener(|this, _, _, cx| {
                            let mut c = this.engine.config();
                            c.ssd_auto = !c.ssd_auto;
                            this.engine.set_config(c);
                            cx.notify();
                        })))
                        .child(chip("ssd-learn", "运行时自动复制", cfg.ssd_learn, true).on_click(cx.listener(|this, _, _, cx| {
                            let mut c = this.engine.config();
                            c.ssd_learn = !c.ssd_learn;
                            this.engine.set_config(c);
                            cx.notify();
                        })))
                        .when(running && ready < chosen.len(), |d| {
                            d.child(button("ssd-sync", "传输未完成的", true).on_click(cx.listener(|this, _, _, cx| {
                                this.engine.ssd_sync();
                                cx.notify();
                            })))
                        })
                        .when(running, |d| {
                            let open = self.wf_open;
                            d.child(chip("wf-open", "从工作流导入…", open, true).on_click(cx.listener(move |this, _, _, cx| {
                                this.wf_open = !open;
                                if this.wf_open {
                                    this.engine.load_workflows();
                                }
                                cx.notify();
                            })))
                        }),
                )
                .child(label(
                    "从 Drive 挂载目录读模型很慢（没缓存过的文件只有 35–50 MB/s）。勾选下面列表里的「SSD」，模型会复制到 VM 本地 SSD，ComfyUI 优先从 SSD 读。\
                     直接走 Drive API 多连接下载（约 300 MB/s），同时复制 2 个文件。选择会记住；关机后 SSD 上的副本会清空，Drive 里的原文件不受影响。                     「运行时自动复制」：ComfyUI 开始跑工作流时，把它用到、还不在 SSD 上的模型复制过去，下次加载就快了。",
                ))
                .child(
                    div().flex().gap_4().flex_wrap().text_xs().text_color(theme::muted())
                        .child(
                            div()
                                .when(!sel_budget.fits(), |d| d.text_color(theme::warn()))
                                .child(format!("已选 {} 个 · {}", chosen.len(), budget_label(&sel_budget))),
                        )
                        .when(running, |d| d.child(format!("已就绪 {ready} 个")))
                        .when(s.disk.1 > 0, |d| d.child(format!("SSD 剩余 {}", fmt_bytes(free)))),
                )
                .child(self.render_profiles(&profiles, &s, running, cx))
                .when(running && self.wf_open, |d| d.child(self.render_wf_import(&s, cx)))
        });
        let list = card()
            .gap_3()
            .child(
                div().flex().items_center().gap_2()
                    .child(div().flex_1().text_sm().font_weight(FontWeight::MEDIUM).child(format!("已有模型 · {}", fmt_bytes(total))))
                    .when(s.models_loading, |d| d.child(label("读取中…")))
                    .child(button("models-refresh", "刷新", running).when(running, |b| b.on_click(cx.listener(|this, _, _, cx| {
                        this.engine.load_models();
                        cx.notify();
                    })))),
            )
            .when_some(s.models_error.clone(), |d, e| d.child(div().text_xs().text_color(theme::faint()).child(e)))
            .when(!running && s.models.is_empty(), |d| d.child(label("开机后显示 Drive 里的模型。")))
            .child(
                div().flex().gap_1p5().flex_wrap()
                    .when(!groups.is_empty(), |d| {
                        d.child(chip("mcat-all", "全部", sel.is_none(), true).on_click(cx.listener(|this, _, _, cx| {
                            this.model_cat = None;
                            cx.notify();
                        })))
                    })
                    .children(groups.iter().map(|(c, n, sz)| {
                        let c2 = c.clone();
                        chip(SharedString::from(format!("mcat-{c}")), format!("{c} {n} · {}", fmt_bytes(*sz)), sel.as_ref() == Some(c), true)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.model_cat = Some(c2.clone());
                                cx.notify();
                            }))
                    })),
            )
            .child(div().flex().flex_col().children(rows.into_iter().enumerate().map(|(i, m)| {
                let key = (m.category.clone(), m.name.clone());
                let asking = confirm.as_ref() == Some(&key);
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .py_1p5()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(div().flex_1().min_w(px(0.)).text_sm().overflow_hidden().whitespace_nowrap().text_ellipsis().child(m.name.clone()))
                    .when(sel.is_none(), |d| d.child(label(m.category.clone())))
                    .child(div().w(px(80.)).text_xs().text_color(theme::muted()).text_right().child(fmt_bytes(m.size)))
                    .when(ssd_on, |d| {
                        let k = m.key();
                        let picked = chosen.contains(&k);
                        let task = copying.get(&k);
                        let (text, color) = match (picked, task, m.ssd) {
                            (true, Some(t), _) if t.status == DlStatus::Running && t.total > 0 => {
                                (format!("SSD {:.0}%", t.done as f32 / t.total as f32 * 100.0), None)
                            }
                            (true, Some(t), _) if t.status == DlStatus::Running => ("SSD 复制中".to_string(), None),
                            (true, Some(_), _) => ("SSD 排队".to_string(), None),
                            (true, None, true) => ("SSD ✓".to_string(), Some(theme::ok())),
                            (true, None, false) => ("SSD 未复制".to_string(), Some(theme::warn())),
                            (false, _, _) => ("SSD".to_string(), None),
                        };
                        let (cat, name, size, on_ssd) = (m.category.clone(), m.name.clone(), m.size, m.ssd);
                        let mut c = chip(("mssd", i), text, picked, true).w(px(96.)).flex().justify_center();
                        if let Some(col) = color {
                            c = c.text_color(col).border_color(col).bg(col.opacity(0.12));
                        }
                        d.child(c.on_click(cx.listener(move |this, _, _, cx| {
                            if !picked && !on_ssd {
                                // 空间检查：留 3 GB 余量
                                let (used, total) = this.engine.state().disk;
                                let pending: u64 = this
                                    .engine
                                    .state()
                                    .downloads
                                    .iter()
                                    .filter(|d| d.to_ssd && d.active())
                                    .map(|d| d.total.saturating_sub(d.done))
                                    .sum();
                                if total > 0 && size + pending + (3 << 30) > total.saturating_sub(used) {
                                    this.toast(
                                        format!("本地 SSD 空间不够：剩 {}，这个模型 {}（加上正在复制的）", fmt_bytes(total.saturating_sub(used)), fmt_bytes(size + pending)),
                                        true,
                                    );
                                    cx.notify();
                                    return;
                                }
                            }
                            this.engine.ssd_select(&cat, &name, !picked);
                            cx.notify();
                        })))
                    })
                    .child(if asking {
                        button(("mdel", i), "确认删除", true).text_color(theme::bad()).border_color(theme::bad()).on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm_delete = None;
                            this.engine.delete_model(key.0.clone(), key.1.clone());
                            cx.notify();
                        }))
                    } else {
                        button(("mdel", i), "删除", running).when(running, |b| {
                            b.on_click(cx.listener(move |this, _, _, cx| {
                                this.confirm_delete = Some(key.clone());
                                cx.notify();
                            }))
                        })
                    })
            })));
        drop(s);
        div()
            .id("models")
            .size_full()
            .overflow_y_scroll()
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(form)
            .children(dl_card)
            .children(ssd_card)
            .child(list)
            .into_any_element()
    }

    // ---------------------------------------------------------------- 出图

    fn render_outputs(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = self.engine.config();
        let (count, syncing) = {
            let s = self.engine.state();
            (s.outputs.len(), s.syncing.clone())
        };
        let tile = 168.0;
        let cols = (((self.center_w - 40.0) / (tile + 12.0)).floor() as usize).max(1);
        let rows = count.div_ceil(cols);
        let root = PathBuf::from(&cfg.output_local);
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_5()
            .py_3()
            .border_b_1()
            .border_color(theme::border())
            .child(div().flex_1().flex().flex_col().gap_0p5()
                .child(div().text_sm().child(format!("{count} 个文件")))
                .child(label(match syncing {
                    Some(f) => format!("正在同步 {f}…"),
                    None => format!("自动同步到 {}，每次开机一个文件夹。点击打开，⌘+点击在 Finder 中显示", cfg.output_local),
                })))
            .child(button("out-refresh", "刷新", true).on_click(cx.listener(|this, _, _, cx| {
                this.engine.load_local_outputs();
                cx.notify();
            })))
            .child(button("out-finder", "在 Finder 中打开", true).on_click(cx.listener(move |_, _, _, _| {
                let _ = std::fs::create_dir_all(&root);
                open_path(&root);
            })));
        let body = if count == 0 {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(label("还没有出图。开机后在 ComfyUI 里生成的图片和视频会自动出现在这里。"))
                .into_any_element()
        } else {
            uniform_list("out-grid", rows, cx.processor(move |this, range: Range<usize>, _, cx| {
                let outs = this.engine.state().outputs.clone();
                range
                    .map(|r| {
                        div().flex().gap_3().px_5().pb_3().children((0..cols).filter_map(|c| outs.get(r * cols + c)).map(|o| {
                            div().flex().flex_col().gap_1().w(px(tile))
                                .child(thumb(o, tile, cx))
                                .child(div().text_xs().text_color(theme::faint()).overflow_hidden().whitespace_nowrap().text_ellipsis().child(o.rel.clone()))
                        }))
                    })
                    .collect()
            }))
            .track_scroll(self.out_scroll.clone())
            .pt_3()
            .flex_1()
            .into_any_element()
        };
        div().size_full().flex().flex_col().child(header).child(body).into_any_element()
    }

    // ---------------------------------------------------------------- 日志

    fn render_logs(&self, cx: &mut Context<Self>) -> AnyElement {
        let comfy = self.comfy_log_tab;
        let running = self.engine.state().phase() == Phase::Running;
        let n = self.log_len();
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_5()
            .py_2p5()
            .border_b_1()
            .border_color(theme::border())
            .child(chip("log-boot", "开机日志", !comfy, true).on_click(cx.listener(|this, _, _, cx| {
                this.comfy_log_tab = false;
                this.engine.set_want_log(false);
                this.log_follow = true;
                cx.notify();
            })))
            .child(chip("log-comfy", "ComfyUI 日志", comfy, true).on_click(cx.listener(|this, _, _, cx| {
                this.comfy_log_tab = true;
                this.engine.set_want_log(true);
                this.log_follow = true;
                cx.notify();
            })))
            .child(div().flex_1())
            .when(comfy && !running, |d| d.child(label("开机后每 3 秒刷新")))
            .child(button("log-copy", "复制", n > 0).on_click(cx.listener(|this, _, _, cx| {
                let s = this.engine.state();
                let text = if this.comfy_log_tab { s.comfy_log.join("\n") } else { s.log.iter().cloned().collect::<Vec<_>>().join("\n") };
                drop(s);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                this.toast("已复制日志", false);
            })))
            .child(chip("log-follow", "跟随最新", self.log_follow, true).on_click(cx.listener(|this, _, _, cx| {
                this.log_follow = !this.log_follow;
                cx.notify();
            })));
        let body = uniform_list("log-lines", n, cx.processor(move |this, range: Range<usize>, _, _| {
            let s = this.engine.state();
            range
                .filter_map(|i| if comfy { s.comfy_log.get(i).cloned() } else { s.log.get(i).cloned() })
                .map(|l| {
                    let color = if l.starts_with("──") {
                        theme::accent()
                    } else if l.contains("Error") || l.contains("失败") || l.contains("Traceback") || l.trim_start().starts_with('!') {
                        theme::bad()
                    } else if l.starts_with('$') {
                        theme::text()
                    } else {
                        theme::muted()
                    };
                    mono().px_5().h(px(18.)).whitespace_nowrap().overflow_hidden().text_ellipsis().text_color(color).child(l)
                })
                .collect()
        }))
        .track_scroll(self.log_scroll.clone())
        .py_2()
        .flex_1();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(if n == 0 { div().flex_1().flex().items_center().justify_center().child(label("暂无日志")).into_any_element() } else { body.into_any_element() })
            .into_any_element()
    }

    // ---------------------------------------------------------------- 设置

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let i = &self.inputs;
        let row = |title: &str, hint: &str, input: AnyElement| {
            div().flex().gap_4().py_2p5().border_t_1().border_color(theme::border())
                .child(div().w(px(180.)).flex_shrink_0().flex().flex_col().gap_0p5().child(div().text_sm().child(title.to_string())).child(label(hint.to_string())))
                .child(div().flex_1().min_w(px(0.)).child(input))
        };
        let toggle = |id: &'static str, on: bool, text: &'static str| {
            div().id(id).flex().items_center().gap_2().cursor_pointer().text_sm()
                .child(
                    div().w(px(30.)).h(px(17.)).rounded_full().p(px(2.)).bg(if on { theme::accent() } else { theme::track() })
                        .flex().when(on, |d| d.justify_end())
                        .child(div().size(px(13.)).rounded_full().bg(gpui::white())),
                )
                .child(text)
        };
        let colab_ok = remote::colab_installed();
        let colab_card = card()
            .gap_2()
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Colab 命令行"))
            .child(label(if colab_ok {
                format!("使用 {}。登录信息存在 macOS 钥匙串里（主账号是它自己登录的那个，其他账号见上面「Colab 账号」）。", remote::colab_bin().display())
            } else {
                "没有找到 colab 命令。在终端运行：uv tool install google-colab-cli，然后运行一次 colab sessions 登录。".to_string()
            }))
            .child(label(format!("ssh 专用密钥：{}", remote::key_path().display())));
        let accounts_card = self.render_accounts(cx);
        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .child(accounts_card)
            .child(
                card()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).pb_2().child("Google Drive"))
                    .child(row("挂载 Drive", "开机时挂载，模型、工作流、上传图都放 Drive", toggle("t-mount", self.draft_mount, if self.draft_mount { "开" } else { "关" })
                        .on_click(cx.listener(|this, _, _, cx| { this.draft_mount = !this.draft_mount; cx.notify(); })).into_any_element()))
                    .child(row("ComfyUI 资料目录", "VM 上的路径，下面放 models/<类别>/、workflows/、input/", i.drive_root.clone().into_any_element()))
                    .child(row("出图也存 Drive", "开：直接写到上面目录的 output/；关：写在 VM 上（关机即删），只同步回本机", toggle("t-outdrive", self.draft_out_drive, if self.draft_out_drive { "开" } else { "关" })
                        .on_click(cx.listener(|this, _, _, cx| { this.draft_out_drive = !this.draft_out_drive; cx.notify(); })).into_any_element())),
            )
            .child(
                card()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).pb_2().child("自动关机（省 CU）"))
                    .child(row("闲置关机（分钟）", "没有排队任务、GPU 也没在算，超过这么久就关机；0 = 不自动关", i.idle_min.clone().into_any_element()))
                    .child(row("最长开机（小时）", "单次开机超过就关；0 = 不限", i.max_hours.clone().into_any_element()))
                    .child(row("最低 CU 余额", "余额低于这个值就关机", i.min_cu.clone().into_any_element())),
            )
            .child(
                card()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).pb_2().child("ComfyUI"))
                    .child(row("自定义节点", "开机时自动安装（git 地址，空格分隔）", i.custom_nodes.clone().into_any_element()))
                    .child(row("启动参数", "追加给 main.py，如 --highvram、--lowvram、--fast", i.extra_args.clone().into_any_element()))
                    .child(row("本机端口", "ComfyUI 转发到 127.0.0.1 的这个端口，被占用时自动换", i.local_port.clone().into_any_element()))
                    .child(row("出图保存到", "本机文件夹，自动同步", i.output_local.clone().into_any_element())),
            )
            .child(
                card()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).pb_2().child("下载令牌"))
                    .child(row("Hugging Face", "下载需授权的模型时用，保存在本机设置文件（仅自己可读）", i.hf_token.clone().into_any_element()))
                    .child(row("Civitai", "Civitai 部分模型需要", i.civitai_token.clone().into_any_element())),
            )
            .child(colab_card)
            .child(div().flex().justify_end().pb_4().child(primary("save", "保存设置", theme::accent()).on_click(cx.listener(|this, _, _, cx| this.save_settings(cx)))))
            .into_any_element()
    }

    /// 设置页的「Colab 账号」：每个账号的余额、换账号、加账号（登录）、重新登录、改名、删除
    fn render_accounts(&self, cx: &mut Context<Self>) -> gpui::Div {
        let active = remote::account();
        let (list, busy, refreshing, login) = {
            let s = self.engine.state();
            (s.accounts.clone(), s.phase().busy(), s.accounts_busy, s.login.clone())
        };
        let total: f32 = list.iter().filter_map(|a| a.balance).sum();
        let rows = list.iter().map(|a| {
            let on = a.id == active;
            let (id1, id2, id3, id4) = (a.id.clone(), a.id.clone(), a.id.clone(), a.id.clone());
            let confirming = self.confirm_account_rm.as_deref() == Some(a.id.as_str());
            let sub = if a.email.is_empty() {
                if a.id == remote::DEFAULT_ACCOUNT { "colab 命令行登录的账号".to_string() } else { String::new() }
            } else {
                a.email.clone()
            };
            let when = a.error.clone().unwrap_or_else(|| a.checked_ago());
            let mut btns = div().flex().items_center().gap_1p5().flex_shrink_0();
            if confirming {
                btns = btns
                    .child(label("删除后要重新登录才能再用").mr_1())
                    .child(button(SharedString::from(format!("acct-rm-yes-{}", a.id)), "确定删除", true).text_color(theme::bad()).on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm_account_rm = None;
                        match this.engine.account_remove(&id1) {
                            Ok(()) => this.toast("已删除账号", false),
                            Err(e) => this.toast(e.to_string(), true),
                        }
                        cx.notify();
                    })))
                    .child(button(SharedString::from(format!("acct-rm-no-{}", a.id)), "取消", true).on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_account_rm = None;
                        cx.notify();
                    })));
            } else {
                if !on {
                    btns = btns.child(button(SharedString::from(format!("acct-use-{}", a.id)), "用这个", !busy).when(!busy, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            match this.engine.account_use(&id2) {
                                Ok(()) => this.toast("已换账号", false),
                                Err(e) => this.toast(e.to_string(), true),
                            }
                            cx.notify();
                        }))
                    }));
                }
                if a.id != remote::DEFAULT_ACCOUNT {
                    btns = btns
                        .child(button(SharedString::from(format!("acct-login-{}", a.id)), "重新登录", login.is_none()).when(login.is_none(), |d| {
                            d.on_click(cx.listener(move |this, _, _, cx| {
                                if let Err(e) = this.engine.login_start(Some(&id3), "") {
                                    this.toast(e.to_string(), true);
                                }
                                cx.notify();
                            }))
                        }))
                        .when(!on, |d| {
                            d.child(button(SharedString::from(format!("acct-rm-{}", a.id)), "删除", true).on_click(cx.listener(move |this, _, _, cx| {
                                this.confirm_account_rm = Some(id4.clone());
                                cx.notify();
                            })))
                        });
                }
            }
            div().flex().items_center().gap_3().py_2p5().border_t_1().border_color(theme::border())
                .child(div().size(px(8.)).flex_shrink_0().rounded_full().bg(if on { theme::ok() } else { theme::track() }))
                .child(
                    div().flex_1().min_w(px(0.)).flex().flex_col().gap_0p5()
                        .child(div().flex().items_center().gap_2().text_sm()
                            .child(a.name.clone())
                            .when(on, |d| d.child(div().text_xs().text_color(theme::ok()).child("开机用这个"))))
                        .when(!sub.is_empty(), |d| d.child(label(sub))),
                )
                .child(
                    div().w(px(150.)).flex_shrink_0().flex().flex_col().items_end().gap_0p5()
                        .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(a.balance.map(|b| format!("{b:.1} CU")).unwrap_or("—".into())))
                        .child(div().text_xs().text_color(if a.error.is_some() { theme::bad() } else { theme::faint() }).child(when)),
                )
                .child(btns)
        }).collect::<Vec<_>>();

        // 登录面板
        let login_panel = login.map(|l| {
            let mut p = div().mt_2().p_3().rounded_md().border_1().border_color(if l.error.is_some() { theme::bad() } else { theme::warn() }).flex().flex_col().gap_2()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(format!("{}：{}", if l.existing { "重新登录" } else { "添加账号" }, l.name)));
            if let Some(err) = &l.error {
                let (id, name, existing) = (l.id.clone(), l.name.clone(), l.existing);
                p = p.child(div().text_xs().text_color(theme::bad()).child(err.clone())).child(
                    div().flex().gap_2()
                        .child(primary("login-retry", "再试一次", theme::accent()).on_click(cx.listener(move |this, _, _, cx| {
                            this.login_opened = None;
                            let r = if existing { this.engine.login_start(Some(&id), "") } else { this.engine.login_start(None, &name) };
                            if let Err(e) = r {
                                this.toast(e.to_string(), true);
                            }
                            cx.notify();
                        })))
                        .child(button("login-cancel", "取消", true).on_click(cx.listener(|this, _, _, cx| {
                            this.engine.login_cancel();
                            cx.notify();
                        }))),
                );
            } else if l.busy {
                p = p.child(label("正在验证授权码…"));
            } else if let Some(url) = l.url.clone() {
                let u2 = url.clone();
                p = p
                    .child(div().text_xs().text_color(theme::muted()).child(
                        "已经在浏览器里打开了 Google 登录页：选要添加的那个 Google 账号，点「允许」。页面上会显示一串授权码，复制后粘贴到下面。",
                    ))
                    .child(
                        div().flex().items_center().gap_2()
                            .child(div().flex_1().min_w(px(0.)).child(self.inputs.login_code.clone()))
                            .child(primary("login-submit", "提交", theme::accent()).on_click(cx.listener(|this, _, _, cx| {
                                let code = this.inputs.login_code.read(cx).text().trim().to_string();
                                match this.engine.login_code(&code) {
                                    Ok(()) => this.inputs.login_code.update(cx, |t, cx| t.set_text("", None, cx)),
                                    Err(e) => this.toast(e.to_string(), true),
                                }
                                cx.notify();
                            }))),
                    )
                    .child(
                        div().flex().gap_2()
                            .child(button("login-open", "重新打开登录页", true).on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))))
                            .child(button("login-copy", "复制地址", true).on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(u2.clone()));
                                this.toast("已复制", false);
                            })))
                            .child(button("login-cancel", "取消", true).on_click(cx.listener(|this, _, _, cx| {
                                this.engine.login_cancel();
                                cx.notify();
                            }))),
                    );
            } else {
                p = p.child(label("正在准备登录…"));
            }
            p
        });

        let can_add = self.engine.state().login.is_none();
        card()
            .child(
                div().flex().items_center().gap_2().pb_2()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Colab 账号"))
                    .when(list.len() > 1, |d| d.child(label(format!("合计 {total:.1} CU"))))
                    .child(div().flex_1())
                    .child(button("acct-refresh", if refreshing { "正在查…" } else { "刷新余额" }, !refreshing).when(!refreshing, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            this.engine.refresh_accounts();
                            cx.notify();
                        }))
                    })),
            )
            .child(label("开机、挂 Drive、扣 CU 都用标着「开机用这个」的账号；其他账号只显示余额（每 15 分钟查一次）。换账号要先关机。").pb_2())
            .children(rows)
            .child(
                div().flex().items_center().gap_2().pt_2p5().border_t_1().border_color(theme::border())
                    .child(div().flex_1().min_w(px(0.)).child(self.inputs.account_name.clone()))
                    .child(primary("acct-add", "添加账号", theme::accent()).when(!can_add, |d| d.opacity(0.5)).when(can_add, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            let name = this.inputs.account_name.read(cx).text().trim().to_string();
                            this.login_opened = None;
                            match this.engine.login_start(None, &name) {
                                Ok(_) => this.inputs.account_name.update(cx, |t, cx| t.set_text("", None, cx)),
                                Err(e) => this.toast(e.to_string(), true),
                            }
                            cx.notify();
                        }))
                    })),
            )
            .children(login_panel)
    }

    // ---------------------------------------------------------------- 浮层

    fn render_notice(&mut self) -> Option<AnyElement> {
        let n = self.notice.as_ref()?;
        let life = Duration::from_millis(if n.err { 6000 } else { 3000 });
        let age = n.at.elapsed();
        if age >= life {
            self.notice = None;
            return None;
        }
        let fade = Duration::from_millis(400);
        let out = if age + fade > life {
            anim::keep_going();
            (life - age).as_secs_f32() / fade.as_secs_f32()
        } else {
            1.0
        };
        Some(
            div().absolute().bottom_4().left_6().right_6().flex().justify_center().child(
                div().relative().px_4().py_2().rounded_lg().bg(theme::card()).border_1()
                    .border_color(if n.err { theme::bad() } else { theme::border() })
                    .text_xs().text_color(if n.err { theme::bad() } else { theme::text() })
                    .shadow_lg()
                    .child(n.text.clone())
                    .with_animation(("notice", n.id as usize), Animation::new(anim::panel()).with_easing(anim::ease_out), move |d, t| {
                        d.opacity(t.min(out)).top(px(10. * (1. - t)))
                    }),
            )
            .into_any_element(),
        )
    }

    fn render_quit(&self, q: QuitDialog, cx: &mut Context<Self>) -> AnyElement {
        let cfg = self.engine.config();
        let body = match q {
            QuitDialog::Ask => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("VM 还在运行"))
                .child(div().text_xs().text_color(theme::muted()).child(format!(
                    "关掉窗口不会自动释放 Colab VM。选「保持运行」时会留一个后台小进程，继续按设置自动关机（闲置 {} 分钟）并同步出图。",
                    cfg.idle_min
                )))
                .child(
                    div().flex().justify_end().gap_2().pt_1()
                        .child(button("q-cancel", "取消", true).on_click(cx.listener(|this, _, _, cx| {
                            this.quit = None;
                            cx.notify();
                        })))
                        .child(button("q-keep", "保持运行并退出", true).on_click(cx.listener(|this, _, _, cx| this.quit_keep_running(cx))))
                        .child(primary("q-stop", "关机并退出", theme::bad()).on_click(cx.listener(|this, _, _, cx| {
                            this.quit = Some(QuitDialog::Stopping);
                            this.engine.stop(None);
                            cx.notify();
                        }))),
                ),
            QuitDialog::Stopping => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("正在关机…"))
                .child(div().text_xs().text_color(theme::muted()).child("同步最后的出图并释放 VM，完成后自动退出。")),
        };
        div()
            .id("quit-backdrop")
            .absolute()
            .inset_0()
            .bg(theme::backdrop())
            .flex()
            .items_center()
            .justify_center()
            .on_click(|_, _, _| {})
            .child(div().w(px(420.)).p_5().rounded_xl().bg(theme::card()).border_1().border_color(theme::border()).shadow_lg().child(body))
            .into_any_element()
    }
}

/// 出图缩略图：图片直接显示，视频 / 其他显示类型；点击用系统默认程序打开
fn thumb(o: &engine::Output, w: f32, cx: &mut Context<Cumulus>) -> AnyElement {
    let p = o.local.clone();
    let name = o.local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = name.rsplit('.').next().unwrap_or("").to_uppercase();
    let inner = if engine::is_image(&name) {
        img(p.clone()).size_full().object_fit(ObjectFit::Cover).into_any_element()
    } else {
        div().size_full().flex().flex_col().items_center().justify_center().gap_1()
            .child(div().text_2xl().text_color(theme::faint()).child(if ["MP4", "WEBM", "MOV", "MKV"].contains(&ext.as_str()) { "▶" } else { "♪" }))
            .child(label(ext))
            .into_any_element()
    };
    div()
        .id(SharedString::from(format!("thumb-{}", o.local.display())))
        .w(px(w))
        .h(px(w))
        .flex_shrink_0()
        .rounded_md()
        .overflow_hidden()
        .bg(theme::sidebar())
        .border_1()
        .border_color(theme::border())
        .cursor_pointer()
        .hover(|s| s.border_color(theme::accent()))
        .child(inner)
        .on_click(cx.listener(move |_, ev: &gpui::ClickEvent, _, cx| {
            if ev.modifiers().platform {
                cx.reveal_path(&p);
            } else {
                open_path(&p);
            }
        }))
        .into_any_element()
}

impl Render for Cumulus {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.center_w = f32::from(window.viewport_size().width) - LEFT_W;
        let page = match self.page {
            Page::Home => self.render_home(cx),
            Page::Models => self.render_models(cx),
            Page::Outputs => self.render_outputs(cx),
            Page::Logs => self.render_logs(cx),
            Page::Settings => self.render_settings(cx),
        };
        let page = div()
            .relative()
            .size_full()
            .child(page)
            .with_animation(
                gpui::ElementId::Name(format!("page-{:?}", self.page).into()),
                Animation::new(anim::fade()).with_easing(anim::ease_out),
                |d, t| d.opacity(t).top(px(6. * (1. - t))),
            );
        let notice = self.render_notice();
        let quit = self.quit.map(|q| self.render_quit(q, cx));
        let left = self.render_left(cx).into_any_element();
        let top = self.render_topbar(cx).into_any_element();
        if anim::frame_needed() {
            window.request_animation_frame();
        }
        div()
            .key_context("Cumulus")
            .on_action(cx.listener(|this, _: &ToggleTheme, _, cx| this.toggle_theme(cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                match this.page {
                    Page::Models => this.engine.load_models(),
                    Page::Outputs => this.engine.load_local_outputs(),
                    _ => {}
                }
                cx.notify();
            }))
            .size_full()
            .relative()
            .flex()
            .bg(theme::bg())
            .text_color(theme::text())
            .child(left)
            .child(
                div().flex_1().min_w(px(0.)).h_full().flex().flex_col()
                    .child(top)
                    .child(div().flex_1().min_h(px(0.)).relative().child(page).children(notice)),
            )
            .children(quit)
    }
}

/// 无界面的守护模式：主程序退出但 VM 还在跑时，或者命令行在没开窗口时被调用，
/// 由它掌管 VM：自动关机、出图同步、响应命令行。VM 关着且 2 分钟没有命令就退出
fn guard() {
    let pidf = config::dir().join("guard.pid");
    let engine = Engine::new(Config::load());
    if let Err(e) = control::serve(engine.clone(), "guard") {
        println!("不启动：{e}");
        return;
    }
    let _ = std::fs::write(&pidf, std::process::id().to_string());
    engine.check_existing();
    let mut last = String::new();
    let mut off_since: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        // 主程序重新打开时会改写 pid 文件，让出控制权
        if std::fs::read_to_string(&pidf).map(|s| s.trim() != std::process::id().to_string()).unwrap_or(true) {
            println!("主程序已接管，退出");
            engine.shutdown_local();
            return;
        }
        let (phase, log) = {
            let s = engine.state();
            (s.phase(), s.log.back().cloned().unwrap_or_default())
        };
        if log != last {
            println!("{log}");
            last = log;
        }
        // 命令行发起的登录等用户贴授权码：最多等 15 分钟，等着的时候不退出
        let login_age = engine.state().login.as_ref().map(|l| l.started.map(|t| t.elapsed()).unwrap_or_default());
        if login_age.is_some_and(|t| t > Duration::from_secs(15 * 60)) {
            engine.login_cancel();
        }
        if matches!(phase, Phase::Off | Phase::Failed(_)) {
            let since = *off_since.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_secs(120) && control::idle_secs() > 120 && login_age.is_none() {
                println!("结束：{}", phase.label());
                engine.shutdown_local();
                let _ = std::fs::remove_file(&pidf);
                let _ = std::fs::remove_file(control::sock_path());
                return;
            }
        } else {
            off_since = None;
        }
    }
}

/// 主程序启动时让后台守护进程退出
fn stop_guard() {
    let pidf = config::dir().join("guard.pid");
    if let Ok(pid) = std::fs::read_to_string(&pidf) {
        let _ = std::fs::write(&pidf, "app");
        let pid = pid.trim().to_string();
        if !pid.is_empty() && pid != "app" {
            let _ = std::process::Command::new("/bin/kill").arg(&pid).status();
        }
        let _ = std::fs::remove_file(&pidf);
    }
}

/// 不开窗口，把整个流程真跑一遍：开机 → 挂 Drive → 启动 ComfyUI → 经端口转发提交一个不需要模型的小任务
/// → 等出图同步回本机 → 下载一个小文件测试模型下载 → 关机（`--keep` 则保持运行）
fn selftest(keep: bool) {
    let t0 = Instant::now();
    let engine = Engine::new(Config::load());
    let cfg = engine.config();
    println!("== 配置：GPU {}，Drive {}（{}）", cfg.gpu, cfg.mount_drive, cfg.drive_root);
    let mut printed = 0;
    let mut authed: Option<String> = None;
    let pump = |printed: &mut usize| {
        let s = engine.state();
        for l in s.log.iter().skip(*printed) {
            println!("  | {l}");
        }
        *printed = s.log.len();
    };
    engine.check_existing();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        pump(&mut printed);
        let phase = engine.state().phase();
        match phase {
            Phase::Checking => {}
            Phase::Off => {
                println!("== 开机（{:.0}s）", t0.elapsed().as_secs_f32());
                engine.start();
                std::thread::sleep(Duration::from_millis(500));
            }
            Phase::Failed(m) => {
                println!("== 失败：{m}");
                if !keep {
                    println!("== 释放 VM");
                    let _ = remote::stop_session(&cfg.session);
                }
                engine.shutdown_local();
                return;
            }
            Phase::Running => break,
            _ => {}
        }
        let auth_url = engine.state().drive_auth_url.clone();
        if auth_url.is_some() && auth_url != authed {
            // 等浏览器里点「允许」：授权成功后 Google 会跳到 Colab 页面，这里没法直接知道，按固定时间继续；
            // 没授权完的话引擎会重新请求一次
            println!("== 需要 Drive 授权（浏览器里应已打开授权页），45 秒后继续");
            std::thread::sleep(Duration::from_secs(45));
            engine.drive_continue();
            authed = auth_url;
        }
    }
    let (port, steps) = {
        let s = engine.state();
        (s.local_port, s.steps_done.clone())
    };
    println!("== 就绪，用时 {:.0}s，步骤：{steps:?}", t0.elapsed().as_secs_f32());
    std::thread::sleep(Duration::from_secs(7));
    {
        let s = engine.state();
        println!("== 状态：comfy_up={} gpu={:?} mem={:?} queue={:?} CU={:?}", s.comfy_up, s.gpu, s.mem, s.queue, s.usage);
    }
    // 经本机端口转发提交任务
    let wf = r#"{"prompt":{"1":{"class_type":"EmptyImage","inputs":{"width":256,"height":256,"batch_size":1,"color":14251863}},"2":{"class_type":"SaveImage","inputs":{"images":["1",0],"filename_prefix":"cumulus_selftest"}}}}"#;
    let out = std::process::Command::new("/usr/bin/curl")
        .args(["-s", "-m", "20", "-H", "Content-Type: application/json", "-d", wf, &format!("http://127.0.0.1:{port}/prompt")])
        .output();
    println!("== 提交任务：{}", out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|e| e.to_string()));
    let before = engine.state().outputs.len();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(40) {
        std::thread::sleep(Duration::from_secs(1));
        if engine.state().outputs.len() > before {
            break;
        }
    }
    let (n, first) = {
        let s = engine.state();
        (s.outputs.len(), s.outputs.first().cloned())
    };
    match first {
        Some(o) if n > before => println!("== 出图已同步：{}（{} 字节）", o.local.display(), o.size),
        _ => println!("== 出图没有同步回来"),
    }
    // 模型列表 + 下载
    engine.load_models();
    std::thread::sleep(Duration::from_secs(1));
    while engine.state().models_loading {
        std::thread::sleep(Duration::from_millis(300));
    }
    {
        let s = engine.state();
        let n = s.models.iter().filter(|m| !m.name.is_empty()).count();
        let total: u64 = s.models.iter().map(|m| m.size).sum();
        println!("== 模型：{n} 个，共 {}，错误：{:?}", fmt_bytes(total), s.models_error);
    }
    engine.download_model("https://huggingface.co/openai-community/gpt2/resolve/main/config.json".into(), "cumulus_selftest".into(), String::new());
    let t = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let d = engine.state().downloads.first().cloned();
        match d.map(|d| d.status.clone()) {
            Some(DlStatus::Running | DlStatus::Queued) if t.elapsed() < Duration::from_secs(60) => {}
            st => {
                println!("== 下载结果：{st:?}");
                break;
            }
        }
    }
    // 复制到本地 SSD
    if cfg.ssd_enabled() {
        engine.load_models();
        std::thread::sleep(Duration::from_secs(4));
        engine.ssd_select("cumulus_selftest", "config.json", true);
        let t = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let d = engine.state().downloads.iter().find(|d| d.to_ssd).cloned();
            match d.map(|d| d.status.clone()) {
                Some(DlStatus::Running | DlStatus::Queued) if t.elapsed() < Duration::from_secs(60) => {}
                st => {
                    println!("== 复制到 SSD：{st:?}");
                    break;
                }
            }
        }
        if let Ok(ssh) = remote::Ssh::new(&cfg.session) {
            let out = ssh
                .run(
                    &format!(
                        "ls -la {ssd}/models/cumulus_selftest/; grep -c cumulus_ssd /content/ComfyUI/extra_model_paths.yaml",
                        ssd = engine::SSD_ROOT
                    ),
                    None,
                    Duration::from_secs(30),
                )
                .unwrap_or_else(|e| e.to_string());
            println!("{out}");
        }
    }
    std::thread::sleep(Duration::from_secs(2));
    engine.delete_model("cumulus_selftest".into(), "config.json".into());
    std::thread::sleep(Duration::from_secs(3));
    if let Some(ssh) = remote::Ssh::new(&cfg.session).ok() {
        let _ = ssh.run(&format!("rmdir {}/cumulus_selftest", remote::sh_quote(&cfg.models_root())), None, Duration::from_secs(30));
    }
    pump(&mut printed);
    if keep {
        println!("== 保持运行（--keep），断开本地连接");
        engine.shutdown_local();
        return;
    }
    println!("== 关机");
    engine.stop(None);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        pump(&mut printed);
        let phase = engine.state().phase();
        match phase {
            Phase::Off => break,
            Phase::Failed(m) => {
                println!("== 关机失败：{m}");
                break;
            }
            _ => {}
        }
    }
    println!("== 完成，总用时 {:.0}s", t0.elapsed().as_secs_f32());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(cmd) = args.get(1) {
        if cli::COMMANDS.contains(&cmd.as_str()) {
            std::process::exit(cli::main(&args[1..]));
        }
        if cmd == "--help" || cmd == "-h" {
            std::process::exit(cli::main(&["help".to_string()]));
        }
        // 不是 - 开头的参数都当命令；不认识就报错，而不是打开窗口
        if !cmd.starts_with('-') {
            eprintln!("不认识的命令：{cmd}（cumulus help 查看用法）");
            std::process::exit(2);
        }
    }
    if args.get(1).map(String::as_str) == Some("--guard") {
        return guard();
    }
    if args.get(1).map(String::as_str) == Some("--selftest") {
        return selftest(args.iter().any(|a| a == "--keep"));
    }
    stop_guard();
    let cfg = Config::load();
    theme::set(&cfg.theme);
    let engine = Engine::new(cfg);
    // 命令行（cumulus status 等）经这个接口操作窗口里的同一个引擎
    if let Err(e) = control::serve(engine.clone(), "app") {
        eprintln!("控制接口没有启动：{e}");
    }
    engine.check_existing();
    engine.start_account_poller();
    let start_page = args.iter().position(|a| a == "--page").and_then(|i| args.get(i + 1)).map(|s| Page::from_key(s));

    Application::new().run(move |cx: &mut App| {
        input::bind_keys(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-w", Quit, None),
            KeyBinding::new("cmd-r", Refresh, None),
            KeyBinding::new("cmd-shift-t", ToggleTheme, None),
        ]);
        cx.set_menus(vec![
            Menu { name: "Cumulus".into(), items: vec![MenuItem::action("退出 Cumulus", Quit)] },
            Menu {
                name: "显示".into(),
                items: vec![MenuItem::action("刷新", Refresh), MenuItem::separator(), MenuItem::action("切换深色 / 浅色主题", ToggleTheme)],
            },
        ]);
        let bounds = Bounds::centered(None, size(px(1180.), px(800.)), cx);
        let handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Cumulus".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(14.), px(13.))),
                    }),
                    window_min_size: Some(size(px(820.), px(560.))),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| {
                        let mut v = Cumulus::new(engine.clone(), cx);
                        if let Some(p) = start_page {
                            v.page = p;
                        }
                        v
                    });
                    // 点窗口关闭按钮：VM 在跑时先问一下
                    let v2 = view.clone();
                    window.on_window_should_close(cx, move |_, cx| {
                        v2.update(cx, |v, cx| v.request_quit(cx));
                        false
                    });
                    view
                },
            )
            .unwrap();
        cx.on_action(move |_: &Quit, cx| {
            handle.update(cx, |v, _, cx| v.request_quit(cx)).ok();
        });
        cx.activate(true);
    });
}
