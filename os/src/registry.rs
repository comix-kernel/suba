//! 服务名绑定的节点复制。副本只应用值类型变更，队列端点始终由唯一实例持有。
use crate::{
    lock::Lock,
    platform,
    runtime::{self, Event},
};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use service_api::{Name, Result, NAMES};

use directory::{apply, Directory, History};
pub use directory::{Change, Slot};
struct Receipt {
    value: Lock<Option<Result<Slot>>>,
    done: Event,
}
#[derive(Clone, Copy)]
struct Entry {
    change: Change,
    origin: usize,
}
const CAP: usize = 4;
struct Log {
    history: History<Entry, CAP>,
    pending: [VecDeque<(Change, Arc<Receipt>)>; 2],
}
pub struct Registry {
    log: Lock<Log>,
    replicas: [Lock<Directory>; 2],
    event: Event,
    #[cfg(feature = "selftest")]
    pub paused: [core::sync::atomic::AtomicBool; 2],
    #[cfg(feature = "selftest")]
    parked: [core::sync::atomic::AtomicBool; 2],
    #[cfg(feature = "selftest")]
    after_append: [core::sync::atomic::AtomicBool; 2],
}
impl Registry {
    pub fn new() -> Arc<Self> {
        let r = Arc::new(Self {
            log: Lock::new(Log {
                history: Default::default(),
                pending: Default::default(),
            }),
            replicas: core::array::from_fn(|_| Lock::new([Slot::default(); NAMES])),
            event: Event::new(),
            #[cfg(feature = "selftest")]
            paused: Default::default(),
            #[cfg(feature = "selftest")]
            parked: Default::default(),
            #[cfg(feature = "selftest")]
            after_append: Default::default(),
        });
        for h in 0..2 {
            let r = r.clone();
            runtime::spawn(h, move || r.worker(h)).expect("replica task");
        }
        r
    }
    fn enqueue(&self, change: Change) -> Arc<Receipt> {
        let h = platform::hart();
        let receipt = Arc::new(Receipt {
            value: Lock::new(None),
            done: Event::new(),
        });
        self.log.lock().pending[h].push_back((change, receipt.clone()));
        self.event.notify();
        receipt
    }
    pub fn submit(&self, c: Change) -> Result<Slot> {
        let receipt = self.enqueue(c);
        loop {
            let s = receipt.done.stamp();
            if let Some(v) = *receipt.value.lock() {
                return v;
            }
            receipt.done.wait(s)
        }
    }
    pub fn status(&self) -> alloc::string::String {
        let l = self.log.lock();
        alloc::format!(
            "tail={} applied0={} applied1={} pending0={} pending1={}\n",
            l.history.tail,
            l.history.applied(0),
            l.history.applied(1),
            l.pending[0].len(),
            l.pending[1].len()
        )
    }
    pub fn lookup(&self, name: Name) -> Slot {
        let h = platform::hart();
        let target = self.log.lock().history.tail;
        loop {
            let s = self.event.stamp();
            if self.log.lock().history.applied(h) >= target {
                return self.replicas[h].lock()[name as usize];
            }
            self.event.wait(s)
        }
    }
    fn worker(&self, h: usize) -> ! {
        loop {
            let stamp = self.event.stamp();
            #[cfg(feature = "selftest")]
            if self.paused[h].load(core::sync::atomic::Ordering::Acquire) {
                self.parked[h].store(true, core::sync::atomic::Ordering::Release);
                self.event.wait(stamp);
                self.parked[h].store(false, core::sync::atomic::Ordering::Release);
                continue;
            }
            let mut receipts = Vec::new();
            let (batch, end) = {
                let mut l = self.log.lock();
                while l.history.has_room() {
                    let Some((change, receipt)) = l.pending[h].pop_front() else {
                        break;
                    };
                    match l.history.push(Entry { change, origin: h }) {
                        Ok(()) => receipts.push(receipt),
                        Err(e) => {
                            *receipt.value.lock() = Some(Err(e));
                            receipt.done.notify();
                        }
                    }
                }
                let batch: Vec<_> = l.history.unread(h).copied().collect();
                (batch, l.history.tail)
            };
            if batch.is_empty() {
                self.event.wait(stamp);
                continue;
            }
            if !receipts.is_empty() {
                crate::trace::record("append", [h as u64, end as u64, receipts.len() as u64]);
                self.event.notify();
                #[cfg(feature = "selftest")]
                loop {
                    let s = self.event.stamp();
                    if !self.after_append[h].load(core::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    self.event.wait(s);
                }
            }
            let mut replies = Vec::new();
            {
                let mut d = self.replicas[h].lock();
                for entry in batch {
                    let result = apply(&mut d, entry.change);
                    if entry.origin == h {
                        replies.push(result);
                    }
                }
            }
            self.log.lock().history.acknowledge(h, end);
            crate::trace::record("applied", [h as u64, end as u64, replies.len() as u64]);
            assert_eq!(receipts.len(), replies.len());
            for (receipt, result) in receipts.into_iter().zip(replies) {
                *receipt.value.lock() = Some(result);
                receipt.done.notify();
            }
            self.event.notify()
        }
    }
}
#[cfg(feature = "selftest")]
#[path = "../tests/replication.rs"]
mod checks;
