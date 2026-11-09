//! 每核固定大小的事件历史。中断和持锁路径只复制值；格式化在锁外进行。
use alloc::string::String;
#[cfg(feature = "trace")]
mod enabled {
    use super::*;
    use crate::{lock::Lock, platform};
    use core::fmt::Write;
    #[derive(Clone, Copy)]
    struct Entry {
        time: usize,
        task: (usize, usize),
        kind: &'static str,
        args: [u64; 3],
    }
    const EMPTY: Entry = Entry {
        time: 0,
        task: (0, 0),
        kind: "",
        args: [0; 3],
    };
    const SIZE: usize = 48;
    #[derive(Clone, Copy)]
    struct Ring {
        items: [Entry; SIZE],
        next: usize,
        used: usize,
        lost: usize,
    }
    impl Ring {
        const fn new() -> Self {
            Self {
                items: [EMPTY; SIZE],
                next: 0,
                used: 0,
                lost: 0,
            }
        }
        fn entries(&self) -> impl Iterator<Item = &Entry> {
            (0..self.used).map(|n| &self.items[(self.next + SIZE - self.used + n) % SIZE])
        }
    }
    static RINGS: [Lock<Ring>; 2] = [const { Lock::new(Ring::new()) }; 2];
    pub fn record(kind: &'static str, args: [u64; 3]) {
        let mut r = RINGS[platform::hart()].lock();
        let i = r.next;
        r.items[i] = Entry {
            time: platform::now(),
            task: crate::runtime::trace_id(),
            kind,
            args,
        };
        r.next = (i + 1) % SIZE;
        if r.used == SIZE {
            r.lost = r.lost.saturating_add(1)
        } else {
            r.used += 1
        }
    }
    pub fn snapshot(h: usize) -> String {
        let r = *RINGS[h].lock();
        let mut s = String::new();
        writeln!(s, "hart={h} overwritten={}", r.lost).unwrap();
        for e in r.entries() {
            writeln!(
                s,
                "{} t{}-{} {} {} {} {}",
                e.time, e.task.0, e.task.1, e.kind, e.args[0], e.args[1], e.args[2]
            )
            .unwrap()
        }
        s
    }
    pub fn dump() {
        for (h, ring) in RINGS.iter().enumerate() {
            // 异常可能发生在记录途中。跳过该锁即可继续读取另一核。
            if let Some(guard) = ring.try_lock() {
                let r = *guard;
                drop(guard);
                for e in r.entries().skip(r.used.saturating_sub(8)) {
                    platform::panic_print(format_args!("h{h} {} {} {:?}\n", e.time, e.kind, e.args))
                }
            }
        }
    }
}
#[cfg(feature = "trace")]
pub use enabled::*;
#[cfg(not(feature = "trace"))]
pub fn record(_: &'static str, _: [u64; 3]) {}
#[cfg(not(feature = "trace"))]
pub fn snapshot(_: usize) -> String {
    "tracing disabled\n".into()
}
#[cfg(not(feature = "trace"))]
pub fn dump() {}
