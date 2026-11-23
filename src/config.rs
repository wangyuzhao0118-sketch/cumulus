//! 设置：存在 ~/Library/Application Support/cumulus/config，每行一个 key=value。
use std::path::PathBuf;

#[derive(Clone, PartialEq, Debug)]
pub struct Config {
    /// colab 会话名
    pub session: String,
    /// CPU / T4 / L4 / A100 / H100 / G4
    pub gpu: String,
    pub mount_drive: bool,
    /// Drive 上的 ComfyUI 资料目录（VM 上的路径）
    pub drive_root: String,
    pub output_to_drive: bool,
    /// 闲置多少分钟自动关机，0 = 不自动关
    pub idle_min: u32,
    /// 单次最长开机小时数，0 = 不限
    pub max_hours: f32,
    /// CU 余额低于这个值时关机
    pub min_cu: f32,
    pub local_port: u16,
    /// 出图同步到本机的目录
    pub output_local: String,
    pub extra_args: String,
    /// 自定义节点 git 地址，空格分隔
    pub custom_nodes: String,
    pub hf_token: String,
    pub civitai_token: String,
    pub theme: String,
    /// 要复制到 VM 本地 SSD 的模型，每项 "类别/文件名"
    pub ssd_models: Vec<String>,
    /// 开机后自动把 ssd_models 复制到 SSD
    pub ssd_auto: bool,
}

pub const GPUS: &[&str] = &["T4", "L4", "A100", "H100", "G4", "CPU"];

impl Default for Config {
    fn default() -> Self {
        let pics = dirs::picture_dir().or_else(|| dirs::home_dir().map(|h| h.join("Pictures"))).unwrap_or_default();
        Self {
            session: "cumulus".into(),
            gpu: "L4".into(),
            mount_drive: true,
            drive_root: "/content/drive/MyDrive/AI_Models/ComfyUI_L4".into(),
            output_to_drive: false,
            idle_min: 30,
            max_hours: 6.0,
            min_cu: 5.0,
            local_port: 8188,
            output_local: pics.join("Cumulus").to_string_lossy().into_owned(),
            extra_args: String::new(),
            custom_nodes: "https://github.com/Comfy-Org/ComfyUI-Manager https://github.com/city96/ComfyUI-GGUF".into(),
            hf_token: String::new(),
            civitai_token: String::new(),
            theme: "dark".into(),
            ssd_models: Vec::new(),
            ssd_auto: true,
        }
    }
}

pub fn dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    let d = base.join("cumulus");
    migrate(&base.join("nimbus"), &d);
    d
}

/// 项目改名前叫 Nimbus：把旧的设置目录（设置、ssh 密钥、日志）整个搬到新名字下。
/// 旧设置里没写会话名时，旧默认会话名是 nimbus，写进去，好让程序认出已经在跑的 VM
fn migrate(old: &std::path::Path, new: &std::path::Path) {
    if new.exists() || !old.is_dir() || std::fs::rename(old, new).is_err() {
        return;
    }
    let p = new.join("config");
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    if !text.lines().any(|l| l.starts_with("session=")) {
        let mut c = Config::parse(&text);
        c.session = "nimbus".into();
        let _ = std::fs::write(&p, c.serialize());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string(dir().join("config")).map(|s| Self::parse(&s)).unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(dir());
        let p = dir().join("config");
        let _ = std::fs::write(&p, self.serialize());
        // 里面有令牌，只给自己读
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }

    pub fn parse(s: &str) -> Self {
        let mut c = Self::default();
        for line in s.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim().to_string();
            let b = v == "1" || v == "true";
            match k.trim() {
                "session" if !v.is_empty() => c.session = v,
                "gpu" if GPUS.contains(&v.as_str()) => c.gpu = v,
                "mount_drive" => c.mount_drive = b,
                "drive_root" => c.drive_root = v,
                "output_to_drive" => c.output_to_drive = b,
                "idle_min" => c.idle_min = v.parse().unwrap_or(c.idle_min),
                "max_hours" => c.max_hours = v.parse().unwrap_or(c.max_hours),
                "min_cu" => c.min_cu = v.parse().unwrap_or(c.min_cu),
                "local_port" => c.local_port = v.parse().unwrap_or(c.local_port),
                "output_local" if !v.is_empty() => c.output_local = v,
                "extra_args" => c.extra_args = v,
                "custom_nodes" => c.custom_nodes = v,
                "hf_token" => c.hf_token = v,
                "civitai_token" => c.civitai_token = v,
                "theme" => c.theme = v,
                "ssd_models" => c.ssd_models = v.split('|').map(str::trim).filter(|x| x.contains('/')).map(String::from).collect(),
                "ssd_auto" => c.ssd_auto = b,
                _ => {}
            }
        }
        c
    }

    pub fn serialize(&self) -> String {
        let b = |x: bool| if x { "1" } else { "0" };
        format!(
            "session={}\ngpu={}\nmount_drive={}\ndrive_root={}\noutput_to_drive={}\nidle_min={}\nmax_hours={}\nmin_cu={}\nlocal_port={}\noutput_local={}\nextra_args={}\ncustom_nodes={}\nhf_token={}\ncivitai_token={}\ntheme={}\nssd_models={}\nssd_auto={}\n",
            self.session,
            self.gpu,
            b(self.mount_drive),
            self.drive_root,
            b(self.output_to_drive),
            self.idle_min,
            self.max_hours,
            self.min_cu,
            self.local_port,
            self.output_local,
            self.extra_args,
            self.custom_nodes,
            self.hf_token,
            self.civitai_token,
            self.theme,
            self.ssd_models.join("|"),
            b(self.ssd_auto),
        )
    }

    /// VM 上 ComfyUI 的出图目录
    pub fn remote_output(&self) -> String {
        if self.mount_drive && self.output_to_drive {
            format!("{}/output", self.drive_root.trim_end_matches('/'))
        } else {
            "/content/ComfyUI/output".into()
        }
    }

    /// 选中的模型能不能放 SSD（只有接了 Drive 才有意义）
    pub fn ssd_enabled(&self) -> bool {
        self.mount_drive
    }

    pub fn models_root(&self) -> String {
        if self.mount_drive {
            format!("{}/models", self.drive_root.trim_end_matches('/'))
        } else {
            "/content/ComfyUI/models".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_from_nimbus() {
        let base = std::env::temp_dir().join(format!("cumulus-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (old, new) = (base.join("nimbus"), base.join("cumulus"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("config"), "gpu=A100\n").unwrap();
        std::fs::write(old.join("id_ed25519"), "key").unwrap();
        migrate(&old, &new);
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(new.join("id_ed25519")).unwrap(), "key");
        let c = Config::parse(&std::fs::read_to_string(new.join("config")).unwrap());
        assert_eq!((c.session.as_str(), c.gpu.as_str()), ("nimbus", "A100"));
        // 已经有新目录时不动
        std::fs::create_dir_all(&old).unwrap();
        migrate(&old, &new);
        assert!(old.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn roundtrip() {
        let mut c = Config::default();
        c.gpu = "A100".into();
        c.idle_min = 12;
        c.max_hours = 2.5;
        c.output_to_drive = true;
        c.hf_token = "hf_x".into();
        c.ssd_models = vec!["checkpoints/a b.safetensors".into(), "loras/sub/x.safetensors".into()];
        c.ssd_auto = false;
        assert_eq!(Config::parse(&c.serialize()), c);
        assert_eq!(Config::parse("gpu=XX\nidle_min=abc\n"), Config::default());
        assert_eq!(c.remote_output(), "/content/drive/MyDrive/AI_Models/ComfyUI_L4/output");
    }
}
