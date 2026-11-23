//! 调用官方 colab CLI 和系统自带的 ssh。
//!
//! 连接方式：`colab ssh --proxy-mode` 当 OpenSSH 的 ProxyCommand（走 WebSocket，不需要公网隧道），
//! 先建一条 ControlMaster 主连接并在上面做 `-L 本机端口:127.0.0.1:8188` 端口转发，
//! 之后所有命令都复用这条连接，不用每次重新握手。
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};

/// 找 colab 可执行文件。从 Finder 启动的 App 没有 shell 的 PATH，所以先看常见安装位置。
pub fn colab_bin() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let candidates = [
        home.join(".local/bin/colab"),
        PathBuf::from("/opt/homebrew/bin/colab"),
        PathBuf::from("/usr/local/bin/colab"),
    ];
    for c in candidates {
        if c.exists() {
            return c;
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            let c = d.join("colab");
            if c.exists() {
                return c;
            }
        }
    }
    PathBuf::from("colab")
}

pub fn colab_installed() -> bool {
    colab_bin().exists()
}

fn colab() -> Command {
    let mut c = Command::new(colab_bin());
    c.env("NO_COLOR", "1").env("COLUMNS", "200").env("TERM", "dumb");
    c
}

/// 去掉 rich 输出里的颜色控制符
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for n in it.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// 运行一个 colab 子命令，超时就杀掉；返回合并后的输出
pub fn colab_run(args: &[&str], timeout: Duration) -> Result<String> {
    let mut child = colab()
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("无法运行 {}（装了 google-colab-cli 吗？）", colab_bin().display()))?;
    let out = wait_output(&mut child, timeout)?;
    let text = strip_ansi(&out.1);
    if !out.0 {
        bail!("colab {} 失败：{}", args.first().unwrap_or(&""), last_lines(&text, 6));
    }
    Ok(text)
}

/// 等子进程结束（带超时），返回 (是否成功, stdout+stderr)
fn wait_output(child: &mut Child, timeout: Duration) -> Result<(bool, String)> {
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let t1 = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = so.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let t2 = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = se.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break Some(st);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut text = t1.join().unwrap_or_default();
    text.push_str(&t2.join().unwrap_or_default());
    match status {
        Some(st) => Ok((st.success(), text)),
        None => Err(anyhow!("超时（{} 秒）：{}", timeout.as_secs(), last_lines(&strip_ansi(&text), 3))),
    }
}

pub fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join(" / ")
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    pub hardware: String,
}

/// `[name] endpoint | Hardware: L4 | Shape: ... | Variant: GPU`
pub fn parse_sessions(text: &str) -> Vec<SessionInfo> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if !l.starts_with('[') || !l.contains(" | ") || l.starts_with("[colab]") {
                return None;
            }
            let name = l[1..l.find(']')?].to_string();
            let hardware = l
                .split(" | ")
                .find_map(|p| p.trim().strip_prefix("Hardware:"))
                .map(|h| h.trim().to_string())
                .unwrap_or_default();
            Some(SessionInfo { name, hardware })
        })
        .collect()
}

