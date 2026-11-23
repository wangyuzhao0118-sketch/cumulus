//! 动画小工具：数值缓动（进度条、侧栏宽度）和统一的时长 / 曲线。
//!
//! `smooth(key, 目标值)` 在渲染时调用：目标变化时从当前显示的值平滑过渡过去。
//! 只要还有动画没结束，视图在 render 末尾调用 `frame_needed()`，为 true 就请求下一帧。
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 调试用：环境变量 `SSHDESK_SLOW_ANIM=10` 让所有动画慢 10 倍，方便截图检查中间帧
fn ms(n: u64) -> Duration {
    use std::sync::OnceLock;
    static SLOW: OnceLock<u64> = OnceLock::new();
    let k = *SLOW.get_or_init(|| std::env::var("SSHDESK_SLOW_ANIM").ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1));
    Duration::from_millis(n * k)
}

/// 面板展开 / 收起
pub fn panel() -> Duration {
    ms(220)
}
/// 内容淡入（切换页面、右侧栏模式）
pub fn fade() -> Duration {
    ms(180)
}
/// 数值变化（进度条）
pub fn value() -> Duration {
    ms(450)
}

/// 先快后慢
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// 慢进慢出
pub fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

#[derive(Clone, Copy, Debug)]
pub struct Tween {
    from: f32,
    to: f32,
    start: Instant,
    dur: Duration,
    ease: fn(f32) -> f32,
}

impl Tween {
    pub fn new(value: f32, dur: Duration) -> Self {
        Tween { from: value, to: value, start: Instant::now() - dur, dur, ease: ease_out }
    }

    pub fn with_ease(mut self, ease: fn(f32) -> f32) -> Self {
        self.ease = ease;
        self
    }

    pub fn progress(&self) -> f32 {
        if self.dur.is_zero() {
            return 1.0;
        }
        (self.start.elapsed().as_secs_f32() / self.dur.as_secs_f32()).min(1.0)
    }

    pub fn value(&self) -> f32 {
        self.from + (self.to - self.from) * (self.ease)(self.progress())
    }

    pub fn running(&self) -> bool {
        self.progress() < 1.0
    }

    /// 改变目标值，从当前显示的位置继续过渡（动画中途反向也不会跳）
    pub fn set(&mut self, to: f32) {
        if (to - self.to).abs() < f32::EPSILON {
            return;
        }
        self.from = self.value();
        self.to = to;
        self.start = Instant::now();
    }

    /// 直接跳到目标值，不做动画
    #[cfg(test)]
    pub fn jump(&mut self, to: f32) {
        self.from = to;
        self.to = to;
        self.start = Instant::now() - self.dur;
    }
}

thread_local! {
    static VALUES: RefCell<HashMap<String, Tween>> = RefCell::new(HashMap::new());
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// 标记还有动画在进行，需要再画一帧
pub fn keep_going() {
    ACTIVE.with(|a| a.set(true));
}

/// 本帧是否还有动画在进行（读取后清零）
pub fn frame_needed() -> bool {
    ACTIVE.with(|a| a.replace(false))
}

/// 按 key 平滑过渡到 target；第一次出现时从 0 开始（进度条从左边长出来）
pub fn smooth(key: &str, target: f32) -> f32 {
    VALUES.with(|v| {
        let mut v = v.borrow_mut();
        let t = v.entry(key.to_string()).or_insert_with(|| Tween::new(0.0, value()));
        t.set(target);
        if t.running() {
            keep_going();
        }
        t.value()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tween() {
        let mut t = Tween::new(10.0, Duration::from_millis(50));
        assert!(!t.running());
        assert_eq!(t.value(), 10.0);
        t.set(20.0);
        assert!(t.running());
        let mid = t.value();
        assert!((10.0..20.0).contains(&mid));
        std::thread::sleep(Duration::from_millis(60));
        assert!(!t.running());
        assert_eq!(t.value(), 20.0);
        t.jump(5.0);
        assert_eq!((t.value(), t.running()), (5.0, false));
    }

    #[test]
    fn easing() {
        for f in [ease_out, ease_in_out] {
            assert_eq!(f(0.0), 0.0);
            assert_eq!(f(1.0), 1.0);
            assert!(f(0.5) > 0.0 && f(0.5) < 1.0);
        }
        assert!(ease_out(0.3) > 0.3);
    }

    #[test]
    fn smoothing() {
        assert!(smooth("test-key", 50.0) < 5.0);
        assert!(frame_needed());
        assert!(!frame_needed());
    }
}
