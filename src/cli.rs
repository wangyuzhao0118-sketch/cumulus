//! 命令行：`cumulus <命令>`。方便脚本和 AI 助手操作（加 --json 输出机器可读的结果）。
//! 命令本身不连 VM，都经过掌管进程（见 control.rs）。
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::control::call;
use crate::remote::{self, fmt_bytes, fmt_duration};

pub const COMMANDS: &[&str] = &[
    "status", "up", "down", "drive-continue", "extend", "logs", "models", "download", "ssd", "tasks", "cancel", "rm-model", "outputs", "run", "workflows",
    "interrupt", "exec", "config", "account", "accounts", "help",
];

const HELP: &str = "\
Cumulus 命令行 —— 在 Google Colab 上跑 ComfyUI

用法：cumulus <命令> [参数]      不带命令运行则打开窗口
所有命令都可以加 --json，输出一个 JSON 对象（出错时 {\"ok\": false, \"error\": ...}，退出码 1）。
等待中的进度和日志写到 stderr，结果写到 stdout。

开关机
  status                     当前状态：阶段、GPU、CU 余额、ComfyUI 地址、自动关机倒计时
  up [--gpu L4] [--no-wait]  开机并启动 ComfyUI，默认等到就绪（第一次挂 Drive 要在浏览器里授权，
                             授权后在这里按回车，或另开终端运行 cumulus drive-continue）
  down [--no-wait]           关机（先把出图同步回本机，再释放 VM，停止计费）
  drive-continue             浏览器里授权 Google Drive 后，通知继续挂载
  extend                     重新计算闲置时间，推迟自动关机
  logs [-n 50] [-f]          Cumulus 的运行日志；--comfy 看 ComfyUI 自己的日志

出图
  run <workflow.json|-> [--timeout 1800] [--no-wait] [--no-ssd]
                             提交一个 API 格式的工作流（ComfyUI 里「导出 (API)」），
                             等它跑完并同步回本机，打印本机文件路径。
                             提交前先把它用到、还不在 SSD 上的模型复制过去（--no-ssd 跳过）
  interrupt                  中断 ComfyUI 正在跑的任务
  outputs [-n 20]            本机最近同步回来的出图

模型（在 Drive 的 models/<类别>/ 下，键写成 类别/文件名）
  models [--refresh]         列出模型，标出哪些在本地 SSD 上
  download <类别> <地址> [--name 文件名] [--no-wait]
                             在 VM 上下载模型（Hugging Face / Civitai / 直链），存进 Drive
  ssd add <键>... [--no-wait]  复制到 VM 本地 SSD（读得快很多），会记住，下次开机自动复制
  ssd rm <键>...             不再放 SSD（删掉 SSD 上的副本，Drive 上的不动）
  ssd from <工作流> [--dry-run] [--no-wait]
                             找出工作流用到的模型，全部复制到 SSD。工作流可以是本机 .json 文件、
                             -（stdin），或 ComfyUI 里保存的工作流名字（见 workflows）；界面格式和 API 格式都行。
                             --dry-run 只列出会复制哪些、缺哪些
  ssd sync                   把选中但还没复制的补上
  ssd list                   已选中放 SSD 的模型，合计多大、SSD 放不放得下
  ssd profile                SSD 方案（按工作流分组的一套模型），* 是当前用的
  ssd profile save <名字> [--from <工作流>]
                             把当前选择（或某个工作流用到的模型）存成方案
  ssd profile use <名字>     切换方案：SSD 上只留方案里的模型，别的删掉腾地方，缺的复制
  ssd profile rm <名字>      删除方案（不动 SSD 上的模型）
  rm-model <键> --yes        从 Drive 删除模型（进 Drive 回收站）
  tasks                      下载 / 复制任务；cancel <编号> 取消
  workflows                  ComfyUI 里保存的工作流（接了 Drive 就在 Drive 的 workflows/ 下）

Colab 账号（可以加好几个，各自的 CU 余额都能看）
  account [--refresh]        账号和 CU 余额，* 是开机用的；--refresh 现在去查一遍
  account use <名字>         换开机用的账号（要先关机）
  account add <名字>         添加账号：打印 Google 登录地址，用户在浏览器登录后会拿到授权码；
                             在终端里直接粘贴，或运行 cumulus account code <授权码>
  account code <授权码>      提交授权码，完成登录
  account login <名字>       令牌失效时重新登录
  account rename <名字> <新名字>
  account rm <名字> --yes    删除账号（钥匙串里的令牌一起删）
  account cancel             取消正在进行的登录

其他
  exec <命令...>             在 VM 上执行 shell 命令（复用已有连接），退出码原样返回
  config                     查看设置；config set <键> <值> 修改（如 gpu、idle_min、max_hours、min_cu）

例子
  cumulus up --gpu L4 && cumulus run flux.json && cumulus down
  cumulus status --json
  cumulus exec nvidia-smi
";

struct Args {
    pos: Vec<String>,
    json: bool,
    no_wait: bool,
    follow: bool,
    comfy: bool,
    refresh: bool,
    yes: bool,
    dry_run: bool,
    no_ssd: bool,
    from: Option<String>,
    n: Option<usize>,
    gpu: Option<String>,
    name: Option<String>,
    timeout: Option<u64>,
}

fn parse(args: &[String]) -> Result<Args> {
    let mut a = Args { pos: vec![], json: false, no_wait: false, follow: false, comfy: false, refresh: false, yes: false, dry_run: false, no_ssd: false, from: None, n: None, gpu: None, name: None, timeout: None };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        let mut val = |flag: &str| it.next().cloned().ok_or_else(|| anyhow!("{flag} 后面要跟一个值"));
        match x.as_str() {
            "--json" => a.json = true,
            "--no-wait" => a.no_wait = true,
            "-f" | "--follow" => a.follow = true,
            "--comfy" => a.comfy = true,
            "--refresh" => a.refresh = true,
            "-y" | "--yes" => a.yes = true,
            "--dry-run" => a.dry_run = true,
            "--no-ssd" => a.no_ssd = true,
            "--from" => a.from = Some(val("--from")?),
            "-n" => a.n = Some(val("-n")?.parse().map_err(|_| anyhow!("-n 要跟数字"))?),
            "--gpu" => a.gpu = Some(val("--gpu")?),
            "--name" => a.name = Some(val("--name")?),
            "--timeout" => a.timeout = Some(val("--timeout")?.parse().map_err(|_| anyhow!("--timeout 要跟秒数"))?),
            "-" => a.pos.push(x.clone()),
            s if s.starts_with('-') => bail!("不认识的参数：{s}（cumulus help 查看用法）"),
            _ => a.pos.push(x.clone()),
        }
    }
    Ok(a)
}

