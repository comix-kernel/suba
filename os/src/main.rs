//! 教学镜像的启动组装。只有启动核初始化共享内存，两个 hart 分别建立本核陷入环境。
#![no_std]
#![no_main]
extern crate alloc;
use core::sync::atomic::{AtomicUsize, Ordering::*};
mod frames;
mod lock;
mod platform;
mod registry;
mod runtime;
mod services;
mod trace;
mod transport;
core::arch::global_asm!(
    include_str!("entry.S"),
    include_str!("switch.S"),
    include_str!("trap.S")
);
macro_rules! log {($($arg:tt)*)=>{crate::platform::print(format_args!("{}\n",format_args!($($arg)*)))}}
pub(crate) use log;
#[cfg(feature = "selftest")]
#[path = "../tests/kernel.rs"]
mod checks;
static BOOT_REPORTS: AtomicUsize = AtomicUsize::new(0);
static PREPARED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static ONLINE: AtomicUsize = AtomicUsize::new(0);
#[no_mangle]
extern "C" fn boot(h: usize, dtb: usize, secondary: usize) -> ! {
    assert!(h < 2);
    runtime::bind(h);
    if secondary == 0 {
        unsafe { platform::init(dtb) };
        PREPARED.store(true, Release);
    } else {
        while !PREPARED.load(Acquire) {
            core::hint::spin_loop()
        }
    }
    platform::activate();
    platform::init_hart();
    ONLINE.fetch_or(1 << h, Release);
    if secondary == 0 {
        platform::start(h ^ 1);
        let started = platform::now();
        while ONLINE.load(Acquire) != 3 {
            assert!(
                platform::now().wrapping_sub(started) < platform::boot_timeout(),
                "secondary boot timeout"
            );
            core::hint::spin_loop()
        }
        for id in 0..2 {
            runtime::spawn(id, move || {
                while runtime::TICKS[id].load(Acquire) == 0 {
                    core::hint::spin_loop()
                }
                log!(
                    "hart={} root={:x} ticks={}",
                    id,
                    platform::root(),
                    runtime::TICKS[id].load(Acquire)
                );
                BOOT_REPORTS.fetch_add(1, Release);
            })
            .unwrap();
        }
        runtime::spawn(0, move || {
            while BOOT_REPORTS.load(Acquire) != 2 {
                runtime::yield_now();
            }
            let uart = platform::Uart::take().unwrap();
            #[cfg(feature = "selftest")]
            checks::run(platform::STAGE.load(Acquire), uart);
            #[cfg(not(feature = "selftest"))]
            {
                let (_, ns, access) = services::assemble(uart);
                runtime::spawn(1, move || {
                    components::shell::run(ns, access, services::ShellAdmin).unwrap()
                })
                .unwrap();
            }
        })
        .unwrap();
    }
    runtime::schedule()
}
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    platform::disable();
    platform::panic_print(format_args!("PANIC {info}\n"));
    trace::dump();
    platform::poweroff(true)
}
