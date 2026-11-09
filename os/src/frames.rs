//! RV64 virt 的启动页帧池。位图只登记早期页表可寻址、且由 DTB 声明可用的 RAM。
use crate::{lock::Lock, platform::PAGE};
const RAM: usize = 0x8000_0000;
const END: usize = RAM + (1 << 30);
const WORDS: usize = (END - RAM) / PAGE / usize::BITS as usize;
struct Pool {
    free: [usize; WORDS],
    cursor: usize,
    count: usize,
}
static POOL: Lock<Pool> = Lock::new(Pool {
    free: [0; WORDS],
    cursor: 0,
    count: 0,
});
impl Pool {
    fn mark(&mut self, base: usize, size: usize, available: bool) {
        if size == 0 {
            return;
        }
        let lo = base.clamp(RAM, END) - RAM;
        let hi = base.saturating_add(size).clamp(RAM, END) - RAM;
        let pages = if available {
            lo.div_ceil(PAGE)..hi / PAGE
        } else {
            lo / PAGE..hi.div_ceil(PAGE)
        };
        for page in pages {
            let word = &mut self.free[page / 64];
            let mask = 1usize << (page % 64);
            if (*word & mask != 0) != available {
                *word ^= mask;
                if available {
                    self.count += 1
                } else {
                    self.count -= 1
                }
            }
        }
    }
}
pub fn init(tree: &fdt::Fdt, dtb: usize, image_end: usize) {
    let mut pool = POOL.lock();
    for r in tree.memory().regions() {
        pool.mark(r.starting_address as usize, r.size.unwrap_or(0), true)
    }
    // QEMU virt 将 OpenSBI 放在内核物理装载地址之前。引导栈属于镜像保留区。
    pool.mark(RAM, image_end - RAM, false);
    pool.mark(dtb, tree.total_size(), false);
    for r in tree.memory_reservations() {
        pool.mark(r.address() as usize, r.size(), false)
    }
    if let Some(reserved) = tree.find_node("/reserved-memory") {
        for child in reserved.children() {
            for r in child.reg().into_iter().flatten() {
                pool.mark(r.starting_address as usize, r.size.unwrap_or(0), false)
            }
        }
    }
}
pub struct Frame(usize);
impl Frame {
    pub fn take() -> Option<Self> {
        let mut p = POOL.lock();
        if p.count == 0 {
            return None;
        }
        let i = (0..WORDS)
            .map(|n| (p.cursor + n) % WORDS)
            .find(|&i| p.free[i] != 0)?;
        let bit = p.free[i].trailing_zeros() as usize;
        p.free[i] &= !(1 << bit);
        p.count -= 1;
        p.cursor = i;
        Some(Self(RAM + (i * 64 + bit) * PAGE))
    }
    pub fn pin(self) -> usize {
        let pa = self.0;
        core::mem::forget(self);
        pa
    }
}
impl Drop for Frame {
    fn drop(&mut self) {
        let mut p = POOL.lock();
        p.mark(self.0, PAGE, true);
        p.cursor = (self.0 - RAM) / PAGE / 64;
    }
}
pub fn free() -> usize {
    POOL.lock().count
}
