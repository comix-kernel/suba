//! 服务发布、接入与回收。名称锁只保护接入门；持有逻辑变更权的任务在锁外等待快照和日志。
use crate::{
    lock::Lock,
    platform,
    registry::{Change, Registry, Slot},
    runtime::{self, Event},
    transport::{Command, Item, Port, Receiver},
};
use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use components::{Action, Console, Counter, Server, Store, Tasks};
use core::sync::atomic::{AtomicU64, Ordering::*};
use service_api::*;

struct Active {
    id: u64,
    port: Arc<Port>,

    task: runtime::Id,
}
struct Gate {
    live: Option<Active>,
    changing: bool,
    migrating: bool,
    draining: Vec<(u64, runtime::Id, alloc::sync::Weak<Port>)>,
}
pub struct Manager {
    pub registry: Arc<Registry>,
    gates: [Lock<Gate>; NAMES],
    gate_event: Event,
    ids: AtomicU64,
}
#[derive(Clone)]
pub struct Access {
    manager: Arc<Manager>,
    mask: usize,
}
impl Connect for Access {
    fn connect(&self, name: Name) -> Result<Arc<dyn Session>> {
        self.connect_after_lookup(name, || {})
    }
}
impl Access {
    pub(crate) fn connect_after_lookup(
        &self,
        name: Name,
        after: impl FnOnce(),
    ) -> Result<Arc<dyn Session>> {
        let mut hook = Some(after);
        if self.mask & (1 << name as usize) == 0 {
            return Err(Error::Denied);
        }
        loop {
            let binding = self.manager.registry.lookup(name);
            let id = binding.instance.ok_or(Error::NotFound)?;
            if let Some(callback) = hook.take() {
                callback()
            }
            let g = self.manager.gates[name as usize].lock();
            if g.migrating {
                return Err(Error::Busy);
            }
            if g.changing {
                let s = self.manager.gate_event.stamp();
                drop(g);
                self.manager.gate_event.wait(s);
                continue;
            }
            if let Some(a) = &g.live {
                if a.id == id {
                    return Ok(a.port.connect()?);
                }
            }
        }
    }
}
impl Manager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            registry: Registry::new(),
            gates: core::array::from_fn(|_| {
                Lock::new(Gate {
                    live: None,
                    changing: false,
                    migrating: false,
                    draining: Vec::new(),
                })
            }),
            gate_event: Event::new(),
            ids: AtomicU64::new(1),
        })
    }
    pub fn access(self: &Arc<Self>, names: &[Name]) -> Access {
        Access {
            manager: self.clone(),
            mask: names.iter().fold(0, |v, n| v | (1 << *n as usize)),
        }
    }
    fn candidate(&self, h: usize, tree: Box<dyn FileTree>) -> Result<Active> {
        let id = self
            .ids
            .fetch_update(Relaxed, Relaxed, |id| id.checked_add(1))
            .map_err(|_| Error::NoCapacity)?;

        let (port, service) = Port::channel(id);
        let ready = Arc::new((Lock::new(Ok(())), Event::new()));
        let signal = ready.clone();
        let stamp = ready.1.stamp();
        let task = runtime::spawn(h, move || {
            let mut server = Server::new(tree);
            let result = server.tree.start();
            *signal.0.lock() = result;
            signal.1.notify();
            if result.is_ok() {
                serve(server, service)
            }
        })
        .ok_or(Error::NoCapacity)?;
        ready.1.wait(stamp);
        (*ready.0.lock())?;
        Ok(Active { id, port, task })
    }
    fn status(&self, name: Name) -> String {
        use core::fmt::Write;
        let mut g = self.gates[name as usize].lock();
        g.draining.retain(|(_, task, _)| !runtime::gone(*task));
        let mut text = String::new();
        if let Some(a) = &g.live {
            let (links, calls, fids) = a.port.counts();
            writeln!(
                text,
                "active={} links={links} requests={calls} fids={fids}",
                a.id
            )
            .unwrap()
        } else {
            text.push_str("unpublished\n")
        }
        for (id, _, port) in &g.draining {
            let (links, calls, fids) = port.upgrade().map_or((0, 0, 0), |p| p.counts());
            writeln!(
                text,
                "draining={id} links={links} requests={calls} fids={fids}"
            )
            .unwrap();
        }
        text
    }
    pub fn publish(&self, name: Name, h: usize, tree: Box<dyn FileTree>) -> Result<()> {
        let expected = self.registry.lookup(name);
        if expected.instance.is_some() {
            return Err(Error::Conflict);
        }
        self.update(name, expected, Some((h, tree)), false)
            .map(|_| ())
    }
    /// Reserve a name before doing blocking work. The reservation protects the
    /// comparison and admission together; replica progress uses neither gate.
    pub fn update(
        &self,
        name: Name,
        expected: Slot,
        target: Option<(usize, Box<dyn FileTree>)>,
        migrate: bool,
    ) -> Result<u64> {
        let gate = &self.gates[name as usize];
        let old = {
            let mut g = gate.lock();
            if g.changing {
                return Err(Error::Busy);
            }
            if g.live.as_ref().map(|a| a.id) != expected.instance {
                return Err(Error::Conflict);
            }
            if let Some((_, tree)) = &target {
                if tree.contract() != name.contract() {
                    return Err(Error::Invalid);
                }
            }
            let old = g.live.as_ref().map(|a| a.port.clone());
            if migrate && (target.is_none() || old.as_ref().is_none_or(|p| p.busy())) {
                return Err(Error::Busy);
            }
            g.changing = true;
            g.migrating = migrate;
            old
        };
        let mut candidate = None;
        let result = (|| {
            if let Some((hart, tree)) = target {
                candidate = Some(self.candidate(hart, tree)?);
            }
            if migrate {
                let image = old
                    .as_ref()
                    .unwrap()
                    .control(Command::Export)?
                    .ok_or(Error::Invalid)?;
                candidate
                    .as_ref()
                    .unwrap()
                    .port
                    .control(Command::Import(image))?;
            }
            self.registry.submit(Change {
                name,
                expected,
                next: candidate.as_ref().map(|a| a.id),
            })
        })();
        {
            let mut g = gate.lock();
            if result.is_ok() {
                g.draining.retain(|(_, task, _)| !runtime::gone(*task));
                if let Some(a) = g.live.take() {
                    g.draining.push((a.id, a.task, Arc::downgrade(&a.port)))
                }
                g.live = candidate.take();
            }
            g.changing = false;
            g.migrating = false;
        }
        self.gate_event.notify();
        match result {
            Ok(binding) => {
                crate::trace::record(
                    "publish",
                    [
                        name as u64,
                        binding.instance.unwrap_or(0),
                        binding.generation,
                    ],
                );
                if let Some(p) = old {
                    p.retire();
                }
                Ok(binding.generation)
            }
            Err(e) => {
                if let Some(a) = candidate {
                    a.port.retire();
                    a.port.wait_stopped();
                }
                Err(e)
            }
        }
    }
    #[cfg(feature = "selftest")]
    pub fn replace_store(&self, migrate: bool) -> Result<u64> {
        let expected = self.registry.lookup(Name::Store);
        self.update(
            Name::Store,
            expected,
            Some((1, Box::new(Store::new(expected.generation % 2 == 1)))),
            migrate,
        )
    }
}
struct TaskInspector(usize);
impl Inspect for TaskInspector {
    fn list(&self) -> Vec<String> {
        runtime::live_ids(self.0)
            .iter()
            .map(|id| format!("{}-{}", id.slot, id.generation))
            .collect()
    }
    fn snapshot(&self, slot: usize, generation: usize) -> Result<String> {
        let state =
            runtime::snapshot(self.0, runtime::Id { slot, generation }).ok_or(Error::NotFound)?;
        Ok(format!("hart={} id={slot}-{generation} {state}\n", self.0))
    }
}
impl Serial for platform::Uart {
    fn read(&mut self) -> Option<u8> {
        self.read()
    }
    fn write(&mut self, data: &[u8]) -> usize {
        self.write(data)
    }
}
fn serve(mut server: Server, mut receiver: Receiver) {
    let mut pending: Vec<Work> = Vec::new();
    let mut last_tick = runtime::CLOCK.stamp();
    loop {
        let stamp = receiver.port().changed.stamp();
        let tick = runtime::CLOCK.stamp();
        let device_pending = server.tree.poll();
        if tick != last_tick {
            last_tick = tick;
            let mut i = 0;
            while i < pending.len() {
                let r = server.progress(pending[i].io());
                if r == Err(Error::WouldBlock) {
                    i += 1
                } else {
                    let _ = pending.remove(i).complete(r);
                }
            }
        }
        receiver.port().fids.store(server.fid_count(), Relaxed);
        match receiver.next() {
            Some(Item::Request(id, op, reply)) => match server.accept(id, op) {
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
                Ok(Action::Done(v)) => {
                    let _ = reply.send(Ok(v));
                }
                Ok(Action::Io(io)) => {
                    let work = Work::new(id, io, move |r| reply.send(r));
                    if let Some(work) = server.tree.defer(work) {
                        let result = server.progress(work.io());
                        if result == Err(Error::WouldBlock) {
                            pending.push(work)
                        } else {
                            let _ = work.complete(result);
                        }
                    }
                }
            },
            Some(Item::Disconnect(id)) => {
                server.tree.cancel(id);
                pending.retain(|w| w.connection != id);
                server.disconnect(id);
                receiver.port().disconnected(id);
            }
            Some(Item::Control(c)) => c.answer.finish(match c.cmd {
                Command::Export => server.tree.export().map(Some),
                Command::Import(s) => server.tree.import(s).map(|_| None),
            }),
            Some(Item::Stop) => {
                assert!(pending.is_empty());
                break;
            }
            None if pending.is_empty() && !device_pending => receiver.port().changed.wait(stamp),
            None => receiver
                .port()
                .changed
                .wait_either(stamp, &runtime::CLOCK, tick),
        }
    }
    drop(server);
    drop(receiver)
}
struct ControlFiles(Arc<Manager>, Option<Name>);

