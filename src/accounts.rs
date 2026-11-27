//! 多个 Colab 账号：名字、邮箱、上次查到的 CU 余额。存在设置目录的 accounts.json（不含令牌，令牌在钥匙串里）。
use serde_json::{Value, json};

use crate::remote::DEFAULT_ACCOUNT;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Account {
    /// default = colab CLI 原来登录的账号；其余是 Cumulus 加的
    pub id: String,
    pub name: String,
    pub email: String,
    pub balance: Option<f32>,
    pub rate: Option<f32>,
    /// 上次查余额的时间（unix 秒）
    pub checked: u64,
    /// 上次查余额出的错（比如需要重新登录）
    pub error: Option<String>,
}

impl Account {
    pub fn label(&self) -> String {
        if self.email.is_empty() { self.name.clone() } else { format!("{}（{}）", self.name, self.email) }
    }

    /// 上次查余额是多久以前
    pub fn checked_ago(&self) -> String {
        if self.checked == 0 {
            return "还没查过".into();
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        match now.saturating_sub(self.checked) {
            d if d < 60 => "刚刚查的".into(),
            d if d < 3600 => format!("{} 分钟前", d / 60),
            d if d < 86400 => format!("{} 小时前", d / 3600),
            d => format!("{} 天前", d / 86400),
        }
    }

    fn to_json(&self) -> Value {
        // f32 转成 JSON 会带一长串尾数，按两位小数存
        let r = |x: Option<f32>| x.map(|v| (v as f64 * 100.0).round() / 100.0);
        json!({"id": self.id, "name": self.name, "email": self.email, "balance": r(self.balance), "rate": r(self.rate), "checked": self.checked, "error": self.error})
    }

    fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let f = |k: &str| v.get(k).and_then(Value::as_f64).map(|x| x as f32);
        let id = s("id");
        if !valid_id(&id) {
            return None;
        }
        Some(Account {
            name: Some(s("name")).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| id.clone()),
            id,
            email: s("email"),
            balance: f("balance"),
            rate: f("rate"),
            checked: v.get("checked").and_then(Value::as_u64).unwrap_or(0),
            error: v.get("error").and_then(Value::as_str).map(String::from),
        })
    }
}

/// id 用在文件夹名和钥匙串条目名里：只要字母数字
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

pub fn valid_name(n: &str) -> bool {
    let n = n.trim();
    !n.is_empty() && n.chars().count() <= 30 && !n.contains(['\n', '\r'])
}

fn path() -> std::path::PathBuf {
    crate::config::dir().join("accounts.json")
}

/// 读账号列表；default 总在第一个
pub fn load() -> Vec<Account> {
    let v: Value = std::fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    normalize(v.as_array().map(|a| a.iter().filter_map(Account::from_json).collect()).unwrap_or_default())
}

fn normalize(mut list: Vec<Account>) -> Vec<Account> {
    let mut seen = std::collections::HashSet::new();
    list.retain(|a| seen.insert(a.id.clone()));
    if !list.iter().any(|a| a.id == DEFAULT_ACCOUNT) {
        list.insert(0, Account { id: DEFAULT_ACCOUNT.into(), name: "主账号".into(), ..Default::default() });
    }
    list.sort_by_key(|a| a.id != DEFAULT_ACCOUNT);
    list
}

pub fn save(list: &[Account]) {
    let _ = std::fs::create_dir_all(crate::config::dir());
    let text = serde_json::to_string_pretty(&Value::Array(list.iter().map(Account::to_json).collect())).unwrap_or_default();
    let _ = std::fs::write(path(), text);
}

/// 新账号的 id：a + 当前时间（36 进制）
pub fn new_id(list: &[Account]) -> String {
    let mut n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(1);
    loop {
        let mut s = String::new();
        let mut x = n;
        while x > 0 {
            s.insert(0, std::char::from_digit((x % 36) as u32, 36).unwrap());
            x /= 36;
        }
        let id = format!("a{s}");
        if !list.iter().any(|a| a.id == id) {
            return id;
        }
        n += 1;
    }
}

/// 按 id、名字或邮箱找账号（命令行用）
pub fn find<'a>(list: &'a [Account], key: &str) -> Option<&'a Account> {
    let k = key.trim();
    list.iter()
        .find(|a| a.id == k)
        .or_else(|| list.iter().find(|a| a.name == k))
        .or_else(|| list.iter().find(|a| !a.email.is_empty() && a.email.eq_ignore_ascii_case(k)))
}

pub fn to_json(list: &[Account], active: &str) -> Value {
    Value::Array(
        list.iter()
            .map(|a| {
                let mut v = a.to_json();
                v["active"] = json!(a.id == active);
                v
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_default_first() {
        let list = vec![
            Account { id: "a1".into(), name: "副号".into(), email: "x@y".into(), balance: Some(12.5), checked: 9, ..Default::default() },
            Account { id: "default".into(), name: "我".into(), ..Default::default() },
            Account { id: "a1".into(), name: "重复".into(), ..Default::default() },
        ];
        let v = Value::Array(list.iter().map(Account::to_json).collect());
        let back = normalize(v.as_array().unwrap().iter().filter_map(Account::from_json).collect());
        assert_eq!(back.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["default", "a1"]);
        assert_eq!(back[1].balance, Some(12.5));
        assert_eq!(back[1].name, "副号");
        assert_eq!(normalize(vec![])[0].id, "default");
        assert!(Account::from_json(&json!({"id": "../x"})).is_none());
        assert_eq!(find(&back, "X@Y").unwrap().id, "a1");
        assert_eq!(find(&back, "我").unwrap().id, "default");
        let id = new_id(&back);
        assert!(valid_id(&id) && id.starts_with('a'));
    }
}
