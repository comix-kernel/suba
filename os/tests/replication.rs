use super::*;
use core::sync::atomic::Ordering::SeqCst;
use service_api::Error;
impl Registry {
    // Exercise a private directory on the real scheduler. Its workers remain
    // live until this test image powers off.
    pub fn exercise() {
        let r = Self::new();
        let old = Slot::default();
        let change = |name, expected, next| Change {
            name,
            expected,
            next: Some(next),
        };
        // Queue a batch on this hart without allowing the local combiner to run.
        let irq = platform::disable();
        let a = r.enqueue(change(Name::Store, old, 1));
        let b = r.enqueue(change(Name::Record, old, 2));
        platform::restore(irq);
        let wait = |receipt: &Receipt| loop {
            let s = receipt.done.stamp();
            if let Some(v) = *receipt.value.lock() {
                break v;
            }
            receipt.done.wait(s)
        };
        let first = wait(&a).unwrap();
        assert_eq!(wait(&b).unwrap().instance, Some(2));
        let remote = platform::hart() ^ 1;
        let copy = r.clone();
        let checked = Arc::new(Event::new());
        let seen = checked.stamp();
        let done = checked.clone();
        runtime::spawn(remote, move || {
            assert_eq!(copy.lookup(Name::Store), first);
            done.notify()
        })
        .unwrap();
        checked.wait(seen);
        // Competing writers use the same expected slot on different harts.
        let other = r.clone();
        let result = Arc::new(Lock::new(None));
        let publish = result.clone();
        runtime::spawn(remote, move || {
            let value = other.submit(Change {
                name: Name::Store,
                expected: first,
                next: Some(4),
            });
            *publish.lock() = Some(value);
        })
        .unwrap();
        let local = r.submit(change(Name::Store, first, 3));
        while result.lock().is_none() {
            runtime::yield_now()
        }
        let peer = result.lock().unwrap();
        let mut slot = match (local, peer) {
            (Ok(slot), Err(Error::Conflict)) | (Err(Error::Conflict), Ok(slot)) => slot,
            x => panic!("both writers used the same revision: {x:?}"),
        };
        while {
            let l = r.log.lock();
            l.history.applied(remote) != l.history.tail
        } {
            runtime::yield_now()
        }
        r.paused[remote].store(true, SeqCst);
        r.event.notify();
        while !r.parked[remote].load(SeqCst) {
            runtime::yield_now()
        }
        let start = r.log.lock().history.tail;
        for id in 0..CAP {
            slot = r.submit(change(Name::Store, slot, id as u64)).unwrap()
        }
        let blocked = r.enqueue(change(Name::Store, slot, 20));
        for _ in 0..4 {
            runtime::yield_now()
        }
        {
            let l = r.log.lock();
            assert_eq!(l.history.tail, start + CAP);
            assert_eq!(l.history.applied(remote), start);
        }
        assert!(blocked.value.lock().is_none());
        r.paused[remote].store(false, SeqCst);
        r.event.notify();
        slot = wait(&blocked).unwrap();
        for id in 0..16 {
            slot = r.submit(change(Name::Store, slot, id)).unwrap()
        }
        while {
            let l = r.log.lock();
            l.history.applied(remote) != l.history.tail
        } {
            runtime::yield_now()
        }
        assert_eq!(*r.replicas[0].lock(), *r.replicas[1].lock());

        // 使用正式合并路径发布记录，只暂停从 append 到本核 apply 的一步。
        let origin = platform::hart();
        let prev = r.log.lock().history.tail;
        r.after_append[origin].store(true, SeqCst);
        let receipt = r.enqueue(change(Name::Store, slot, 99));
        while r.log.lock().history.tail == prev {
            runtime::yield_now()
        }
        let copy = r.clone();
        let seen = Arc::new(Event::new());
        let stamp = seen.stamp();
        let signal = seen.clone();
        let ack = receipt.clone();
        let caller = runtime::current();
        runtime::spawn(remote, move || {
            assert_eq!(copy.lookup(Name::Store).instance, Some(99));
            assert!(ack.value.lock().is_none());
            signal.notify();
            // Wait for the reader below to block on its stale local replica.
            while !runtime::blocked_on(caller, &copy.event) {
                runtime::yield_now()
            }
            copy.after_append[origin].store(false, SeqCst);
            copy.event.notify();
        })
        .unwrap();
        seen.wait(stamp);
        assert_eq!(r.lookup(Name::Store).instance, Some(99));
        assert_eq!(wait(&receipt).unwrap().instance, Some(99));
    }
}