pub fn sessions() -> Result<Vec<SessionInfo>> {
    Ok(parse_sessions(&colab_run(&["sessions"], Duration::from_secs(60))?))
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Usage {
    pub balance: Option<f32>,
    pub rate: Option<f32>,
}

/// `Current balance: 886.92 compute units` / `Usage rate: 1.71/hr`
pub fn parse_usage(text: &str) -> Usage {
    let num = |l: &str| -> Option<f32> {
        let s = l.split_once(':')?.1.trim();
        let n: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
        n.parse().ok()
    };
    let mut u = Usage::default();
    for l in text.lines() {
        let low = l.to_ascii_lowercase();
        if low.contains("balance") {
            u.balance = num(l).or(u.balance);
        } else if low.contains("rate") {
            u.rate = num(l).or(u.rate);
        }
    }
    u
}

pub fn usage() -> Result<Usage> {
    Ok(parse_usage(&colab_run(&["usage"], Duration::from_secs(60))?))
}

pub fn new_session(name: &str, gpu: &str) -> Result<()> {
    let mut args = vec!["new", "-s", name];
    if gpu != "CPU" {
        args.extend(["--gpu", gpu]);
    }
    colab_run(&args, Duration::from_secs(600)).map(|_| ())
}

pub fn stop_session(name: &str) -> Result<()> {
    match colab_run(&["stop", "-s", name], Duration::from_secs(180)) {
        Ok(_) => Ok(()),
        Err(e) => {
            let m = e.to_string().to_lowercase();
            if m.contains("not found") || m.contains("no such") || m.contains("no active") {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// 在内核里执行一行代码，算作会话活动，防止 Colab 认为内核闲置
pub fn heartbeat(name: &str) -> Result<()> {
    let mut child = colab()
        .args(["exec", "-s", name, "--timeout", "30"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut i) = child.stdin.take() {
        let _ = i.write_all(b"import time; open('/content/.cumulus_heartbeat','w').write(str(time.time()))\n");
    }
    let (ok, text) = wait_output(&mut child, Duration::from_secs(60))?;
    if !ok {
        bail!("{}", last_lines(&strip_ansi(&text), 3));
    }
    Ok(())
}

/// `colab drivemount`：需要浏览器授权时 CLI 会打印授权地址并等回车。
/// 返回子进程，调用方读 stdout 找 URL / "Mounted at"，用户授权后往 stdin 写回车。
pub fn drivemount(name: &str) -> Result<Child> {
    Ok(colab()
        .args(["drivemount", "-s", name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?)
}

// ------------------------------------------------------------------ ssh

pub fn key_path() -> PathBuf {
    crate::config::dir().join("id_ed25519")
}

/// 专用的 ssh 密钥（没有就生成一个），colab ssh 会把公钥带给 VM
pub fn ensure_key() -> Result<PathBuf> {
    let k = key_path();
    if k.exists() && k.with_extension("pub").exists() {
        return Ok(k);
    }
    std::fs::create_dir_all(crate::config::dir())?;
    let _ = std::fs::remove_file(&k);
    let st = Command::new("/usr/bin/ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "cumulus-colab", "-f"])
        .arg(&k)
        .stdin(Stdio::null())
        .status()?;
    if !st.success() {
        bail!("生成 ssh 密钥失败");
    }
    Ok(k)
}

pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub struct Ssh {
    pub session: String,
    pub sock: PathBuf,
    key: PathBuf,
}

impl Ssh {
    pub fn new(session: &str) -> Result<Self> {
        let key = ensure_key()?;
        // unix socket 路径有长度限制，放 /tmp
        let sock = PathBuf::from(format!("/tmp/cumulus-{}.sock", session.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()));
        Ok(Ssh { session: session.into(), sock, key })
    }

    fn base(&self) -> Command {
        let mut c = Command::new("/usr/bin/ssh");
        let proxy = format!(
            "{} ssh --proxy-mode -s {} -i {}",
            sh_quote(&colab_bin().to_string_lossy()),
            sh_quote(&self.session),
            sh_quote(&self.key.to_string_lossy())
        );
        c.arg("-i")
            .arg(&self.key)
            .args(["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"])
            .args(["-o", "BatchMode=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=4"])
            .args(["-o", "IdentitiesOnly=yes"])
            .arg("-o")
            .arg(format!("ProxyCommand={proxy}"))
            .arg("-S")
            .arg(&self.sock)
            .env("NO_COLOR", "1");
        c
    }

    fn target(&self) -> String {
        format!("root@colab-{}", self.session)
    }

    /// 主连接是否还活着
    pub fn alive(&self) -> bool {
        self.sock.exists()
            && self
                .base()
                .args(["-O", "check"])
                .arg(self.target())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
    }

    /// 建立主连接并转发端口；返回主连接进程（结束它就断开）
    pub fn connect(&self, local_port: u16) -> Result<Child> {
        // Colab 每台 VM 只允许一条 colab ssh 连接（多了返回 HTTP 429）。
        // 之前异常退出（或后台守护进程）留下的主连接要先关掉
        self.cleanup();
        let mut child = self
            .base()
            .args(["-M", "-N", "-o", "ExitOnForwardFailure=yes", "-o", "ControlPersist=no"])
            .arg("-L")
            .arg(format!("127.0.0.1:{local_port}:127.0.0.1:8188"))
            .arg(self.target())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(60) {
            if let Some(st) = child.try_wait()? {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                let err = strip_ansi(&err);
                if err.contains("429") {
                    bail!("这台 VM 已经有别的 colab ssh 连接（Colab 只允许一条）。如果你在终端里开着 colab ssh，请先退出");
                }
                bail!("ssh 连接失败（{}）：{}", st, last_lines(&err, 4));
            }
            if self.alive() {
                return Ok(child);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        let _ = child.kill();
        bail!("ssh 连接超时")
    }

    /// 在 VM 上执行命令（bash -c），可以从 stdin 喂数据
    pub fn command(&self, cmd: &str) -> Command {
        let mut c = self.base();
        c.arg(self.target()).arg("--").arg(cmd);
        c
    }

    pub fn run(&self, cmd: &str, stdin: Option<&[u8]>, timeout: Duration) -> Result<String> {
        let mut child = self
            .command(cmd)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let (Some(data), Some(mut i)) = (stdin, child.stdin.take()) {
            let data = data.to_vec();
            std::thread::spawn(move || {
                let _ = i.write_all(&data);
            });
        }
        let (ok, text) = wait_output(&mut child, timeout)?;
        if !ok {
            bail!("{}", last_lines(&text, 4));
        }
        Ok(text)
    }

    /// 执行命令并逐行回调输出（stdout 和 stderr 合在一起），返回是否成功
    pub fn stream(&self, cmd: &str, stdin: Option<Vec<u8>>, mut on_line: impl FnMut(&str)) -> Result<bool> {
        let mut child = self
            .command(&format!("{cmd} 2>&1"))
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let (Some(data), Some(mut i)) = (stdin, child.stdin.take()) {
            std::thread::spawn(move || {
                let _ = i.write_all(&data);
            });
        }
        let out = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let mut r = BufReader::new(out);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let s = String::from_utf8_lossy(&buf);
                    // 进度条常用 \r 刷新同一行
                    for part in s.split('\r') {
                        let p = part.trim_end_matches('\n');
                        if !p.trim().is_empty() {
                            on_line(&strip_ansi(p));
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let st = child.wait()?;
        Ok(st.success())
    }

    /// 把 VM 上的文件下载到本机（先写临时文件，完成后改名）
    pub fn download(&self, remote: &str, local: &Path) -> Result<u64> {
        if let Some(d) = local.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = local.with_file_name(format!(".{}.part", local.file_name().unwrap_or_default().to_string_lossy()));
        let file = std::fs::File::create(&tmp)?;
        let st = self
            .command(&format!("cat {}", sh_quote(remote)))
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(Stdio::null())
            .status()?;
        if !st.success() {
            let _ = std::fs::remove_file(&tmp);
            bail!("下载 {remote} 失败");
        }
        let n = std::fs::metadata(&tmp)?.len();
        std::fs::rename(&tmp, local)?;
        Ok(n)
    }

    /// 关掉之前留下的主连接（包括 socket 文件已经丢失的孤儿进程）
    pub fn cleanup(&self) {
        if self.sock.exists() {
            self.disconnect();
        }
        if self.kill_stale() {
            std::thread::sleep(Duration::from_secs(2));
        }
        let _ = std::fs::remove_file(&self.sock);
    }

    /// 结束这个会话遗留的主连接进程（socket 文件丢了、连不上它时用），返回是否结束了进程
    fn kill_stale(&self) -> bool {
        // 第二个是改名前（Nimbus）的程序留下的
        let pats = [format!("{} -M -N", self.sock.display()), format!("/tmp/nimbus-{}.sock -M -N", self.session)];
        let pids: Vec<String> = pats
            .iter()
            .filter_map(|pat| Command::new("/usr/bin/pgrep").args(["-f", "--", pat]).output().ok())
            .flat_map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().map(str::to_string).collect::<Vec<_>>())
            .collect();
        for p in &pids {
            let _ = Command::new("/bin/kill").arg(p).status();
        }
        !pids.is_empty()
    }

    pub fn disconnect(&self) {
        let _ = self
            .base()
            .args(["-O", "exit"])
            .arg(self.target())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// 本机端口是否空闲
pub fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

pub fn fmt_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else if v >= 100.0 { format!("{v:.0} {}", U[i]) } else { format!("{v:.1} {}", U[i]) }
}

pub fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h} 小时 {m} 分") } else if m > 0 { format!("{m} 分 {s} 秒") } else { format!("{s} 秒") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_parse() {
        let t = "[cctest] m-s-usc1b2-iqu6eo1a7fmk | Hardware: CPU | Shape: Standard | Variant: DEFAULT\n[colab] something\n";
        assert_eq!(parse_sessions(t), vec![SessionInfo { name: "cctest".into(), hardware: "CPU".into() }]);
        assert!(parse_sessions("[colab] No active sessions found on server.").is_empty());
    }

    #[test]
    fn usage_parse() {
        let u = parse_usage("Current balance: 886.92 compute units\nUsage rate: 1.71/hr\nActive assignments: 1");
        assert_eq!(u, Usage { balance: Some(886.92), rate: Some(1.71) });
    }

    #[test]
    fn ansi() {
        assert_eq!(strip_ansi("\x1b[1;32mok\x1b[0m done"), "ok done");
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_duration(3725), "1 小时 2 分");
    }
}
