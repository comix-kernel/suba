//! QEMU virt 的可信硬件边界：OpenSBI、Sv39、堆与 16550 串口。最终页表在第二核上线前完成。
use crate::lock::Lock;
use core::{
    alloc::{GlobalAlloc, Layout},
    arch::asm,
    fmt::{self, Write},
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering::*},
};
use talc::{ErrOnOom, Span, Talc};

pub const BASE: usize = 0xffffffc000000000;
pub const PAGE: usize = 4096;
extern "C" {
    static sbss: u8;
    static ebss: u8;
    static stext: u8;
    static srodata: u8;
    static sdata: u8;
    static heap_start: u8;
    static kernel_end: u8;
    pub static task_stacks: u8;
    fn trap_entry();
    fn secondary_entry();
}
pub fn disable() -> usize {
    let old;
    unsafe { asm!("csrrci {}, sstatus, 2",out(reg)old) }
    old
}
pub fn restore(old: usize) {
    if old & 2 != 0 {
        unsafe { asm!("csrsi sstatus, 2") }
    }
}
pub fn hart() -> usize {
    let id;
    // SAFETY: runtime::bind 在引导时将 tp 指向永久的 HartLocal，首字段是 hart ID。
    unsafe { asm!("ld {}, 0(tp)",out(reg)id) }
    id
}
pub fn root() -> usize {
    let v: usize;
    unsafe { asm!("csrr {}, satp",out(reg)v) }
    v & ((1 << 44) - 1)
}
pub fn now() -> usize {
    let v;
    unsafe { asm!("rdtime {}",out(reg)v) }
    v
}
fn sbi(ext: usize, fid: usize, a: usize, b: usize, c: usize) -> isize {
    let res;
    unsafe {
        asm!("ecall",inlateout("a0")a=>res,inlateout("a1")b=>_,in("a2")c,in("a6")fid,in("a7")ext)
    }
    res
}
pub fn start(h: usize) {
    assert_eq!(sbi(0x48534d, 0, h, secondary_entry as usize - BASE, 0), 0)
}
pub fn ipi(h: usize) {
    sbi(0x735049, 0, 1 << h, 0, 0);
}
pub fn poweroff(fail: bool) -> ! {
    sbi(0x53525354, 0, 0, fail as usize, 0);
    loop {
        core::hint::spin_loop()
    }
}
#[cfg(feature = "selftest")]
pub static STAGE: AtomicUsize = AtomicUsize::new(6);
static INTERVAL: AtomicUsize = AtomicUsize::new(10000);
pub fn arm_timer() {
    sbi(0x54494d45, 0, now() + INTERVAL.load(Relaxed), 0, 0);
}
pub fn boot_timeout() -> usize {
    INTERVAL.load(Relaxed) * 400
}
pub fn init_hart() {
    unsafe {
        asm!("csrw stvec, {}",in(reg)trap_entry as usize);
        asm!("csrs sie, {}",in(reg)34usize);
    }
    arm_timer();
}
struct Early;
impl Write for Early {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            sbi(1, 0, b as usize, 0, 0);
        }
        Ok(())
    }
}
static PRINT: Lock<()> = Lock::new(());
pub fn print(args: fmt::Arguments) {
    let _g = PRINT.lock();
    let _ = Early.write_fmt(args);
}
pub fn panic_print(args: fmt::Arguments) {
    let _ = Early.write_fmt(args);
}

