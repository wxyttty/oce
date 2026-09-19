//! 外部模型故障冷却门（借鉴 BCE 的 embed/rerank cooldown）。
//!
//! 任一外部通路（embed / rerank / LLM）失败后短时间冷却：冷却期内检索直接
//! 跳过该通路、走既有降级语义（保序回退 / 启发式分类），避免一个死服务把
//! 之后每次检索都拖满超时（LLM 超时上限 120s，不冷却时最坏每次检索都等满）。
//!
//! 进程内状态即可：个人模式单进程，无跨实例共享需求。

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct CooldownGate {
    down_until: Mutex<Option<Instant>>,
}

impl CooldownGate {
    pub const fn new() -> Self {
        Self {
            down_until: Mutex::new(None),
        }
    }

    /// 冷却期内返回 true；调用方应跳过外部调用，直接走降级路径。
    /// 锁中毒视为「未冷却」（fail-open：宁可多试一次真服务，不静默丢通路）。
    pub fn is_down(&self) -> bool {
        self.down_until
            .lock()
            .map(|g| g.is_some_and(|until| Instant::now() < until))
            .unwrap_or(false)
    }

    /// 触发冷却。持锁窗口内无 await，不会跨异步点持有。
    pub fn trip(&self, duration: Duration) {
        if let Ok(mut guard) = self.down_until.lock() {
            *guard = Some(Instant::now() + duration);
        }
    }
}

impl Default for CooldownGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_gate_is_up() {
        let gate = CooldownGate::new();
        assert!(!gate.is_down());
    }

    #[test]
    fn trip_then_expire() {
        let gate = CooldownGate::new();
        gate.trip(Duration::from_millis(50));
        assert!(gate.is_down());
        std::thread::sleep(Duration::from_millis(80));
        assert!(!gate.is_down());
    }

    #[test]
    fn later_trip_extends_window() {
        let gate = CooldownGate::new();
        gate.trip(Duration::from_millis(30));
        gate.trip(Duration::from_millis(200));
        std::thread::sleep(Duration::from_millis(60));
        assert!(gate.is_down(), "第二次 trip 应覆盖第一次的窗口");
    }
}
