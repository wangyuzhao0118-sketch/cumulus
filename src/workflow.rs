//! 从 ComfyUI 工作流里找出用到的模型，和 Drive 上的模型对上，用来一键复制到本地 SSD。
//!
//! 界面格式（nodes / widgets_values，包括子图 definitions.subgraphs）和 API 格式（class_type / inputs）都支持：
//! 整个 JSON 递归找「看起来像模型文件名」的字符串。官方模板的节点还带 properties.models
//! （文件名、目录、下载地址），缺模型时用它提示该放哪个类别、从哪下载。
use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};

use crate::engine::Model;

const EXTS: &[&str] = &[".safetensors", ".sft", ".ckpt", ".pt", ".pth", ".bin", ".gguf", ".onnx", ".pkl"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelRef {
    /// 工作流里写的名字（统一成 / 分隔），如 flux1-dev.safetensors、sub/x.safetensors
    pub name: String,
    /// 用到它的节点类型（如 CheckpointLoaderSimple）
    pub node: String,
    /// 模板里标注的类别目录（如 diffusion_models）
    pub dir: Option<String>,
    /// 模板里标注的下载地址
    pub url: Option<String>,
}

fn is_model_name(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    !s.contains("://") && !s.contains('\n') && s.len() < 300 && EXTS.iter().any(|e| l.ends_with(e))
}

fn norm(s: &str) -> String {
    s.trim().replace('\\', "/")
}

/// 工作流里引用的模型（按名字去重，保留第一次出现的节点类型；模板标注合并进来）
pub fn refs(wf: &Value) -> Vec<ModelRef> {
    let mut out: Vec<ModelRef> = Vec::new();
    let mut hints: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    walk(wf, "", &mut out, &mut hints);
    for r in &mut out {
        if let Some((dir, url)) = hints.get(&r.name) {
            r.dir = dir.clone();
            r.url = url.clone();
        }
    }
    out
}

fn walk(v: &Value, node: &str, out: &mut Vec<ModelRef>, hints: &mut BTreeMap<String, (Option<String>, Option<String>)>) {
    match v {
        Value::Object(m) => {
            // properties.models: [{name, url, directory}]
            if let Some(list) = m.get("models").and_then(Value::as_array) {
                for x in list {
                    if let Some(n) = x.get("name").and_then(Value::as_str).filter(|n| is_model_name(n)) {
                        let dir = x.get("directory").and_then(Value::as_str).map(String::from);
                        let url = x.get("url").and_then(Value::as_str).map(String::from);
                        hints.entry(norm(n)).or_insert((dir, url));
                    }
                }
            }
            let here = m.get("type").or_else(|| m.get("class_type")).and_then(Value::as_str).unwrap_or(node);
            for (k, x) in m {
                // 模板标注里的名字不算「用到」（节点可能已经换了别的模型），只当提示
                if k == "models" || k == "url" {
                    continue;
                }
                walk(x, here, out, hints);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| walk(x, node, out, hints)),
        Value::String(s) if is_model_name(s) => {
            let name = norm(s);
            if !out.iter().any(|r| r.name == name) {
                out.push(ModelRef { name, node: node.to_string(), dir: None, url: None });
            }
        }
        _ => {}
    }
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// 工作流用到、Drive 上也有的模型：(类别/文件名, 大小, 已在 SSD, 已选中)
    pub found: Vec<(String, u64, bool, bool)>,
    /// Drive 上找不到的
    pub missing: Vec<ModelRef>,
}

impl Plan {
    /// 还需要复制的（不在 SSD 上的）
    pub fn to_copy(&self) -> impl Iterator<Item = &(String, u64, bool, bool)> {
        self.found.iter().filter(|f| !f.2)
    }

    pub fn copy_bytes(&self) -> u64 {
        self.to_copy().map(|f| f.1).sum()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "found": self.found.iter().map(|(k, size, on_ssd, sel)| json!({"key": k, "size": size, "on_ssd": on_ssd, "selected": sel})).collect::<Vec<_>>(),
            "missing": self.missing.iter().map(|r| json!({"name": r.name, "node": r.node, "category": r.dir, "url": r.url})).collect::<Vec<_>>(),
            "copy_count": self.to_copy().count(),
            "copy_bytes": self.copy_bytes(),
        })
    }
}

