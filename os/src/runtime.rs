//! 固定核轮转调度。保存寄存器的内存地址在任务整个生存期内固定。
use crate::{lock::Lock, platform};
use alloc::boxed::Box;
use core::{
    arch::asm,
    cell::UnsafeCell,
    sync::atomic::{AtomicUsize, Ordering::*},
};
pub const SLOTS: usize = 24;
const _: () = assert!(SLOTS <= usize::BITS as usize);
pub const STRIDE: usize = 17 * 4096;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Id {
    pub slot: usize,
    pub generation: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Free,
    Ready,
    Running,
    Waiting(usize),
    Exited,
}
type Entry = Box<dyn FnOnce() + Send>;
struct Task {
    state: State,
    generation: usize,
    hart: usize,
    entry: Option<Entry>,
    wait_seq: usize,
}
impl Task {
    const fn empty() -> Self {
        Self {
            state: State::Free,
            generation: 0,
            hart: 0,
            entry: None,
            wait_seq: 0,
        }
    }
}
static TASKS: Lock<[Task; SLOTS]> = Lock::new([const { Task::empty() }; SLOTS]);
#[repr(C)]
struct Context {
    ra: usize,
    sp: usize,
    s: [usize; 12],
}
const _: () = {
    assert!(core::mem::offset_of!(Context, sp) == 8);
    assert!(core::mem::offset_of!(Context, s) == 16);
    assert!(core::mem::size_of::<Context>() == 112);
};
struct Cell(UnsafeCell<Context>);
// Only the assigned hart switches this context. A Free slot is initialized
// under TASKS before publication, and reused only after switching off its stack.
unsafe impl Sync for Cell {}
impl Cell {
    const fn new() -> Self {
        Self(UnsafeCell::new(Context {
            ra: 0,
            sp: 0,
            s: [0; 12],
        }))
    }
}
static TASK_CTX: [Cell; SLOTS] = [const { Cell::new() }; SLOTS];
#[derive(Clone, Copy)]
struct Wake {
    id: Id,
    seq: usize,
}
struct Inbox {
    records: [Option<Wake>; SLOTS],
    pending: usize,
}
impl Inbox {
    const fn new() -> Self {
        Self {
            records: [None; SLOTS],
            pending: 0,
        }
    }
    fn post(&mut self, slot: usize, task: &Task) {
        self.records[slot] = Some(Wake {
            id: Id {
                slot,
                generation: task.generation,
            },
            seq: task.wait_seq,
        });
        self.pending |= 1 << slot;
    }
}
/// 只有本核调度循环可以修改此 FIFO。每个活任务最多占一个位置。
struct Ready {
    slots: [usize; SLOTS],
    head: usize,
    len: usize,
}
impl Ready {
    fn push(&mut self, slot: usize) {
        assert!(self.len < SLOTS);
        self.slots[(self.head + self.len) % SLOTS] = slot;
        self.len += 1;
    }
    fn pop(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let slot = self.slots[self.head];
        self.head = (self.head + 1) % SLOTS;
        self.len -= 1;
        Some(slot)
    }
}
#[repr(C)]
struct HartLocal {
    id: usize,
    scheduler: Cell,
    current: AtomicUsize,
    #[cfg(feature = "trace")]
    generation: AtomicUsize,
    inbox: Lock<Inbox>,
}
impl HartLocal {
    const fn new(id: usize) -> Self {
        Self {
            id,
            scheduler: Cell::new(),
            current: AtomicUsize::new(SLOTS),
            #[cfg(feature = "trace")]
            generation: AtomicUsize::new(0),
            inbox: Lock::new(Inbox::new()),
        }
    }
}
static LOCAL: [HartLocal; 2] = [HartLocal::new(0), HartLocal::new(1)];
pub fn bind(h: usize) {
    // tp remains valid across every interrupt and switch on this fixed hart.
    unsafe {
        asm!("mv tp, {}",in(reg)&LOCAL[h]);
    }
}
pub static TICKS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
pub static CLOCK: Event = Event::new();
extern "C" {
    fn switch(old: *mut Context, new: *mut Context);
}
pub fn spawn(h: usize, entry: impl FnOnce() + Send + 'static) -> Option<Id> {
    let mut tasks = TASKS.lock();
    let (slot, t) = tasks
        .iter_mut()
        .enumerate()
        .find(|(_, t)| t.state == State::Free && t.generation < usize::MAX)?;
    assert!(h < 2);
    t.hart = h;
    t.generation += 1;
    t.entry = Some(Box::new(entry));
    let top = core::ptr::addr_of!(platform::task_stacks) as usize + (slot + 1) * STRIDE;
    unsafe {
        *TASK_CTX[slot].0.get() = Context {
            ra: first as usize,
            sp: top,
            s: [0; 12],
        };
    }
    t.state = State::Ready;
    let id = Id {
        slot,
        generation: t.generation,
    };
    LOCAL[h].inbox.lock().post(slot, t);
    drop(tasks);
    if platform::hart() != h {
        platform::ipi(h)
    }
    Some(id)
}
pub fn current() -> Id {
    let h = platform::hart();
    let slot = LOCAL[h].current.load(Relaxed);
    Id {
        slot,
        generation: TASKS.lock()[slot].generation,
    }
}
pub fn snapshot(h: usize, id: Id) -> Option<&'static str> {
    if platform::hart() != h {
        return None;
    }
    let tasks = TASKS.lock();
    let t = tasks.get(id.slot)?;
    if t.generation != id.generation || t.hart != h {
        return None;
    }
    match t.state {
        State::Free | State::Exited => None,
        State::Ready => Some("Ready"),
        State::Running => Some("Running"),
        State::Waiting(_) => Some("Blocked"),
    }
}
fn first() -> ! {
    let slot = LOCAL[platform::hart()].current.load(Relaxed);
    let job = TASKS.lock()[slot].entry.take().unwrap();
    platform::restore(2);
    job();
    platform::disable();
    TASKS.lock()[slot].state = State::Exited;
    switch_out();
    unreachable!()
}
fn switch_out() {
    let h = platform::hart();
    let slot = LOCAL[h].current.load(Relaxed);
    unsafe { switch(TASK_CTX[slot].0.get(), LOCAL[h].scheduler.0.get()) }
}
pub fn yield_now() {
    let irq = platform::disable();
    let slot = LOCAL[platform::hart()].current.load(Relaxed);
    if slot != SLOTS {
        {
            let mut tasks = TASKS.lock();
            let t = &mut tasks[slot];
            t.state = State::Ready;
            LOCAL[t.hart].inbox.lock().post(slot, t);
        }

        switch_out();
    }
    platform::restore(irq)
}
/// Sample before checking the protected condition. wait() checks the same stamp
/// under TASKS before parking; a notifier marks a slot only while it still waits
/// on this event. The sleeping stack keeps the event alive through switch_out.
pub struct Event {
    stamp: AtomicUsize,
    waiters: AtomicUsize,
}
impl Event {
    pub const fn new() -> Self {
        Self {
            stamp: AtomicUsize::new(0),
            waiters: AtomicUsize::new(0),
        }
    }
    pub fn stamp(&self) -> usize {
        self.stamp.load(Acquire)
    }
    pub fn wait(&self, seen: usize) {
        self.park(seen, None, || {});
    }
    #[cfg(feature = "selftest")]
    pub(crate) fn park_with(&self, seen: usize, registered: impl FnOnce()) {
        self.park(seen, None, registered)
    }
    pub fn wait_either(&self, seen: usize, other: &Event, other_seen: usize) {
        self.park(seen, Some((other, other_seen)), || {})
    }
    fn park(&self, seen: usize, other: Option<(&Event, usize)>, registered: impl FnOnce()) {
        let irq = platform::disable();
        let slot = LOCAL[platform::hart()].current.load(Relaxed);
        let mut t = TASKS.lock();
        if self.stamp() == seen && other.is_none_or(|(e, n)| e.stamp() == n) {
            t[slot].wait_seq = t[slot]
                .wait_seq
                .checked_add(1)
                .expect("wait sequence exhausted");
            if let Some((e, _)) = other {
                e.waiters.fetch_or(1 << slot, Relaxed);
            }
            self.waiters.fetch_or(1 << slot, Relaxed);
            t[slot].state = State::Waiting(self as *const _ as usize);
            let seq = t[slot].wait_seq;
            let generation = t[slot].generation;
            drop(t);
            crate::trace::record("park", [slot as u64, generation as u64, seq as u64]);
            registered();
            switch_out();
            // 另一事件可能仍有本等待的位；清除后才可开始下一次等待。
            let _tasks = TASKS.lock();
            self.waiters.fetch_and(!(1 << slot), Relaxed);
            if let Some((e, _)) = other {
                e.waiters.fetch_and(!(1 << slot), Relaxed);
            }
        } else {
            drop(t)
        }
        platform::restore(irq)
    }
    pub fn notify(&self) {
        let tasks = TASKS.lock();
        self.stamp.fetch_add(1, Release);
        let mut waiting = self.waiters.swap(0, Relaxed);
        let remote = platform::hart() ^ 1;
        let mut kick = false;
        while waiting != 0 {
            let slot = waiting.trailing_zeros() as usize;
            waiting &= waiting - 1;
            let task = &tasks[slot];
            if !matches!(task.state, State::Waiting(_)) {
                continue;
            }
            crate::trace::record(
                "wake",
                [slot as u64, task.generation as u64, task.wait_seq as u64],
            );
            LOCAL[task.hart].inbox.lock().post(slot, task);
            kick |= task.hart == remote;
        }
        drop(tasks);
        if kick {
            platform::ipi(remote)
        }
    }
}
pub fn schedule() -> ! {
    platform::disable();
    let h = platform::hart();
    let mut ready = Ready {
        slots: [0; SLOTS],
        head: 0,
        len: 0,
    };
    loop {
        let mut tasks = TASKS.lock();
        {
            let mut inbox = LOCAL[h].inbox.lock();
            while inbox.pending != 0 {
                let slot = inbox.pending.trailing_zeros() as usize;
                inbox.pending &= inbox.pending - 1;
                let wake = inbox.records[slot].take().unwrap();
                let t = &mut tasks[slot];
                if t.generation == wake.id.generation
                    && t.hart == h
                    && t.wait_seq == wake.seq
                    && matches!(t.state, State::Ready | State::Waiting(_))
                {
                    t.state = State::Ready;
                    ready.push(slot);
                }
            }
        }
        if let Some(i) = ready.pop() {
            tasks[i].state = State::Running;
            LOCAL[h].current.store(i, Relaxed);
            #[cfg(feature = "trace")]
            LOCAL[h].generation.store(tasks[i].generation, Relaxed);
            drop(tasks);
            unsafe { switch(LOCAL[h].scheduler.0.get(), TASK_CTX[i].0.get()) }
            LOCAL[h].current.store(SLOTS, Relaxed);

            let mut tasks = TASKS.lock();
            if tasks[i].state == State::Exited {
                tasks[i].state = State::Free
            }
        } else {
            drop(tasks);
            unsafe { asm!("wfi") }
        }
        platform::restore(2);
        platform::disable();
    }
}
#[no_mangle]
extern "C" fn trap() {
    #[cfg(feature = "selftest")]
    {
        let status: usize;
        unsafe { asm!("csrr {}, sstatus",out(reg)status) }
        assert_ne!(status & (1 << 8), 0);
    }
    let cause: usize;
    unsafe { asm!("csrr {}, scause",out(reg)cause) }
    let h = platform::hart();
    match cause ^ (1usize << 63) {
        5 => {
            platform::arm_timer();
            if h == 0 {
                platform::poll_uart()
            }
            TICKS[h].fetch_add(1, Relaxed);
            CLOCK.notify();
            yield_now();
        }
        1 => unsafe { asm!("csrci sip, 2") },
        _ => {
            let pc: usize;
            let addr: usize;
            unsafe { asm!("csrr {}, sepc","csrr {}, stval",out(reg)pc,out(reg)addr) }
            panic!("trap {cause:x} at {pc:x} addr {addr:x} hart {h}")
        }
    }
}
#[cfg(feature = "selftest")]
pub fn free_slots() -> usize {
    TASKS
        .lock()
        .iter()
        .filter(|t| t.state == State::Free)
        .count()
}
#[cfg(feature = "selftest")]
pub fn blocked(id: Id) -> bool {
    let t = TASKS.lock();
    t[id.slot].generation == id.generation && matches!(t[id.slot].state, State::Waiting(_))
}

pub fn gone(id: Id) -> bool {
    let t = TASKS.lock();
    t[id.slot].generation != id.generation || t[id.slot].state == State::Free
}

#[cfg(feature = "selftest")]
pub fn blocked_on(id: Id, event: &Event) -> bool {
    let tasks = TASKS.lock();
    let task = &tasks[id.slot];
    task.generation == id.generation && task.state == State::Waiting(event as *const _ as usize)
}

pub fn live_ids(h: usize) -> alloc::vec::Vec<Id> {
    assert_eq!(platform::hart(), h);
    TASKS
        .lock()
        .iter()
        .enumerate()
        .filter(|(_, t)| t.hart == h && !matches!(t.state, State::Free | State::Exited))
        .map(|(slot, t)| Id {
            slot,
            generation: t.generation,
        })
        .collect()
}

#[cfg(feature = "trace")]
pub fn trace_id() -> (usize, usize) {
    let local = &LOCAL[platform::hart()];
    (local.current.load(Relaxed), local.generation.load(Relaxed))
}
