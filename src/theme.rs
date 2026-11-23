//! 配色主题。所有颜色都通过这里的函数读取当前主题。
//!
//! 新增主题：在 `THEMES` 里加一个 `Palette` 即可，菜单和切换按钮会自动出现。
//! 主题按 `key` 保存在界面状态里，下次启动恢复。
use std::cell::Cell;

use gpui::{Hsla, rgb};

pub struct Palette {
    /// 保存用的标识，不要改动已有主题的 key
    pub key: &'static str,
    /// 菜单里显示的名称
    pub name: &'static str,
    pub is_dark: bool,
    pub bg: u32,
    pub sidebar: u32,
    pub term_bg: u32,
    pub card: u32,
    pub border: u32,
    pub hover: u32,
    pub selected: u32,
    pub text: u32,
    pub muted: u32,
    pub faint: u32,
    pub accent: u32,
    pub ok: u32,
    pub warn: u32,
    pub bad: u32,
    pub track: u32,
    /// 对话框背后的遮罩不透明度
    pub backdrop: f32,
    /// 终端的 16 种基础色：黑 红 绿 黄 蓝 品红 青 白，然后是对应的亮色
    pub ansi: [u32; 16],
}

/// 深色：参考 Claude / Codex 桌面端的暖色深色主题
pub const DARK: Palette = Palette {
    key: "dark",
    name: "深色",
    is_dark: true,
    bg: 0x262624,
    sidebar: 0x1f1e1d,
    term_bg: 0x1a1918,
    card: 0x2c2b28,
    border: 0x3a3935,
    hover: 0x32312e,
    selected: 0x3b3a36,
    text: 0xececea,
    muted: 0xa3a29c,
    faint: 0x74736e,
    accent: 0xd97757,
    ok: 0x7cc48a,
    warn: 0xe3b55b,
    bad: 0xe5675f,
    track: 0x3a3935,
    backdrop: 0.45,
    ansi: [
        0x2e2d2a, 0xe5675f, 0x8cc47c, 0xe3b55b, 0x6fa3e0, 0xc792ea, 0x6fc2c0, 0xd6d4cf,
        0x6c6b66, 0xff8a80, 0xa8e090, 0xf2d07a, 0x8dbbf5, 0xdbaaf5, 0x8ee0dd, 0xf6f5f2,
    ],
};

/// 浅色：米白底色，和深色主题同一套暖色调
pub const LIGHT: Palette = Palette {
    key: "light",
    name: "浅色",
    is_dark: false,
    bg: 0xfaf9f5,
    sidebar: 0xf0eee6,
    term_bg: 0xfdfcf9,
    card: 0xffffff,
    border: 0xe2dfd5,
    hover: 0xebe8df,
    selected: 0xe3dfd3,
    text: 0x1f1e1d,
    muted: 0x5e5d59,
    faint: 0x8f8d86,
    accent: 0xc96442,
    ok: 0x3f9a55,
    warn: 0xb07d12,
    bad: 0xcf4339,
    track: 0xe6e3d9,
    backdrop: 0.22,
    // 浅色底上「白色」要用深灰才看得见
    ansi: [
        0x1f1e1d, 0xc8443b, 0x3d8b40, 0x9a7400, 0x2f6fb7, 0x9a4fb5, 0x2a8a88, 0x6c6b66,
        0x8f8d86, 0xe0574d, 0x4ea851, 0xb38a14, 0x3f84d0, 0xb164cc, 0x35a3a0, 0x3a3935,
    ],
};

/// 所有可选主题，顺序就是菜单和循环切换的顺序
pub const THEMES: &[&Palette] = &[&DARK, &LIGHT];

thread_local! {
    static CURRENT: Cell<usize> = const { Cell::new(0) };
}

pub fn current() -> &'static Palette {
    THEMES[CURRENT.with(|c| c.get()).min(THEMES.len() - 1)]
}

/// 按 key 切换主题；找不到时保持不变，返回是否切换成功
pub fn set(key: &str) -> bool {
    match THEMES.iter().position(|p| p.key == key) {
        Some(i) => {
            CURRENT.with(|c| c.set(i));
            true
        }
        None => false,
    }
}

/// 切到下一个主题（只有深浅两个时就是来回切换）
pub fn next() -> &'static Palette {
    let i = (CURRENT.with(|c| c.get()) + 1) % THEMES.len();
    CURRENT.with(|c| c.set(i));
    THEMES[i]
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn bg() -> Hsla { c(current().bg) }
pub fn sidebar() -> Hsla { c(current().sidebar) }
pub fn term_bg() -> Hsla { c(current().term_bg) }
pub fn card() -> Hsla { c(current().card) }
pub fn border() -> Hsla { c(current().border) }
pub fn hover() -> Hsla { c(current().hover) }
pub fn selected() -> Hsla { c(current().selected) }
pub fn text() -> Hsla { c(current().text) }
pub fn muted() -> Hsla { c(current().muted) }
pub fn faint() -> Hsla { c(current().faint) }
pub fn accent() -> Hsla { c(current().accent) }
pub fn ok() -> Hsla { c(current().ok) }
pub fn warn() -> Hsla { c(current().warn) }
pub fn bad() -> Hsla { c(current().bad) }
pub fn track() -> Hsla { c(current().track) }
/// 强调色按钮上的文字
pub fn on_accent() -> Hsla { c(0xffffff) }
/// 对话框遮罩
pub fn backdrop() -> Hsla { gpui::black().opacity(current().backdrop) }
pub fn ansi(i: usize) -> Hsla { c(current().ansi[i.min(15)]) }

/// 根据使用率选颜色
pub fn level(pct: f32) -> Hsla {
    if pct >= 90.0 { bad() } else if pct >= 75.0 { warn() } else { accent() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching() {
        assert_eq!(current().key, "dark");
        assert!(set("light"));
        assert!(!current().is_dark);
        assert!(!set("no-such-theme"));
        assert_eq!(current().key, "light");
        assert_eq!(next().key, "dark");
        // key 不能重复
        for (i, a) in THEMES.iter().enumerate() {
            assert!(THEMES[i + 1..].iter().all(|b| b.key != a.key));
        }
    }
}