/// 堆的边界来自链接脚本，由启动核一次性交给 talc。
struct Heap(Lock<Talc<ErrOnOom>>);
#[global_allocator]
static HEAP: Heap = Heap(Lock::new(Talc::new(ErrOnOom)));
// SAFETY: talc 串行管理这块独占区域，释放使用分配调用提供的同一布局。
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        self.0
            .lock()
            .malloc(l)
            .map_or(core::ptr::null_mut(), |p| p.as_ptr())
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        self.0.lock().free(NonNull::new_unchecked(p), l)
    }
}
#[repr(align(4096))]
struct Table([usize; 512]);
static mut ROOT: Table = Table([0; 512]);
static UART: AtomicUsize = AtomicUsize::new(0);
/// 启动核独占建表过程；早期映射提供整个页帧池的物理别名。
unsafe fn map(va: usize, pa: usize, flags: usize) {
    let mut pt = core::ptr::addr_of_mut!(ROOT);
    for shift in [30, 21] {
        let pte = &mut (*pt).0[(va >> shift) & 511];
        if *pte == 0 {
            let child = crate::frames::Frame::take()
                .expect("page table frame")
                .pin();
            ((BASE + child) as *mut u8).write_bytes(0, PAGE);
            *pte = ((child >> 12) << 10) | 1;
        }
        pt = (((*pte >> 10) << 12) + BASE) as *mut Table;
    }
    (*pt).0[(va >> 12) & 511] = ((pa >> 12) << 10) | flags | 0xc1;
}
pub unsafe fn init(dtb: usize) {
    // SAFETY: 汇编从核入口跳过此函数。启动栈位于 BSS 结束符号之后。
    let bss = core::ptr::addr_of!(sbss) as *mut u8;
    bss.write_bytes(0, core::ptr::addr_of!(ebss) as usize - bss as usize);
    let tree = fdt::Fdt::from_ptr((BASE + dtb) as *const u8).unwrap();
    #[cfg(feature = "selftest")]
    STAGE.store(
        tree.chosen()
            .bootargs()
            .and_then(|s| s.strip_prefix("stage="))
            .unwrap_or("6")
            .parse()
            .unwrap(),
        Relaxed,
    );
    assert_eq!(tree.cpus().count(), 2, "requires two harts");
    assert!(
        tree.cpus().any(|cpu| cpu.ids().first() == 0)
            && tree.cpus().any(|cpu| cpu.ids().first() == 1)
    );
    let end = core::ptr::addr_of!(kernel_end) as usize;
    let text = core::ptr::addr_of!(stext) as usize;
    assert!(tree
        .memory()
        .regions()
        .any(|r| r.starting_address as usize <= text - BASE
            && r.starting_address as usize + r.size.unwrap_or(0) >= end - BASE));
    assert!(
        dtb >= core::ptr::addr_of!(task_stacks) as usize - BASE
            || dtb + tree.total_size() <= text - BASE
    );
    INTERVAL.store(
        tree.cpus().next().unwrap().timebase_frequency() / 200,
        Relaxed,
    );
    let uart = tree
        .find_compatible(&["ns16550a"])
        .unwrap()
        .reg()
        .unwrap()
        .next()
        .unwrap()
        .starting_address as usize;
    UART.store(BASE + uart, Relaxed);
    let stack = core::ptr::addr_of!(task_stacks) as usize;
    let heap = core::ptr::addr_of!(heap_start) as usize;
    assert_eq!(heap - stack, crate::runtime::SLOTS * crate::runtime::STRIDE);
    crate::frames::init(&tree, dtb, stack - BASE);
    for va in (text..end).step_by(PAGE) {
        if (stack..heap).contains(&va) && (va - stack) % crate::runtime::STRIDE == 0 {
            continue;
        }
        let perm = if va < core::ptr::addr_of!(srodata) as usize {
            10
        } else if va < core::ptr::addr_of!(sdata) as usize {
            2
        } else {
            6
        };
        let pa = if va < stack {
            va - BASE
        } else {
            crate::frames::Frame::take()
                .expect("stack/heap frame")
                .pin()
        };
        map(va, pa, perm);
    }
    map(BASE + uart, uart, 6);
    #[cfg(feature = "selftest")]
    {
        // 仍在早期物理别名中，逐级走最终页表，核对真正写入的 PTE。
        let flags = |va: usize| {
            let mut table = core::ptr::addr_of!(ROOT);
            for shift in [30, 21] {
                let entry = (*table).0[(va >> shift) & 511];
                assert_eq!(entry & 15, 1);
                table = (BASE + ((entry >> 10) << 12)) as *const Table;
            }
            (*table).0[(va >> 12) & 511] & 15
        };
        assert_eq!(flags(text), 11);
        assert_eq!(flags(core::ptr::addr_of!(srodata) as usize), 3);
        assert_eq!(flags(heap), 7);
        for guard in (stack..heap).step_by(crate::runtime::STRIDE) {
            assert_eq!(flags(guard), 0);
            assert_eq!(flags(guard + PAGE), 7)
        }
    }
    activate(); // 堆元数据必须写进最终页表选定的物理帧。
    HEAP.0
        .lock()
        .claim(Span::from_base_size(heap as *mut u8, end - heap))
        .unwrap();
}
pub fn activate() {
    let pa = core::ptr::addr_of!(ROOT) as usize - BASE;
    unsafe { asm!("csrw satp, {}","sfence.vma",in(reg)((8usize<<60)|(pa>>12))) }
}
struct Rx {
    bytes: [u8; 256],
    head: usize,
    len: usize,
    dropped: usize,
}
static RX: Lock<Rx> = Lock::new(Rx {
    bytes: [0; 256],
    head: 0,
    len: 0,
    dropped: 0,
});
pub fn rx_dropped() -> usize {
    RX.lock().dropped
}
/// 只有核 0 的时钟处理程序从 RBR 取数，每次最多处理硬件 FIFO 的 16 字节。
pub fn poll_uart() {
    let base = UART.load(Relaxed) as *mut u8;
    let mut rx = RX.lock();
    for _ in 0..16 {
        // SAFETY: 启动已将 DTB 描述的 UART 页映射为 MMIO。
        let byte = unsafe {
            if base.add(5).read_volatile() & 1 == 0 {
                break;
            }
            base.read_volatile()
        };
        if rx.len == rx.bytes.len() {
            rx.dropped = rx.dropped.saturating_add(1)
        } else {
            let tail = (rx.head + rx.len) % rx.bytes.len();
            rx.bytes[tail] = byte;
            rx.len += 1;
        }
    }
}
pub struct Uart(());
impl Uart {
    /// 每台机器只签发一份令牌，之后通过 move 授予串口任务。
    pub fn take() -> Option<Self> {
        static TAKEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        (!TAKEN.swap(true, AcqRel)).then_some(Self(()))
    }
    /// UART 页由设备树确定并以读写权限映射；唯一令牌串行化寄存器访问。
    pub fn read(&mut self) -> Option<u8> {
        let mut rx = RX.lock();
        if rx.len == 0 {
            return None;
        }
        let b = rx.bytes[rx.head];
        rx.head = (rx.head + 1) % rx.bytes.len();
        rx.len -= 1;
        Some(b)
    }
    pub fn write(&mut self, bytes: &[u8]) -> usize {
        let base = UART.load(Relaxed) as *mut u8;
        let mut n = 0;
        for &b in bytes {
            unsafe {
                if base.add(5).read_volatile() & 32 == 0 {
                    break;
                }
                base.write_volatile(b)
            }
            n += 1;
        }
        n
    }
}