fn managed(s: &str) -> Option<Name> {
    match s {
        "store" => Some(Name::Store),
        "record" => Some(Name::Record),
        "counter" => Some(Name::Counter),
        _ => None,
    }
}
impl ControlFiles {
    fn update(&self, name: Name, expected: Slot, command: &str) -> Result<()> {
        let mut words = command.split_ascii_whitespace();
        let action = words.next().ok_or(Error::Invalid)?;
        let argument = words.next();
        if words.next().is_some() {
            return Err(Error::Invalid);
        }
        match (action, expected.instance) {
            ("publish", Some(_)) => return Err(Error::Conflict),
            ("migrate" | "replace" | "withdraw", None) => return Err(Error::NotFound),
            ("publish" | "migrate" | "replace" | "withdraw", _) => {}
            _ => return Err(Error::Invalid),
        }
        if action == "withdraw" {
            if argument.is_some() {
                return Err(Error::Invalid);
            }
            return self.0.update(name, expected, None, false).map(|_| ());
        }
        let tree: Box<dyn FileTree> = match name {
            Name::Store | Name::Record => {
                let indexed = match argument {
                    Some("v1") => false,
                    Some("v2") => true,
                    None => expected.generation % 2 == 1,
                    _ => return Err(Error::Invalid),
                };
                Box::new(Store::new(indexed))
            }
            Name::Counter => {
                // 只授予叶服务的可写文件，禁止同步转发回自己或管理服务。
                let backend = match argument {
                    Some("console") => Name::Console,
                    Some("record") => Name::Record,
                    Some("store") => Name::Store,
                    _ => return Err(Error::Invalid),
                };
                let ns = Namespace::default().mount("/", backend)?;
                let sink = ns.open(&self.0.access(&[backend]), "/cons", Mode::Write)?;
                Box::new(Counter::new(sink))
            }
            _ => return Err(Error::Denied),
        };
        let hart = if name == Name::Record { 0 } else { 1 };
        self.0
            .update(name, expected, Some((hart, tree)), action == "migrate")
            .map(|_| ())
    }
}
impl FileTree for ControlFiles {
    fn contract(&self) -> &'static str {
        "service-control/v1"
    }
    fn contains(&self, p: &str) -> bool {
        if self.1.is_some() {
            return matches!(p, "ctl" | "generation" | "status");
        }
        managed(p).is_some()
            || matches!(
                p,
                "memory" | "uart" | "replicas" | "trace" | "trace/0" | "trace/1"
            )
    }
    fn open(&mut self, p: &str, mode: Mode) -> Result<Option<Vec<u8>>> {
        if self.1.is_some() && p.is_empty() && mode == Mode::Read {
            return Ok(Some(b"ctl\ngeneration\nstatus\n".to_vec()));
        }
        if let Some((name, leaf)) = self.1.map(|name| (name, p)) {
            let slot = self.0.registry.lookup(name);
            let data = match (leaf, mode) {
                ("ctl", Mode::Write) => slot.generation.to_le_bytes().to_vec(),
                ("generation", Mode::Read) => format!("{}", slot.generation).into_bytes(),
                ("status", Mode::Read) => self.0.status(name).into_bytes(),
                _ => return Err(Error::Denied),
            };
            return Ok(Some(data));
        }
        if mode != Mode::Read {
            return Err(Error::Denied);
        }
        let text = match p {
            "" => "counter\nmemory\nrecord\nreplicas\nstore\ntrace\nuart\n".into(),
            "memory" => format!(
                "page_size={} free_frames={}\n",
                platform::PAGE,
                crate::frames::free()
            ),
            "uart" => format!("rx_dropped={}\n", platform::rx_dropped()),
            "replicas" => self.0.registry.status(),
            "trace" => "0\n1\n".into(),
            "trace/0" => crate::trace::snapshot(0),
            "trace/1" => crate::trace::snapshot(1),
            n if managed(n).is_some() => String::new(),
            _ => return Err(Error::NotFound),
        };
        Ok(Some(text.into_bytes()))
    }
    fn read(&mut self, _: &Opened, _: u64, _: usize) -> Result<Vec<u8>> {
        Err(Error::Denied)
    }
    fn write(&mut self, file: &Opened, offset: u64, data: &[u8]) -> Result<usize> {
        if offset != 0 {
            return Err(Error::Invalid);
        }
        let (name, "ctl") = self
            .1
            .map(|name| (name, file.path.as_str()))
            .ok_or(Error::Denied)?
        else {
            return Err(Error::Denied);
        };
        let opened = u64::from_le_bytes(
            file.snapshot
                .as_deref()
                .ok_or(Error::Invalid)?
                .try_into()
                .map_err(|_| Error::Invalid)?,
        );
        let expected = self.0.registry.lookup(name);
        if opened != expected.generation {
            return Err(Error::Conflict);
        }
        self.update(
            name,
            expected,
            core::str::from_utf8(data).map_err(|_| Error::Invalid)?,
        )?;
        Ok(data.len())
    }
}
pub struct ShellAdmin;
impl Admin for ShellAdmin {
    fn hart(&self) -> usize {
        platform::hart()
    }
    fn run_on(
        &self,
        hart: usize,
        job: Box<dyn FnOnce() -> Result<String> + Send>,
    ) -> Result<String> {
        if hart > 1 {
            return Err(Error::Invalid);
        }
        let answer = Arc::new((Lock::new(None), Event::new()));
        let child = answer.clone();
        runtime::spawn(hart, move || {
            let result = job();
            *child.0.lock() = Some(result);
            child.1.notify();
        })
        .ok_or(Error::NoCapacity)?;
        loop {
            let seen = answer.1.stamp();
            if let Some(result) = answer.0.lock().take() {
                return result;
            }
            answer.1.wait(seen)
        }
    }

