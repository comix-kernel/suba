//! 每条连接固定一个实例。需要嵌套时先锁请求队列，再锁应答槽；锁外执行服务代码。
use crate::{lock::Lock, runtime::Event};
use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
use service_api::{Error, Op, Response, Result, Session, Snapshot};
const MAX_QUEUE: usize = 2;
const MAX_BYTES: usize = service_api::MAX_DATA;
const MAX_CONNECTIONS: usize = 32;
const MAX_CALLS: usize = 16;

enum Outcome {
    Waiting,
    Ready(Result<Response>),
    Consumed,
}
struct Call {
    result: Lock<Outcome>,
    done: Event,
    unconfirmed: Error,
}
impl Call {
    fn complete(&self, result: Result<Response>) -> bool {
        let mut slot = self.result.lock();
        if matches!(*slot, Outcome::Waiting) {
            *slot = Outcome::Ready(result);
            drop(slot);
            self.done.notify();
            true
        } else {
            false
        }
    }
}
pub struct Ticket(Arc<Call>);
impl Ticket {
    pub fn wait(self) -> Result<Response> {
        loop {
            let stamp = self.0.done.stamp();
            {
                let mut state = self.0.result.lock();
                if matches!(*state, Outcome::Ready(_)) {
                    let Outcome::Ready(result) = core::mem::replace(&mut *state, Outcome::Consumed)
                    else {
                        unreachable!()
                    };
                    return result;
                }
            }
            self.0.done.wait(stamp);
        }
    }
}
struct Link {
    closing: bool,
    cleaning: bool,
    cleaned: bool,
    tag: u64,
    calls: BTreeMap<u64, Arc<Call>>,
}
struct Message {
    id: u64,
    tag: u64,
    op: Op,
    bytes: usize,
}
pub enum Command {
    Export,
    Import(Snapshot),
}
pub struct Control {
    pub cmd: Command,
    pub answer: Arc<Answer>,
}
pub struct Answer {
    value: Lock<Option<Result<Option<Snapshot>>>>,
    done: Event,
}
impl Answer {
    pub fn finish(&self, v: Result<Option<Snapshot>>) {
        *self.value.lock() = Some(v);
        self.done.notify();
    }
}
pub enum Item {
    Request(u64, Op, Reply),
    Disconnect(u64),
    Control(Control),
    Stop,
}
struct Inbox {
    next: u64,
    links: BTreeMap<u64, Link>,
    queue: VecDeque<Message>,
    bytes: usize,
    retiring: bool,
    control: Option<Control>,
}
pub struct Port {
    instance: u64,
    pub fids: core::sync::atomic::AtomicUsize,
    inbox: Lock<Inbox>,
    pub changed: Event,
    pub stopped: Event,
    finished: core::sync::atomic::AtomicBool,
}
/// 接收权只在创建队列时签发一次，由服务任务独占。
pub struct Receiver {
    port: Arc<Port>,
}
impl Receiver {
    pub fn next(&mut self) -> Option<Item> {
        self.port.take_next()
    }
    pub fn port(&self) -> &Port {
        &self.port
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        let mut q = self.port.inbox.lock();
        q.retiring = true;
        while let Some(message) = q.queue.pop_front() {
            let call = q
                .links
                .get_mut(&message.id)
                .unwrap()
                .calls
                .remove(&message.tag)
                .unwrap();
            call.complete(Err(Error::Closed));
        }
        q.bytes = 0;
        for link in q.links.values() {
            for call in link.calls.values() {
                call.complete(Err(call.unconfirmed));
            }
        }
        q.links.clear();
        if let Some(control) = q.control.take() {
            control.answer.finish(Err(Error::Closed));
        }
        drop(q);
        self.port.changed.notify();
        self.port.mark_stopped();
    }
}
pub struct Client {
    id: u64,
    port: Arc<Port>,
}
pub struct Reply {
    port: Arc<Port>,
    id: u64,
    tag: u64,
    active: bool,
}
impl Port {
    pub fn channel(instance: u64) -> (Arc<Self>, Receiver) {
        let port = Arc::new(Self {
            instance,
            fids: 0.into(),
            inbox: Lock::new(Inbox {
                next: 0,
                links: BTreeMap::new(),
                queue: VecDeque::new(),
                bytes: 0,
                retiring: false,
                control: None,
            }),
            changed: Event::new(),
            stopped: Event::new(),
            finished: false.into(),
        });
        (port.clone(), Receiver { port })
    }
    pub fn connect(self: &Arc<Self>) -> Result<Arc<Client>> {
        let mut q = self.inbox.lock();
        if q.retiring {
            return Err(Error::Closed);
        }
        if q.links.len() >= MAX_CONNECTIONS {
            return Err(Error::NoCapacity);
        }
        let id = q.next;
        q.next = id.checked_add(1).ok_or(Error::NoCapacity)?;
        q.links.insert(
            id,
            Link {
                closing: false,
                cleaning: false,
                cleaned: false,
                tag: 0,
                calls: BTreeMap::new(),
            },
        );
        Ok(Arc::new(Client {
            id,
            port: self.clone(),
        }))
    }
    pub fn counts(&self) -> (usize, usize, usize) {
        let q = self.inbox.lock();
        (
            q.links.len(),
            q.links.values().map(|l| l.calls.len()).sum(),
            self.fids.load(core::sync::atomic::Ordering::Relaxed),
        )
    }
    pub fn busy(&self) -> bool {
        !self.inbox.lock().links.is_empty()
    }
    pub fn retire(&self) {
        self.inbox.lock().retiring = true;
        self.changed.notify()
    }
    fn mark_stopped(&self) {
        self.finished
            .store(true, core::sync::atomic::Ordering::Release);
        self.stopped.notify()
    }
    pub fn wait_stopped(&self) {
        loop {
            let s = self.stopped.stamp();
            if self.finished.load(core::sync::atomic::Ordering::Acquire) {
                return;
            }
            self.stopped.wait(s)
        }
    }
    pub fn control(&self, cmd: Command) -> Result<Option<Snapshot>> {
        let a = Arc::new(Answer {
            value: Lock::new(None),
            done: Event::new(),
        });
        {
            let mut q = self.inbox.lock();
            if q.control.is_some() || q.retiring {
                return Err(Error::Busy);
            }
            q.control = Some(Control {
                cmd,
                answer: a.clone(),
            });
        }
        self.changed.notify();
        loop {
            let s = a.done.stamp();
            if let Some(v) = a.value.lock().take() {
                return v;
            }
            a.done.wait(s)
        }
    }
    fn take_next(self: &Arc<Self>) -> Option<Item> {
        let mut q = self.inbox.lock();
        if let Some(c) = q.control.take() {
            return Some(Item::Control(c));
        }
        if let Some((&id, l)) = q.links.iter_mut().find(|(_, l)| l.closing && !l.cleaning) {
            l.cleaning = true;
            return Some(Item::Disconnect(id));
        }
        if let Some(m) = q.queue.pop_front() {
            q.bytes -= m.bytes;
            crate::trace::record("deliver", [self.instance, m.id, m.tag]);
            drop(q);
            self.changed.notify();
            return Some(Item::Request(
                m.id,
                m.op,
                Reply {
                    port: self.clone(),
                    id: m.id,
                    tag: m.tag,
                    active: true,
                },
            ));
        }
        if q.retiring && q.links.is_empty() {
            Some(Item::Stop)
        } else {
            None
        }
    }
    pub fn disconnected(&self, id: u64) {
        let mut q = self.inbox.lock();
        let l = q.links.get_mut(&id).unwrap();
        l.cleaned = true;
        if l.calls.is_empty() {
            q.links.remove(&id);
        }
        drop(q);
        self.changed.notify()
    }
    fn finish(&self, id: u64, tag: u64, outcome: Option<Result<Response>>) -> Result<()> {
        let mut q = self.inbox.lock();
        let Some(link) = q.links.get_mut(&id) else {
            return Err(Error::Closed);
        };
        let call = link.calls.remove(&tag).unwrap();
        if link.cleaned && link.calls.is_empty() {
            q.links.remove(&id);
        }
        crate::trace::record("reply", [self.instance, id, tag]);
        let result = outcome.unwrap_or(Err(call.unconfirmed));
        let accepted = call.complete(result);
        drop(q);
        self.changed.notify();
        if accepted {
            Ok(())
        } else {
            Err(Error::Closed)
        }
    }
}
impl Client {
    pub fn submit(&self, op: Op) -> Result<Ticket> {
        let bytes = op.weight()?;
        let unconfirmed = match op {
            Op::Write { .. } | Op::Replace { .. } => Error::WriteOutcomeUnknown,
            Op::Create { .. } => Error::CreateOutcomeUnknown,
            _ => Error::Closed,
        };
        loop {
            let seen = self.port.changed.stamp();
            let mut q = self.port.inbox.lock();
            if q.links.get(&self.id).ok_or(Error::Closed)?.closing {
                return Err(Error::Closed);
            }
            if q.queue.len() < MAX_QUEUE && q.bytes + bytes <= MAX_BYTES {
                let l = q.links.get_mut(&self.id).unwrap();
                if l.calls.len() >= MAX_CALLS {
                    return Err(Error::NoCapacity);
                }
                let tag = l.tag;
                l.tag = tag.checked_add(1).ok_or(Error::NoCapacity)?;
                let call = Arc::new(Call {
                    result: Lock::new(Outcome::Waiting),
                    done: Event::new(),
                    unconfirmed,
                });
                l.calls.insert(tag, call.clone());
                q.queue.push_back(Message {
                    id: self.id,
                    tag,
                    op,
                    bytes,
                });
                q.bytes += bytes;
                crate::trace::record("queue", [self.port.instance, self.id, tag]);
                drop(q);
                self.port.changed.notify();
                return Ok(Ticket(call));
            }
            drop(q);
            self.port.changed.wait(seen)
        }
    }
    pub fn close(&self) {
        let mut q = self.port.inbox.lock();
        if q.links.get(&self.id).is_none_or(|l| l.closing) {
            return;
        }
        q.links.get_mut(&self.id).unwrap().closing = true;
        let mut index = 0;
        while index < q.queue.len() {
            if q.queue[index].id == self.id {
                let m = q.queue.remove(index).unwrap();
                q.bytes -= m.bytes;
                q.links
                    .get_mut(&self.id)
                    .unwrap()
                    .calls
                    .remove(&m.tag)
                    .unwrap()
                    .complete(Err(Error::Closed));
            } else {
                index += 1
            }
        }
        for c in q.links[&self.id].calls.values() {
            c.complete(Err(c.unconfirmed));
        }
        drop(q);
        self.port.changed.notify()
    }
}
impl Session for Client {
    fn disconnect(&self) {
        self.close();
        loop {
            let stamp = self.port.changed.stamp();
            if !self.port.inbox.lock().links.contains_key(&self.id) {
                return;
            }
            self.port.changed.wait(stamp);
        }
    }

    fn call(&self, op: Op) -> Result<Response> {
        self.submit(op)?.wait()
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.close()
    }
}
impl Reply {
    pub fn send(mut self, result: Result<Response>) -> Result<()> {
        self.active = false;
        self.port.finish(self.id, self.tag, Some(result))
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        if self.active {
            let _ = self.port.finish(self.id, self.tag, None);
        }
    }
}