/// 和 Drive 上的模型对名字。先按完整名字（含子目录）找，找不到再按文件名找；
/// 同名文件在几个类别里都有时，优先模板标注的类别，没有标注就都算上
pub fn plan(refs: &[ModelRef], models: &[Model], selected: &[String]) -> Plan {
    let mut p = Plan::default();
    let mut seen = HashSet::new();
    for r in refs {
        let base = r.name.rsplit('/').next().unwrap_or(&r.name);
        let files = models.iter().filter(|m| !m.name.is_empty());
        let mut hits: Vec<&Model> = files.clone().filter(|m| m.name == r.name).collect();
        if hits.is_empty() {
            hits = files.filter(|m| m.name.rsplit('/').next() == Some(base)).collect();
        }
        if hits.len() > 1 {
            if let Some(d) = &r.dir {
                let same: Vec<&Model> = hits.iter().copied().filter(|m| m.category == *d).collect();
                if !same.is_empty() {
                    hits = same;
                }
            }
        }
        if hits.is_empty() {
            p.missing.push(r.clone());
        }
        for m in hits {
            let k = m.key();
            if seen.insert(k.clone()) {
                let sel = selected.contains(&k);
                p.found.push((k, m.size, m.ssd, sel));
            }
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(cat: &str, name: &str, size: u64, ssd: bool) -> Model {
        Model { category: cat.into(), name: name.into(), size, ssd }
    }

    #[test]
    fn ui_format_with_subgraph_and_hints() {
        let wf = json!({
            "nodes": [
                {"type": "CheckpointLoaderSimple", "widgets_values": ["sdxl\\base.safetensors"]},
                {"type": "LoraLoader", "widgets_values": ["style.safetensors", 1.0, 1.0]},
                {"type": "SaveImage", "widgets_values": ["ComfyUI"]}
            ],
            "definitions": {"subgraphs": [{"nodes": [
                {"type": "UNETLoader", "widgets_values": ["flux1-dev.safetensors", "default"],
                 "properties": {"models": [{"name": "flux1-dev.safetensors", "url": "https://hf.co/x/flux1-dev.safetensors", "directory": "diffusion_models"}]}},
                {"type": "VAELoader", "widgets_values": ["ae.safetensors"],
                 "properties": {"models": [{"name": "ae.safetensors", "url": "https://hf.co/x/ae.safetensors", "directory": "vae"}]}}
            ]}]}
        });
        let r = refs(&wf);
        let mut names: Vec<&str> = r.iter().map(|x| x.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["ae.safetensors", "flux1-dev.safetensors", "sdxl/base.safetensors", "style.safetensors"]);
        let flux = r.iter().find(|x| x.name == "flux1-dev.safetensors").unwrap();
        assert_eq!(flux.node, "UNETLoader");
        assert_eq!(flux.dir.as_deref(), Some("diffusion_models"));
        assert!(flux.url.as_deref().unwrap().starts_with("https://"));

        let models = vec![
            m("checkpoints", "sdxl/base.safetensors", 7 << 30, false),
            m("diffusion_models", "flux1-dev.safetensors", 23 << 30, true),
            m("unet", "flux1-dev.safetensors", 23 << 30, false),
            m("loras", "old/style.safetensors", 100 << 20, false),
            m("vae", "", 0, false),
        ];
        let p = plan(&r, &models, &["diffusion_models/flux1-dev.safetensors".into()]);
        let mut keys: Vec<&str> = p.found.iter().map(|f| f.0.as_str()).collect();
        keys.sort();
        // 文件名对上子目录里的 lora；flux 在两个类别都有时取模板标注的 diffusion_models
        assert_eq!(keys, ["checkpoints/sdxl/base.safetensors", "diffusion_models/flux1-dev.safetensors", "loras/old/style.safetensors"]);
        assert_eq!(p.missing.len(), 1);
        assert_eq!(p.missing[0].name, "ae.safetensors");
        assert_eq!(p.to_copy().count(), 2);
        assert_eq!(p.copy_bytes(), (7 << 30) + (100 << 20));
        assert!(p.found.iter().find(|f| f.0.starts_with("diffusion")).unwrap().3);
    }

    #[test]
    fn api_format() {
        let wf = json!({
            "4": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": "v1-5.ckpt"}},
            "9": {"class_type": "SaveImage", "inputs": {"filename_prefix": "x", "images": ["8", 0]}},
            "10": {"class_type": "UpscaleModelLoader", "inputs": {"model_name": "4x.pth"}}
        });
        let r = refs(&wf);
        let mut got: Vec<(&str, &str)> = r.iter().map(|x| (x.name.as_str(), x.node.as_str())).collect();
        got.sort();
        assert_eq!(got, [("4x.pth", "UpscaleModelLoader"), ("v1-5.ckpt", "CheckpointLoaderSimple")]);
        assert!(!is_model_name("https://hf.co/a.safetensors"));
    }
}

#[cfg(test)]
mod samples {
    /// 用真实工作流检查解析：CUMULUS_WF_SAMPLES=<目录> cargo test -- --ignored samples
    #[test]
    #[ignore]
    fn print_samples() {
        let Some(dir) = std::env::var_os("CUMULUS_WF_SAMPLES") else { return };
        for ent in std::fs::read_dir(dir).unwrap().flatten() {
            let text = std::fs::read_to_string(ent.path()).unwrap();
            let wf: serde_json::Value = serde_json::from_str(&text).unwrap();
            println!("== {}", ent.file_name().to_string_lossy());
            for r in super::refs(&wf) {
                println!("   {:<55} {:<24} {:?}", r.name, r.node, r.dir);
            }
        }
    }
}