    fn task_id(&self) -> String {
        let id = runtime::current();
        format!(
            "/tasks/{}/{}-{}/status",
            platform::hart(),
            id.slot,
            id.generation
        )
    }
    fn shutdown(&self) -> ! {
        platform::poweroff(false)
    }
}
pub fn assemble(uart: platform::Uart) -> (Arc<Manager>, Namespace, Access) {
    let m = Manager::new();
    m.publish(Name::Console, 0, Box::new(Console::new(Box::new(uart))))
        .unwrap();
    m.publish(Name::Store, 1, Box::new(Store::new(false)))
        .unwrap();
    m.publish(Name::Record, 0, Box::new(Store::new(false)))
        .unwrap();
    for (h, n) in [Name::Tasks0, Name::Tasks1].into_iter().enumerate() {
        m.publish(n, h, Box::new(Tasks(Box::new(TaskInspector(h)))))
            .unwrap();
    }
    m.publish(Name::Control, 0, Box::new(ControlFiles(m.clone(), None)))
        .unwrap();
    for (admin, name, hart) in [
        (Name::StoreAdmin, Name::Store, 0),
        (Name::RecordAdmin, Name::Record, 1),
        (Name::CounterAdmin, Name::Counter, 0),
    ] {
        m.publish(admin, hart, Box::new(ControlFiles(m.clone(), Some(name))))
            .unwrap();
    }
    let ns = Namespace::default()
        .mount("/svc", Name::Control)
        .unwrap()
        .mount("/svc/store", Name::StoreAdmin)
        .unwrap()
        .mount("/svc/record", Name::RecordAdmin)
        .unwrap()
        .mount("/svc/counter", Name::CounterAdmin)
        .unwrap()
        .mount("/dev", Name::Console)
        .unwrap()
        .mount("/work", Name::Store)
        .unwrap()
        .mount("/tasks/0", Name::Tasks0)
        .unwrap()
        .mount("/tasks/1", Name::Tasks1)
        .unwrap();
    let sink = ns
        .open(&m.access(&[Name::Console]), "/dev/cons", Mode::Write)
        .unwrap();
    m.publish(Name::Counter, 1, Box::new(Counter::new(sink)))
        .unwrap();
    let access = m.access(&[
        Name::StoreAdmin,
        Name::RecordAdmin,
        Name::CounterAdmin,
        Name::Control,
        Name::Console,
        Name::Store,
        Name::Record,
        Name::Tasks0,
        Name::Tasks1,
        Name::Counter,
    ]);
    (m, ns.mount("/out", Name::Counter).unwrap(), access)
}