/// 入口：args[0] 是命令名。返回退出码
pub fn main(args: &[String]) -> i32 {
    let cmd = args[0].as_str();
    // exec 后面的内容原样交给 VM，不解析参数
    if cmd == "exec" {
        let rest: Vec<&str> = args[1..].iter().map(String::as_str).skip_while(|x| *x == "--").collect();
        return match exec(&rest.join(" ")) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("错误：{e}");
                1
            }
        };
    }
    let json = args.iter().any(|a| a == "--json");
    let res = parse(&args[1..]).and_then(|a| run_cmd(cmd, &a));
    match res {
        Ok(code) => code,
        Err(e) => {
            if json {
                println!("{}", json!({"ok": false, "error": e.to_string()}));
            } else {
                eprintln!("错误：{e}");
            }
            1
        }
    }
}

fn run_cmd(cmd: &str, a: &Args) -> Result<i32> {
    match cmd {
        "help" => {
            print!("{HELP}");
            Ok(0)
        }
        "status" => {
            let v = call(json!({"cmd": "status"}))?;
            emit(a, &v, || status_text(&v));
            Ok(0)
        }
        "up" => up(a),
        "down" => down(a),
        "drive-continue" => simple(a, json!({"cmd": "drive_continue"}), "已通知继续挂载 Drive"),
        "extend" => {
            let v = call(json!({"cmd": "extend"}))?;
            emit(a, &v, || match v["idle_left_s"].as_u64() {
                Some(s) => format!("已推迟，{} 后闲置关机", fmt_duration(s)),
                None => "已推迟".into(),
            });
            Ok(0)
        }
        "logs" => logs(a),
        "models" => models(a),
        "download" => download(a),
        "ssd" => ssd(a),
        "tasks" => {
            let v = call(json!({"cmd": "tasks"}))?;
            emit(a, &v, || {
                let t = v["tasks"].as_array().cloned().unwrap_or_default();
                if t.is_empty() { "没有任务".into() } else { t.iter().map(task_line).collect::<Vec<_>>().join("\n") }
            });
            Ok(0)
        }
        "cancel" => {
            let id: u64 = a.pos.first().and_then(|x| x.parse().ok()).ok_or_else(|| anyhow!("用法：cumulus cancel <任务编号>"))?;
            simple(a, json!({"cmd": "cancel", "id": id}), "已取消")
        }
        "rm-model" => {
            let key = a.pos.first().ok_or_else(|| anyhow!("用法：cumulus rm-model <类别/文件名> --yes"))?;
            if !a.yes {
                bail!("会从 Drive 删除 {key}（进 Drive 回收站），确认的话加 --yes");
            }
            simple(a, json!({"cmd": "delete_model", "key": key}), &format!("已删除 {key}"))
        }
        "outputs" => {
            let v = call(json!({"cmd": "outputs", "n": a.n.unwrap_or(20), "rescan": true}))?;
            emit(a, &v, || {
                let o = v["outputs"].as_array().cloned().unwrap_or_default();
                if o.is_empty() {
                    format!("{} 里还没有出图", v["dir"].as_str().unwrap_or(""))
                } else {
                    o.iter().map(|x| x["path"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join("\n")
                }
            });
            Ok(0)
        }
        "run" => run_workflow(a),
        "workflows" => {
            let v = call(json!({"cmd": "workflows"}))?;
            emit(a, &v, || {
                let w = v["workflows"].as_array().cloned().unwrap_or_default();
                if w.is_empty() {
                    format!("{} 里还没有工作流（在 ComfyUI 里保存一下就有了）", v["dir"].as_str().unwrap_or(""))
                } else {
                    w.iter().map(|x| x["name"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join("\n")
                }
            });
            Ok(0)
        }
        "interrupt" => {
            let port = comfy_port()?;
            http("POST", &format!("http://127.0.0.1:{port}/interrupt"), Some(b"{}"))?;
            emit(a, &json!({"ok": true}), || "已中断".into());
            Ok(0)
        }
        "config" => config(a),
        "account" | "accounts" => account(a),
        _ => bail!("不认识的命令：{cmd}"),
    }
}

fn emit(a: &Args, v: &Value, text: impl FnOnce() -> String) {
    if a.json {
        println!("{v}");
    } else {
        let t = text();
        if !t.is_empty() {
            println!("{t}");
        }
    }
}

fn simple(a: &Args, req: Value, done: &str) -> Result<i32> {
    let v = call(req)?;
    emit(a, &v, || done.to_string());
    Ok(0)
}

// ------------------------------------------------------------------ 状态 / 开关机

fn status_text(v: &Value) -> String {
    let mut o = Vec::new();
    let hw = v["hardware"].as_str().filter(|s| !s.is_empty()).unwrap_or(v["gpu_setting"].as_str().unwrap_or(""));
    o.push(format!("状态：{}（会话 {}，GPU {hw}）", v["phase_label"].as_str().unwrap_or(""), v["session"].as_str().unwrap_or("")));
    if let Some(u) = v["comfy_url"].as_str() {
        o.push(format!("ComfyUI：{u}{}", if v["comfy_up"].as_bool() == Some(true) { "" } else { "（暂时连不上）" }));
    }
    if let Some(u) = v["drive"]["auth_url"].as_str() {
        o.push(format!("等待 Google Drive 授权：{u}"));
    }
    let mut t = Vec::new();
    if let Some(s) = v["uptime_s"].as_u64() {
        t.push(format!("已开机 {}", fmt_duration(s)));
    }
    if let Some(s) = v["idle_shutdown_in_s"].as_u64() {
        t.push(format!("闲置 {} 后关机", fmt_duration(s)));
    }
    if let Some(s) = v["max_shutdown_in_s"].as_u64() {
        t.push(format!("最长还能开 {}", fmt_duration(s)));
    }
    if !t.is_empty() {
        o.push(t.join("，"));
    }
    if let Some(b) = v["cu"]["balance"].as_f64() {
        let rate = v["cu"]["rate_per_hour"].as_f64().map(|r| format!("，每小时 {r:.2}")).unwrap_or_default();
        let who = v["account"]["name"].as_str().map(|n| format!("（账号 {n}）")).unwrap_or_default();
        o.push(format!("CU 余额：{b:.1}{rate}{who}"));
    }
    if let Some(g) = v["gpu"].as_object() {
        let f = |k: &str| g.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        o.push(format!(
            "GPU：{} 占用 {:.0}% · 显存 {:.1}/{:.1} GB · {:.0}°C",
            g.get("name").and_then(Value::as_str).unwrap_or(""),
            f("util"),
            f("mem_used_mb") / 1024.0,
            f("mem_total_mb") / 1024.0,
            f("temp_c")
        ));
    }
    if v["phase"] == "running" {
        o.push(format!("队列：运行 {}，排队 {}", v["queue"]["running"], v["queue"]["pending"]));
        let d = &v["disk"];
        if d["total"].as_u64().unwrap_or(0) > 0 {
            o.push(format!("VM 磁盘：{} / {}", fmt_bytes(d["used"].as_u64().unwrap_or(0)), fmt_bytes(d["total"].as_u64().unwrap_or(0))));
        }
    }
    let n = v["tasks_active"].as_u64().unwrap_or(0);
    if n > 0 {
        o.push(format!("{n} 个下载 / 复制任务进行中（cumulus tasks）"));
    }
    if let Some(r) = v["auto_stopped"].as_str() {
        o.push(format!("上次自动关机：{r}"));
    }
    o.join("\n")
}

/// 跟着日志等到某个阶段；返回最后的状态
fn follow_until(since: &mut u64, done: impl Fn(&str) -> bool, timeout: Duration) -> Result<Value> {
    let t = Instant::now();
    let mut shown_auth: Option<String> = None;
    loop {
        let l = call(json!({"cmd": "logs", "since": *since}))?;
        for line in l["lines"].as_array().into_iter().flatten() {
            eprintln!("  {}", line.as_str().unwrap_or(""));
        }
        *since = l["next"].as_u64().unwrap_or(*since);
        let s = call(json!({"cmd": "status"}))?;
        let phase = s["phase"].as_str().unwrap_or("").to_string();
        if done(&phase) {
            return Ok(s);
        }
        let auth = s["drive"]["auth_url"].as_str().map(String::from);
        if auth.is_some() && auth != shown_auth {
            eprintln!("\n需要授权 Google Drive：浏览器里应该已经打开了授权页（没有的话打开下面的地址），点「允许」：");
            eprintln!("{}", auth.as_deref().unwrap_or(""));
            if std::io::stdin().is_terminal() {
                eprintln!("授权完成后在这里按回车。\n");
            } else {
                eprintln!("授权完成后运行 cumulus drive-continue。\n");
            }
            shown_auth = auth;
        }
        if t.elapsed() > timeout {
            bail!("等了 {} 还是 {}", fmt_duration(timeout.as_secs()), s["phase_label"].as_str().unwrap_or(""));
        }
        std::thread::sleep(Duration::from_millis(800));
    }
}

fn up(a: &Args) -> Result<i32> {
    let before = call(json!({"cmd": "status"}))?;
    let mut since = before["log_seq"].as_u64().unwrap_or(0);
    let mut req = json!({"cmd": "up"});
    if let Some(g) = &a.gpu {
        req["gpu"] = json!(g);
    }
    let r = call(req)?;
    if !a.json {
        if let Some(n) = r["note"].as_str() {
            eprintln!("{n}");
        }
    }
    if a.no_wait {
        emit(a, &r, || "已开始开机（cumulus status 查看进度）".into());
        return Ok(0);
    }
    // 终端里按回车 = Drive 授权完成
    if std::io::stdin().is_terminal() {
        std::thread::spawn(|| {
            let mut line = String::new();
            while std::io::stdin().read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                line.clear();
                let _ = call(json!({"cmd": "drive_continue"}));
            }
        });
    }
    let s = follow_until(&mut since, |p| matches!(p, "running" | "failed" | "off"), Duration::from_secs(a.timeout.unwrap_or(1800)))?;
    let ok = s["phase"] == "running";
    emit(a, &s, || {
        if ok {
            format!("ComfyUI 已就绪：{}", s["comfy_url"].as_str().unwrap_or(""))
        } else {
            format!("开机没有成功：{}", s["phase_label"].as_str().unwrap_or(""))
        }
    });
    Ok(if ok { 0 } else { 1 })
}

fn down(a: &Args) -> Result<i32> {
    let before = call(json!({"cmd": "status"}))?;
    let mut since = before["log_seq"].as_u64().unwrap_or(0);
    let r = call(json!({"cmd": "down"}))?;
    if a.no_wait || r["stopping"] == false {
        emit(a, &r, || r["note"].as_str().unwrap_or("正在关机").to_string());
        return Ok(0);
    }
    let s = follow_until(&mut since, |p| matches!(p, "off" | "failed"), Duration::from_secs(a.timeout.unwrap_or(600)))?;
    let ok = s["phase"] == "off";
    emit(a, &s, || if ok { "已关机，VM 已释放".into() } else { format!("关机失败：{}", s["phase_label"].as_str().unwrap_or("")) });
    Ok(if ok { 0 } else { 1 })
}

fn logs(a: &Args) -> Result<i32> {
    let n = a.n.unwrap_or(50);
    if a.comfy {
        // ComfyUI 的日志带颜色控制符，输出不是终端（脚本 / AI）时去掉
        let strip = if std::io::stdout().is_terminal() { "" } else { r" | sed -u 's/\x1b\[[0-9;]*m//g'" };
        return exec(&format!("tail -n {n} {} /content/comfyui.log{strip}", if a.follow { "-F" } else { "" }));
    }
    let v = call(json!({"cmd": "logs", "n": n}))?;
    let mut since = v["next"].as_u64().unwrap_or(0);
    if a.json && !a.follow {
        println!("{v}");
        return Ok(0);
    }
    for l in v["lines"].as_array().into_iter().flatten() {
        println!("{}", l.as_str().unwrap_or(""));
    }
    while a.follow {
        std::thread::sleep(Duration::from_secs(1));
        let v = call(json!({"cmd": "logs", "since": since}))?;
        for l in v["lines"].as_array().into_iter().flatten() {
            println!("{}", l.as_str().unwrap_or(""));
        }
        since = v["next"].as_u64().unwrap_or(since);
    }
    Ok(0)
}

// ------------------------------------------------------------------ 模型

fn models(a: &Args) -> Result<i32> {
    let v = call(json!({"cmd": "models", "refresh": a.refresh}))?;
    emit(a, &v, || {
        let mut o = vec![format!("模型目录：{}", v["root"].as_str().unwrap_or(""))];
        let list = v["models"].as_array().cloned().unwrap_or_default();
        for m in &list {
            let tag = match (m["on_ssd"].as_bool(), m["ssd_selected"].as_bool()) {
                (Some(true), _) => "  [SSD ✓]",
                (_, Some(true)) => "  [SSD 未复制]",
                _ => "",
            };
            o.push(format!("{}  {}{tag}", m["key"].as_str().unwrap_or(""), fmt_bytes(m["size"].as_u64().unwrap_or(0))));
        }
        let empty: Vec<&str> = v["categories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|c| !list.iter().any(|m| m["category"] == *c))
            .collect();
        if !empty.is_empty() {
            o.push(format!("空的类别：{}", empty.join(" ")));
        }
        o.join("\n")
    });
    Ok(0)
}

fn task_line(t: &Value) -> String {
    let (done, total) = (t["done"].as_u64().unwrap_or(0), t["total"].as_u64().unwrap_or(0));
    let pct = if total > 0 { format!(" {:.0}%", done as f64 * 100.0 / total as f64) } else { String::new() };
    let speed = t["speed_bps"].as_u64().filter(|s| *s > 0 && t["status"] == "running").map(|s| format!(" {}/s", fmt_bytes(s))).unwrap_or_default();
    let err = t["error"].as_str().map(|e| format!("：{e}")).unwrap_or_default();
    format!(
        "#{} {} {}  {}{pct}{speed}{err}",
        t["id"],
        if t["kind"] == "ssd" { "→SSD" } else { "下载" },
        t["key"].as_str().unwrap_or(""),
        t["status"].as_str().unwrap_or("")
    )
}

/// 等一批任务结束，返回 (全部成功, 任务的最终状态)
fn wait_tasks(ids: &[u64]) -> Result<(bool, Vec<Value>)> {
    let tty = std::io::stderr().is_terminal();
    let mut last_print = Instant::now() - Duration::from_secs(60);
    loop {
        let v = call(json!({"cmd": "tasks"}))?;
        let mine: Vec<Value> = v["tasks"].as_array().into_iter().flatten().filter(|t| t["id"].as_u64().is_some_and(|i| ids.contains(&i))).cloned().collect();
        let active = mine.iter().any(|t| matches!(t["status"].as_str(), Some("queued" | "running")));
        if !active {
            if tty {
                eprint!("\r\x1b[K");
            }
            let ok = mine.len() == ids.len() && mine.iter().all(|t| t["status"] == "done");
            return Ok((ok, mine));
        }
        // 终端里原地刷新；不是终端（脚本 / AI）时每 15 秒一行，免得刷屏
        if tty {
            eprint!("\r\x1b[K{}", mine.iter().map(task_line).collect::<Vec<_>>().join(" | "));
        } else if last_print.elapsed() > Duration::from_secs(15) {
            last_print = Instant::now();
            for t in &mine {
                eprintln!("{}", task_line(t));
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn report_tasks(a: &Args, ok: bool, tasks: Vec<Value>) -> i32 {
    let v = json!({"ok": ok, "tasks": tasks});
    emit(a, &v, || tasks.iter().map(task_line).collect::<Vec<_>>().join("\n"));
    if ok { 0 } else { 1 }
}

fn download(a: &Args) -> Result<i32> {
    let [cat, url] = a.pos.as_slice() else { bail!("用法：cumulus download <类别> <地址> [--name 文件名]，例如 cumulus download loras https://civitai.com/api/download/models/12345") };
    let v = call(json!({"cmd": "download", "category": cat, "url": url, "name": a.name.clone().unwrap_or_default()}))?;
    let id = v["id"].as_u64().unwrap_or(0);
    if a.no_wait {
        emit(a, &v, || format!("已开始下载，任务 #{id}"));
        return Ok(0);
    }
    let (ok, tasks) = wait_tasks(&[id])?;
    Ok(report_tasks(a, ok, tasks))
}

fn ssd(a: &Args) -> Result<i32> {
    let sub = a.pos.first().map(String::as_str).unwrap_or("list");
    let keys = &a.pos[1.min(a.pos.len())..];
    match sub {
        "add" | "rm" => {
            if keys.is_empty() {
                bail!("用法：cumulus ssd {sub} <类别/文件名>...");
            }
            let mut ids = Vec::new();
            for k in keys {
                let v = call(json!({"cmd": "ssd", "key": k, "on": sub == "add"}))?;
                if let Some(id) = v["id"].as_u64() {
                    ids.push(id);
                }
                if let Some(n) = v["note"].as_str().filter(|n| !n.is_empty()) {
                    eprintln!("{k}：{n}");
                }
            }
            if sub == "rm" || ids.is_empty() || a.no_wait {
                let v = json!({"ok": true, "tasks": ids});
                emit(a, &v, || if sub == "rm" { "已取消，SSD 上的副本会删掉".into() } else if ids.is_empty() { String::new() } else { format!("已开始复制：{ids:?}") });
                return Ok(0);
            }
            let (ok, tasks) = wait_tasks(&ids)?;
            Ok(report_tasks(a, ok, tasks))
        }
        "from" => ssd_from(a),
        "sync" => simple(a, json!({"cmd": "ssd_sync"}), "已把没复制的排进队列（cumulus tasks 查看）"),
        "list" => {
            let v = call(json!({"cmd": "ssd_profiles"}))?;
            emit(a, &v, || {
                let list: Vec<&str> = v["selected"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                if list.is_empty() {
                    return "还没有选模型放 SSD".into();
                }
                format!("{}\n共 {} 个，{}", list.join("\n"), list.len(), budget_text(&v["selected_budget"]))
            });
            Ok(0)
        }
        "profile" => ssd_profile(a),
        _ => bail!("用法：cumulus ssd add|rm|from|sync|list|profile"),
    }
}

/// 「12.3 GB，SSD 可放约 180 GB」
fn budget_text(b: &Value) -> String {
    let mut t = fmt_bytes(b["bytes"].as_u64().unwrap_or(0));
    if let Some(n) = b["unknown_sizes"].as_u64().filter(|n| *n > 0) {
        t.push_str(&format!("（另有 {n} 个不知道大小）"));
    }
    match b["room"].as_u64() {
        Some(r) => {
            let room = r.saturating_sub(crate::engine::SSD_RESERVE);
            t.push_str(&format!("，SSD 可放约 {}", fmt_bytes(room)));
            if b["fits"] == false {
                t.push_str("，放不下");
            }
        }
        None => t.push_str("，这种 GPU 的 SSD 容量还没量过（开一次机就知道）"),
    }
    t
}

fn accounts_text(v: &Value) -> String {
    let list = v["accounts"].as_array().cloned().unwrap_or_default();
    let mut out: Vec<String> = list
        .iter()
        .map(|x| {
            let bal = x["balance"].as_f64().map(|b| format!("{b:.1} CU")).unwrap_or("—".into());
            let ago = crate::accounts::Account { checked: x["checked"].as_u64().unwrap_or(0), ..Default::default() }.checked_ago();
            let note = format!("（{}）", x["error"].as_str().map(String::from).unwrap_or(ago));
            let email = x["email"].as_str().filter(|e| !e.is_empty()).map(|e| format!("  {e}")).unwrap_or_default();
            format!("{}{:<12} {:>10} {note}{email}", if x["active"] == true { "* " } else { "  " }, x["name"].as_str().unwrap_or(""), bal)
        })
        .collect();
    if list.len() > 1 {
        out.push(format!("  合计 {:.1} CU", v["total_balance"].as_f64().unwrap_or(0.0)));
    }
    out.join("\n")
}

fn account(a: &Args) -> Result<i32> {
    let sub = a.pos.first().map(String::as_str).unwrap_or("list");
    let arg = |i: usize, usage: &str| a.pos.get(i).cloned().ok_or_else(|| anyhow!("用法：cumulus account {usage}"));
    match sub {
        "list" => {
            if a.refresh && !a.json {
                eprintln!("正在查每个账号的余额…");
            }
            let v = call(json!({"cmd": "accounts", "refresh": a.refresh}))?;
            emit(a, &v, || accounts_text(&v));
            Ok(0)
        }
        "use" => {
            let k = arg(1, "use <名字>")?;
            let v = call(json!({"cmd": "account_use", "account": k}))?;
            emit(a, &v, || format!("开机改用账号「{k}」\n{}", accounts_text(&v)));
            Ok(0)
        }
        "rename" => {
            let (k, n) = (arg(1, "rename <名字> <新名字>")?, arg(2, "rename <名字> <新名字>")?);
            let v = call(json!({"cmd": "account_rename", "account": k, "name": n}))?;
            emit(a, &v, || format!("已改名为「{n}」"));
            Ok(0)
        }
        "rm" => {
            let k = arg(1, "rm <名字> --yes")?;
            if !a.yes {
                bail!("会删除账号「{k}」和它在钥匙串里的登录信息，确认的话加 --yes");
            }
            let v = call(json!({"cmd": "account_rm", "account": k}))?;
            emit(a, &v, || format!("已删除账号「{k}」"));
            Ok(0)
        }
        "add" | "login" => {
            let k = arg(1, &format!("{sub} <名字>"))?;
            let req = if sub == "add" { json!({"cmd": "account_add", "name": k}) } else { json!({"cmd": "account_login", "account": k}) };
            let v = call(req)?;
            let url = v["url"].as_str().unwrap_or("");
            if a.json {
                println!("{v}");
                return Ok(0);
            }
            eprintln!("在浏览器里打开下面的地址，选要{}的 Google 账号并点「允许」，页面上会显示授权码：\n\n  {url}\n", if sub == "add" { "添加" } else { "登录" });
            if !std::io::stdin().is_terminal() {
                eprintln!("拿到授权码后运行：cumulus account code <授权码>");
                return Ok(0);
            }
            eprint!("粘贴授权码后回车（直接回车取消）：");
            let mut code = String::new();
            std::io::stdin().read_line(&mut code)?;
            let code = code.trim();
            if code.is_empty() {
                call(json!({"cmd": "account_cancel"}))?;
                bail!("已取消");
            }
            let v = call(json!({"cmd": "account_code", "code": code}))?;
            println!("登录成功\n{}", accounts_text(&v));
            Ok(0)
        }
        "code" => {
            let c = arg(1, "code <授权码>")?;
            let v = call(json!({"cmd": "account_code", "code": c}))?;
            emit(a, &v, || format!("登录成功\n{}", accounts_text(&v)));
            Ok(0)
        }
        "cancel" => simple(a, json!({"cmd": "account_cancel"}), "已取消登录"),
        _ => bail!("用法：cumulus account [list|use|add|code|login|rename|rm|cancel]"),
    }
}

fn ssd_profile(a: &Args) -> Result<i32> {
    let sub = a.pos.get(1).map(String::as_str).unwrap_or("list");
    let pname = a.pos.get(2);
    let need_name = || pname.cloned().ok_or_else(|| anyhow!("用法：cumulus ssd profile {sub} <方案名>"));
    match sub {
        "list" => {
            let v = call(json!({"cmd": "ssd_profiles"}))?;
            emit(a, &v, || {
                let ps = v["profiles"].as_array().cloned().unwrap_or_default();
                if ps.is_empty() {
                    return "还没有方案。cumulus ssd profile save <名字> 把当前选择存成方案，或加 --from <工作流>".into();
                }
                ps.iter()
                    .map(|p| {
                        format!(
                            "{}{}  {} 个模型，{}",
                            if p["active"] == true { "* " } else { "  " },
                            p["name"].as_str().unwrap_or(""),
                            p["models"].as_array().map(|m| m.len()).unwrap_or(0),
                            budget_text(&p["budget"])
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            Ok(0)
        }
        "save" => {
            let n = need_name()?;
            let mut req = json!({"cmd": "ssd_profile_save", "profile": n});
            if let Some(src) = &a.from {
                if src == "-" {
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s)?;
                    req["text"] = json!(s);
                } else if Path::new(src).is_file() {
                    req["text"] = json!(std::fs::read_to_string(src)?);
                } else {
                    req["name"] = json!(src);
                }
            }
            let v = call(req)?;
            let p = v["profiles"].as_array().into_iter().flatten().find(|p| p["name"] == n.as_str()).cloned().unwrap_or_default();
            emit(a, &v, || {
                let models: Vec<&str> = p["models"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                format!("已存方案「{n}」：{} 个模型，{}\n{}", models.len(), budget_text(&p["budget"]), models.join("\n"))
            });
            Ok(0)
        }
        "use" => {
            let n = need_name()?;
            let v = call(json!({"cmd": "ssd_profile_use", "profile": n}))?;
            let ids: Vec<u64> = v["tasks"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
            if !a.json {
                eprintln!("{}", v["message"].as_str().unwrap_or(""));
            }
            if ids.is_empty() || a.no_wait {
                emit(a, &v, String::new);
                return Ok(0);
            }
            let (ok, tasks) = wait_tasks(&ids)?;
            Ok(report_tasks(a, ok, tasks))
        }
        "rm" => {
            let n = need_name()?;
            simple(a, json!({"cmd": "ssd_profile_rm", "profile": n}), &format!("已删除方案「{n}」（SSD 上的模型不动）"))
        }
        _ => bail!("用法：cumulus ssd profile list|save <名字> [--from <工作流>]|use <名字>|rm <名字>"),
    }
}

/// 跑工作流前把它用到的模型复制到 SSD（等复制完再提交，第一次加载就快）。
/// 放不下或出错只提示，照样从 Drive 读着跑
fn prewarm(a: &Args, wf: &Value) -> Result<()> {
    let s = call(json!({"cmd": "status"}))?;
    if a.no_ssd || s["drive"]["enabled"] != true {
        return Ok(());
    }
    let v = match call(json!({"cmd": "ssd_from_workflow", "text": wf.to_string(), "apply": true})) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("（没有预先复制到 SSD：{e}，这次从 Drive 读）");
            return Ok(());
        }
    };
    for m in v["missing"].as_array().into_iter().flatten() {
        eprintln!("注意：Drive 上没有 {}（{}），工作流可能会报错", m["name"].as_str().unwrap_or(""), m["node"].as_str().unwrap_or(""));
    }
    let ids: Vec<u64> = v["tasks"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
    if ids.is_empty() {
        return Ok(());
    }
    eprintln!("先把 {} 个模型复制到 SSD（{}）…（不想等加 --no-ssd）", ids.len(), fmt_bytes(v["copy_bytes"].as_u64().unwrap_or(0)));
    let (ok, _) = wait_tasks(&ids)?;
    if !ok {
        eprintln!("（有模型没复制成功，这次从 Drive 读）");
    }
    Ok(())
}

fn ssd_from(a: &Args) -> Result<i32> {
    let src = a.pos.get(1).ok_or_else(|| anyhow!("用法：cumulus ssd from <工作流.json | - | ComfyUI 里保存的工作流名字> [--dry-run]"))?;
    let mut req = json!({"cmd": "ssd_from_workflow", "apply": !a.dry_run});
    if src == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        req["text"] = json!(s);
    } else if Path::new(src).is_file() {
        req["text"] = json!(std::fs::read_to_string(src).map_err(|e| anyhow!("读不了 {src}：{e}"))?);
    } else {
        req["name"] = json!(src);
    }
    let v = call(req)?;
    let found = v["found"].as_array().cloned().unwrap_or_default();
    let missing = v["missing"].as_array().cloned().unwrap_or_default();
    // 先把分析结果写到 stderr（--json 时结果都在最后的 JSON 里）
    if !a.json {
        for f in &found {
            let st = if f["on_ssd"] == true { "已在 SSD" } else if a.dry_run { "会复制  " } else { "复制    " };
            eprintln!("  {st}  {}  {}", f["key"].as_str().unwrap_or(""), fmt_bytes(f["size"].as_u64().unwrap_or(0)));
        }
        for m in &missing {
            let cat = m["category"].as_str().unwrap_or("<类别>");
            let hint = m["url"].as_str().map(|u| format!("，可以 cumulus download {cat} {u}")).unwrap_or_default();
            eprintln!("  缺少      {}（{}）{hint}", m["name"].as_str().unwrap_or(""), m["node"].as_str().unwrap_or(""));
        }
        eprintln!("共 {} 个模型：{} 个要复制（{}），缺 {} 个", found.len() + missing.len(), v["copy_count"], fmt_bytes(v["copy_bytes"].as_u64().unwrap_or(0)), missing.len());
    }
    let ids: Vec<u64> = v["tasks"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
    if a.dry_run || a.no_wait || ids.is_empty() {
        emit(a, &v, String::new);
        return Ok(0);
    }
    let (ok, tasks) = wait_tasks(&ids)?;
    let mut out = v.clone();
    out["ok"] = json!(ok);
    out["tasks"] = json!(tasks);
    emit(a, &out, || if ok { "全部复制到 SSD 了".into() } else { tasks.iter().map(task_line).collect::<Vec<_>>().join("\n") });
    Ok(if ok { 0 } else { 1 })
}

// ------------------------------------------------------------------ 设置

fn config(a: &Args) -> Result<i32> {
    match a.pos.as_slice() {
        [] => {
            let v = call(json!({"cmd": "config_get"}))?;
            emit(a, &v, || config_text(&v));
        }
        [g, k] if g == "get" => {
            let v = call(json!({"cmd": "config_get"}))?;
            let val = v["config"].get(k.as_str()).cloned().ok_or_else(|| anyhow!("没有这个设置项：{k}"))?;
            emit(a, &json!({"ok": true, "key": k, "value": val}), || val.as_str().unwrap_or("").to_string());
        }
        [s, k, val] if s == "set" => {
            let v = call(json!({"cmd": "config_set", "key": k, "value": val}))?;
            emit(a, &v, || format!("已保存 {k}"));
        }
        _ => bail!("用法：cumulus config | config get <键> | config set <键> <值>"),
    }
    Ok(0)
}

fn config_text(v: &Value) -> String {
    let mut o = vec![format!("# {}", v["path"].as_str().unwrap_or(""))];
    if let Some(m) = v["config"].as_object() {
        for (k, val) in m {
            o.push(format!("{k}={}", val.as_str().unwrap_or("")));
        }
    }
    o.join("\n")
}

// ------------------------------------------------------------------ 在 VM 上执行

/// 复用掌管进程的 ssh 主连接执行命令（Colab 只允许一条连接，不能另开）
fn exec(cmd: &str) -> Result<i32> {
    if cmd.trim().is_empty() {
        bail!("用法：cumulus exec <命令>，例如 cumulus exec nvidia-smi");
    }
    let s = call(json!({"cmd": "wait_running"}))?;
    let session = s["session"].as_str().unwrap_or("").to_string();
    let ssh = remote::Ssh::new(&session)?;
    if !ssh.alive() {
        bail!("没有连接到 VM（{}），先运行 cumulus up", s["phase_label"].as_str().unwrap_or(""));
    }
    let st = ssh.command(cmd).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).status()?;
    Ok(st.code().unwrap_or(1))
}

// ------------------------------------------------------------------ 跑工作流

fn comfy_port() -> Result<u64> {
    let s = call(json!({"cmd": "wait_running"}))?;
    if s["comfy_up"] != true {
        bail!("ComfyUI 暂时连不上，稍后再试（cumulus logs --comfy 看看）");
    }
    s["local_port"].as_u64().ok_or_else(|| anyhow!("不知道本机端口"))
}

/// 用系统的 curl 发 HTTP 请求，返回 (状态码, 内容)
fn http(method: &str, url: &str, body: Option<&[u8]>) -> Result<(u16, String)> {
    let mut c = Command::new("/usr/bin/curl");
    c.args(["-s", "-m", "60", "-X", method, "-w", "\n%{http_code}"]);
    if body.is_some() {
        c.args(["-H", "Content-Type: application/json", "--data-binary", "@-"]);
    }
    let mut child = c.arg(url).stdin(if body.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    if let (Some(b), Some(mut i)) = (body, child.stdin.take()) {
        i.write_all(b)?;
    }
    let out = child.wait_with_output()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    let code: u16 = code.trim().parse().unwrap_or(0);
    if code == 0 {
        bail!("连不上 ComfyUI（{url}）");
    }
    Ok((code, body.to_string()))
}

/// 把读进来的工作流整理成 /prompt 要的 {"prompt": {...}}
fn prompt_body(wf: Value) -> Result<Value> {
    let wf = match wf {
        Value::Object(mut m) if m.contains_key("prompt") => m.remove("prompt").unwrap_or_default(),
        v => v,
    };
    let Value::Object(m) = &wf else { bail!("工作流应该是一个 JSON 对象") };
    if m.contains_key("nodes") && m.contains_key("links") {
        bail!("这是界面格式的工作流。请在 ComfyUI 里用「工作流 → 导出 (API)」得到 API 格式再提交");
    }
    if m.is_empty() || !m.values().all(|n| n.get("class_type").is_some()) {
        bail!("不像 API 格式的工作流：每个节点都应该有 class_type 和 inputs");
    }
    Ok(json!({"prompt": wf, "client_id": "cumulus-cli"}))
}

fn run_workflow(a: &Args) -> Result<i32> {
    let src = a.pos.first().ok_or_else(|| anyhow!("用法：cumulus run <workflow.json>（- 表示从 stdin 读）"))?;
    let text = if src == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        std::fs::read_to_string(Path::new(src)).map_err(|e| anyhow!("读不了 {src}：{e}"))?
    };
    let wf: Value = serde_json::from_str(&text).map_err(|e| anyhow!("{src} 不是合法的 JSON：{e}"))?;
    let body = prompt_body(wf)?;
    let port = comfy_port()?;
    prewarm(a, &body["prompt"])?;
    let base = format!("http://127.0.0.1:{port}");
    let (code, resp) = http("POST", &format!("{base}/prompt"), Some(body.to_string().as_bytes()))?;
    let r: Value = serde_json::from_str(&resp).unwrap_or(json!({"raw": resp}));
    if code != 200 {
        let msg = r["error"]["message"].as_str().unwrap_or("提交失败");
        let detail = r["node_errors"].as_object().filter(|m| !m.is_empty()).map(|m| format!("\n节点错误：{}", Value::Object(m.clone()))).unwrap_or_default();
        bail!("ComfyUI 拒绝了这个工作流（HTTP {code}）：{msg}{detail}");
    }
    let id = r["prompt_id"].as_str().ok_or_else(|| anyhow!("ComfyUI 没有返回 prompt_id：{resp}"))?.to_string();
    eprintln!("已提交，prompt_id {id}");
    if a.no_wait {
        emit(a, &json!({"ok": true, "prompt_id": id}), || id.clone());
        return Ok(0);
    }
    // 等它跑完
    let timeout = Duration::from_secs(a.timeout.unwrap_or(1800));
    let t = Instant::now();
    let mut last_extend = Instant::now();
    let entry = loop {
        std::thread::sleep(Duration::from_millis(1500));
        if let Ok((200, h)) = http("GET", &format!("{base}/history/{id}"), None) {
            let h: Value = serde_json::from_str(&h).unwrap_or_default();
            if let Some(e) = h.get(&id) {
                if e["status"]["completed"] == true || e["status"]["status_str"] == "error" {
                    break e.clone();
                }
            }
        }
        if t.elapsed() > timeout {
            bail!("等了 {} 还没跑完（prompt_id {id}）", fmt_duration(timeout.as_secs()));
        }
        // 跑很久的任务：顺便推迟闲置关机（ComfyUI 有任务时本来也算活动，这里保险）
        if last_extend.elapsed() > Duration::from_secs(120) {
            last_extend = Instant::now();
            let _ = call(json!({"cmd": "extend"}));
        }
    };
    if entry["status"]["status_str"] == "error" {
        let msgs = entry["status"]["messages"].as_array().into_iter().flatten().filter(|m| m[0] == "execution_error").map(|m| {
            format!("{}（节点 {} {}）", m[1]["exception_message"].as_str().unwrap_or("").trim(), m[1]["node_id"], m[1]["node_type"].as_str().unwrap_or(""))
        });
        bail!("工作流执行出错：{}", msgs.collect::<Vec<_>>().join("；"));
    }
    // 收集输出文件
    let mut files: Vec<(String, String)> = Vec::new();
    for node in entry["outputs"].as_object().into_iter().flat_map(|m| m.values()) {
        for list in node.as_object().into_iter().flat_map(|m| m.values()) {
            for f in list.as_array().into_iter().flatten() {
                if f["type"] == "output" {
                    if let Some(name) = f["filename"].as_str() {
                        files.push((f["subfolder"].as_str().unwrap_or("").to_string(), name.to_string()));
                    }
                }
            }
        }
    }
    // 等出图同步回本机（每几秒同步一次）
    let mut found: Vec<Value> = Vec::new();
    let t = Instant::now();
    if !files.is_empty() {
        eprintln!("跑完了，等 {} 个文件同步回本机…", files.len());
    }
    while !files.is_empty() && t.elapsed() < Duration::from_secs(90) {
        let v = call(json!({"cmd": "outputs", "n": 500}))?;
        let outs = v["outputs"].as_array().cloned().unwrap_or_default();
        found = files
            .iter()
            .map(|(sub, name)| {
                let suffix = if sub.is_empty() { format!("/{name}") } else { format!("/{sub}/{name}") };
                let local = outs.iter().find_map(|o| o["path"].as_str().filter(|p| p.ends_with(&suffix)).map(String::from));
                json!({"filename": name, "subfolder": sub, "local": local, "view_url": format!("{base}/view?filename={}&subfolder={}&type=output", urlenc(name), urlenc(sub))})
            })
            .collect();
        if found.iter().all(|f| f["local"].is_string()) {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let v = json!({"ok": true, "prompt_id": id, "seconds": t.elapsed().as_secs(), "outputs": found});
    emit(a, &v, || {
        if found.is_empty() {
            return "跑完了，没有输出文件".into();
        }
        found.iter().map(|f| f["local"].as_str().map(String::from).unwrap_or_else(|| format!("（还没同步回来）{}", f["view_url"].as_str().unwrap_or("")))).collect::<Vec<_>>().join("\n")
    });
    Ok(0)
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_formats() {
        let api = json!({"1": {"class_type": "EmptyImage", "inputs": {}}});
        assert_eq!(prompt_body(api.clone()).unwrap()["prompt"], api);
        assert_eq!(prompt_body(json!({"prompt": api.clone()})).unwrap()["prompt"], api);
        assert!(prompt_body(json!({"nodes": [], "links": []})).is_err());
        assert!(prompt_body(json!([1, 2])).is_err());
        assert_eq!(urlenc("a b/中"), "a%20b%2F%E4%B8%AD");
    }

    #[test]
    fn args() {
        let a = parse(&["x".into(), "--json".into(), "-n".into(), "5".into(), "--gpu".into(), "A100".into()]).unwrap();
        assert!(a.json && a.n == Some(5) && a.gpu.as_deref() == Some("A100") && a.pos == ["x"]);
        assert!(parse(&["--bogus".into()]).is_err());
    }
}
