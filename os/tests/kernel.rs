//! Hardware acceptance cases; compiled only into the selftest image.
use crate::{
    log, platform,
    registry::Registry,
    runtime::{self, Event},
    services,
    transport::{Item, Port},
};
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use service_api::*;

fn until(mut condition: impl FnMut() -> bool) {
    while !condition() {
        runtime::yield_now()
    }
}
fn tasks() {
    let base = runtime::free_slots();
    let done = Arc::new(AtomicBool::new(false));
    let counts = Arc::new([const { AtomicUsize::new(0) }; 3]);
    for (i, h) in [0, 0, 1].into_iter().enumerate() {
        let c = counts.clone();
        let d = done.clone();
        runtime::spawn(h, move || {
            let root = platform::root();
            while !d.load(SeqCst) {
                c[i].fetch_add(1, SeqCst);
            }
            assert_eq!(platform::root(), root);
        })
        .unwrap();
    }
    until(|| counts.iter().all(|n| n.load(SeqCst) > 0));
    done.store(true, SeqCst);
    until(|| runtime::free_slots() == base);
    for i in 0..100 {
        runtime::spawn(i % 2, || {}).unwrap();
        until(|| runtime::free_slots() == base)
    }
    let gate = Arc::new(Event::new());
    let seen = gate.stamp();
    for _ in 0..base {
        let g = gate.clone();
        runtime::spawn(1, move || g.wait(seen)).unwrap();
    }
    assert!(runtime::spawn(1, || {}).is_none());
    gate.notify();
    until(|| runtime::free_slots() == base);
}
fn transport() {
    let (p, mut receiver) = Port::channel(0);
    let c = p.connect().unwrap();
    let a = c.submit(Op::Attach(1)).unwrap();
    let b = c.submit(Op::Attach(2)).unwrap();
    let producer = c.clone();
    let finished = Arc::new(Event::new());
    let s = finished.stamp();
    let signal = finished.clone();
    let third = runtime::spawn(1, move || {
        assert_eq!(producer.call(Op::Clunk(3)), Ok(Response::Done));
        signal.notify()
    })
    .unwrap();
    until(|| runtime::blocked(third));
    let Some(Item::Request(_, _, ra)) = receiver.next() else {
        panic!()
    };
    let Some(Item::Request(_, _, rb)) = receiver.next() else {
        panic!()
    };
    rb.send(Ok(Response::Written(2))).unwrap();
    ra.send(Ok(Response::Written(1))).unwrap();
    assert_eq!(b.wait(), Ok(Response::Written(2)));
    assert_eq!(a.wait(), Ok(Response::Written(1)));
    loop {
        let seen = p.changed.stamp();
        if let Some(Item::Request(_, _, r)) = receiver.next() {
            r.send(Ok(Response::Done)).unwrap();
            break;
        }
        p.changed.wait(seen)
    }
    finished.wait(s);
    let w = || Op::Write {
        fid: 1,
        offset: 0,
        data: b"x".as_slice().into(),
    };
    let delivered = c.submit(w()).unwrap();
    let queued = c.submit(w()).unwrap();
    let Some(Item::Request(_, _, reply)) = receiver.next() else {
        panic!()
    };
    c.close();
    assert_eq!(queued.wait(), Err(Error::Closed));
    assert_eq!(delivered.wait(), Err(Error::WriteOutcomeUnknown));
    let Some(Item::Disconnect(id)) = receiver.next() else {
        panic!()
    };
    p.disconnected(id);
    assert!(p.busy()); // 当前连接已清理 fid，但设备工作仍持有旧实例。
    assert_eq!(reply.send(Ok(Response::Written(1))), Err(Error::Closed));
    assert!(!p.busy());
    let c = p.connect().unwrap();
    let started = c.submit(w()).unwrap();
    let Some(Item::Request(_, _, late)) = receiver.next() else {
        panic!()
    };
    let queued = c.submit(w()).unwrap();
    drop(receiver);
    assert_eq!(started.wait(), Err(Error::WriteOutcomeUnknown));
    assert_eq!(queued.wait(), Err(Error::Closed));
    assert_eq!(late.send(Ok(Response::Written(1))), Err(Error::Closed));
    // A notification between the condition check and park consumes the stamp.
    let event = Event::new();
    let before = event.stamp();
    event.notify();
    event.wait(before);
    // Force the precise interval after registration and before saving sp.
    let event = Arc::new(Event::new());
    let signal = event.clone();
    let phase = Arc::new(AtomicUsize::new(0));
    let peer = phase.clone();
    let before = event.stamp();
    runtime::spawn(1, move || {
        while peer.load(SeqCst) == 0 {
            runtime::yield_now()
        }
        signal.notify();
        peer.store(2, SeqCst);
    })
    .unwrap();
    event.park_with(before, || {
        phase.store(1, SeqCst);
        while phase.load(SeqCst) != 2 {
            core::hint::spin_loop()
        }
    });
    assert_eq!(phase.load(SeqCst), 2);
}
fn read(ns: &Namespace, a: &dyn Connect, p: &str) -> Vec<u8> {
    let mut f = ns.open(a, p, Mode::Read).unwrap();
    let v = f.read(MAX_DATA).unwrap();
    f.close().unwrap();
    v
}
pub fn run(stage: usize, uart: platform::Uart) -> ! {
    assert!((1..=6).contains(&stage));
    let pass = |n| {
        log!("PASS stage {n}");
        if stage == n {
            platform::poweroff(false)
        }
    };
    until(|| crate::BOOT_REPORTS.load(SeqCst) == 2 && runtime::free_slots() == runtime::SLOTS - 1);
    let free = crate::frames::free();
    {
        let _frame = crate::frames::Frame::take().unwrap();
        assert_eq!(crate::frames::free(), free - 1);
    }
    assert_eq!(crate::frames::free(), free);
    pass(1);
    tasks();
    pass(2);
    transport();
    pass(3);
    let (m, ns, access) = services::assemble(uart);
    let inner = ns.mount("/dev", Name::Record).unwrap();
    let mut f = inner.open(&access, "/dev/cons", Mode::Write).unwrap();
    assert_eq!(f.write(b"local"), Ok(5));
    f.close().unwrap();
    assert_eq!(read(&inner, &access, "/dev/cons"), b"local");
    let mut f = ns.open(&access, "/out/cons", Mode::Write).unwrap();
    let text = b"[one-uart-write]";
    let mut sent = 0;
    while sent < text.len() {
        sent += f.write(&text[sent..]).unwrap()
    }
    assert_eq!(read(&ns, &access, "/out/count"), b"16");
    f.close().unwrap();
    log!("");
    assert!(matches!(
        ns.open(&m.access(&[]), "/work/note", Mode::Read),
        Err(Error::Denied)
    ));
    // 驱动只在收到标记后发送字节，排除提前缓冲输入掩盖阻塞错误。
    let mut cons = ns.open(&access, "/dev/cons", Mode::Read).unwrap();
    let reader = runtime::spawn(1, move || {
        assert_eq!(cons.read(1).unwrap(), b"Q");
        cons.close().unwrap()
    })
    .unwrap();
    until(|| runtime::blocked(reader));
    let mut other = inner.open(&access, "/dev/note", Mode::Write).unwrap();
    other.write(b"while-blocked").unwrap();
    other.close().unwrap();
    assert_eq!(read(&inner, &access, "/dev/note"), b"while-blocked");
    log!("UART_READER_WAITING");
    until(|| runtime::gone(reader));
    let gate = Arc::new(Event::new());
    let hold = gate.clone();
    let stamp = gate.stamp();
    let tid = runtime::spawn(1, move || hold.wait(stamp)).unwrap();
    until(|| runtime::blocked(tid));
    let path = alloc::format!("/tasks/1/{}-{}/status", tid.slot, tid.generation);
    let mut opened = ns.open(&access, &path, Mode::Read).unwrap();
    let mut status = opened.read(5).unwrap();
    gate.notify();
    until(|| runtime::gone(tid));
    status.extend(opened.read(128).unwrap());
    opened.close().unwrap();
    assert!(core::str::from_utf8(&status).unwrap().contains("Blocked"));
    // A new task reuses the free slot with a fresh generation.
    let more = Arc::new(Event::new());
    let hold = more.clone();
    let stamp = more.stamp();
    let next = runtime::spawn(1, move || hold.wait(stamp)).unwrap();
    assert_eq!(tid.slot, next.slot);
    assert_ne!(tid.generation, next.generation);
    assert!(matches!(
        ns.open(&access, &path, Mode::Read),
        Err(Error::NotFound)
    ));
    more.notify();
    pass(4);
    Registry::exercise();
    pass(5);
    // 在另一核的目录查询和取得租约之间完成替换。
    let looked = Arc::new(Event::new());
    let observed = looked.stamp();
    let started = looked.clone();
    let resume = Arc::new(Event::new());
    let wait = resume.stamp();
    let permit = resume.clone();
    let reader = access.clone();
    let task = runtime::spawn(1, move || {
        let session = reader
            .connect_after_lookup(Name::Record, || {
                started.notify();
                permit.wait(wait)
            })
            .unwrap();
        session.call(Op::Attach(0)).unwrap();
        session
            .call(Op::Walk {
                from: 0,
                to: 1,
                path: "version".into(),
            })
            .unwrap();
        session.call(Op::Open(1, Mode::Read)).unwrap();
        assert_eq!(
            session.call(Op::Read {
                fid: 1,
                offset: 0,
                len: 2
            }),
            Ok(Response::Data(b"v2".to_vec()))
        );
        session.disconnect();
    })
    .unwrap();
    looked.wait(observed);
    let slot = m.registry.lookup(Name::Record);
    m.update(
        Name::Record,
        slot,
        Some((0, alloc::boxed::Box::new(components::Store::new(true)))),
        true,
    )
    .unwrap();
    resume.notify();
    until(|| runtime::gone(task));
    let mut old = ns.open(&access, "/work/version", Mode::Read).unwrap();
    assert_eq!(m.replace_store(true), Err(Error::Busy));
    m.replace_store(false).unwrap();
    assert_eq!(old.read(8).unwrap(), b"v1");
    old.close().unwrap();
    assert_eq!(read(&ns, &access, "/work/version"), b"v2");
    let mut f = ns.open(&access, "/work/note", Mode::Write).unwrap();
    f.write(b"preserved").unwrap();
    f.close().unwrap();
    // Same public file contract, incompatible snapshot decoder.
    struct RejectSnapshot(bool);
    impl FileTree for RejectSnapshot {
        fn start(&mut self) -> Result<()> {
            assert_eq!(platform::hart(), 1);
            if self.0 {
                Err(Error::Invalid)
            } else {
                Ok(())
            }
        }
        fn contract(&self) -> &'static str {
            "store/v1"
        }
        fn contains(&self, _: &str) -> bool {
            false
        }
        fn open(&mut self, _: &str, _: Mode) -> Result<Option<Vec<u8>>> {
            Err(Error::NotFound)
        }
        fn read(&mut self, _: &Opened, _: u64, _: usize) -> Result<Vec<u8>> {
            Err(Error::Denied)
        }
        fn write(&mut self, _: &Opened, _: u64, _: &[u8]) -> Result<usize> {
            Err(Error::Denied)
        }
        fn import(&mut self, _: Snapshot) -> Result<()> {
            Err(Error::Invalid)
        }
    }
    let slot = m.registry.lookup(Name::Store);
    let before = runtime::free_slots();
    assert_eq!(
        m.update(
            Name::Store,
            slot,
            Some((1, alloc::boxed::Box::new(RejectSnapshot(true)))),
            false
        ),
        Err(Error::Invalid)
    );
    until(|| runtime::free_slots() == before);
    let rejected = m.update(
        Name::Store,
        slot,
        Some((1, alloc::boxed::Box::new(RejectSnapshot(false)))),
        true,
    );
    assert_eq!(rejected, Err(Error::Invalid));
    assert_eq!(m.registry.lookup(Name::Store), slot);
    until(|| runtime::free_slots() == before);
    assert_eq!(read(&ns, &access, "/work/note"), b"preserved");
    m.replace_store(true).unwrap();
    assert_eq!(read(&ns, &access, "/work/note"), b"preserved");
    assert_eq!(read(&ns, &access, "/work/revision"), b"1");
    // 控制句柄绑定 open 时看到的代号，旧管理员不能覆盖已成功发布的新实例。
    let mut first = ns.open(&access, "/svc/record/ctl", Mode::Write).unwrap();
    let mut stale = ns.open(&access, "/svc/record/ctl", Mode::Write).unwrap();
    first.write(b"migrate v2").unwrap();
    assert_eq!(stale.write(b"withdraw"), Err(Error::Conflict));
    first.close().unwrap();
    stale.close().unwrap();
    let old_slot = m.registry.lookup(Name::Record);
    let mut orphan = inner.open(&access, "/dev/cons", Mode::Read).unwrap();
    m.update(Name::Record, old_slot, None, false).unwrap();
    assert_eq!(orphan.read(5).unwrap(), b"local");
    assert!(matches!(
        inner.open(&access, "/dev/cons", Mode::Read),
        Err(Error::NotFound)
    ));
    orphan.close().unwrap();
    assert_eq!(
        m.update(Name::Record, old_slot, None, false),
        Err(Error::Conflict)
    );
    pass(6);
    unreachable!()
}
