//! 供组件使用的安全接口：私有挂载视图、文件协议和访问能力。
#![no_std]
#![forbid(unsafe_code)]
extern crate alloc;
use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

pub const MAX_DATA: usize = 4096;
pub const MAX_PATH: usize = 256;
pub type Fid = u32;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NotFound,
    Denied,
    Busy,
    Closed,
    WriteOutcomeUnknown,
    CreateOutcomeUnknown,
    NoCapacity,
    Invalid,
    Conflict,
    WouldBlock,
}
pub type Result<T> = core::result::Result<T, Error>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Name {
    Console,
    Store,
    Counter,
    Tasks0,
    Tasks1,
    Record,
    Control,
    StoreAdmin,
    RecordAdmin,
    CounterAdmin,
}
pub const NAMES: usize = Name::CounterAdmin as usize + 1;
impl Name {
    pub fn contract(self) -> &'static str {
        match self {
            Self::Console => "console/v1",
            Self::Store | Self::Record => "store/v1",
            Self::Counter => "counter/v1",
            Self::Tasks0 | Self::Tasks1 => "task-status/v1",
            _ => "service-control/v1",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Read,
    Write,
    ReadWrite,
}
#[derive(Debug)]
pub enum Op {
    Attach(Fid),
    Create {
        parent: Fid,
        name: String,
        mode: Mode,
    },
    Walk {
        from: Fid,
        to: Fid,
        path: String,
    },
    Open(Fid, Mode),
    Read {
        fid: Fid,
        offset: u64,
        len: usize,
    },
    Write {
        fid: Fid,
        offset: u64,
        data: Box<[u8]>,
    },
    Replace {
        fid: Fid,
        data: Box<[u8]>,
    },
    Clunk(Fid),
}
impl Op {
    pub fn weight(&self) -> Result<usize> {
        match self {
            Self::Write { data, .. } | Self::Replace { data, .. } if data.len() > MAX_DATA => {
                Err(Error::NoCapacity)
            }
            Self::Read { len, .. } if *len > MAX_DATA => Err(Error::NoCapacity),
            Self::Create { name, .. } if name.len() > MAX_PATH => Err(Error::NoCapacity),
            Self::Create { name, .. } => Ok(name.len()),
            Self::Walk { path, .. } if path.len() > MAX_PATH => Err(Error::NoCapacity),
            Self::Write { data, .. } | Self::Replace { data, .. } => Ok(data.len()),
            Self::Walk { path, .. } => Ok(path.len()),
            _ => Ok(0),
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Response {
    Done,
    Data(Vec<u8>),
    Written(usize),
}
pub trait Session: Send + Sync {
    fn call(&self, op: Op) -> Result<Response>;
    /// Explicit close waits for server-side fid cleanup. Drop may be asynchronous.
    fn disconnect(&self);
}
pub trait Connect: Send + Sync {
    fn connect(&self, name: Name) -> Result<Arc<dyn Session>>;
}
#[derive(Clone, Default)]
pub struct Namespace(Arc<Vec<(String, Name)>>);
impl Namespace {
    pub fn mount(&self, path: &str, name: Name) -> Result<Self> {
        validate(path)?;
        let mut v = self.clone();
        let mounts = Arc::make_mut(&mut v.0);
        mounts.retain(|(p, _)| p != path);
        mounts.push((path.to_string(), name));
        Ok(v)
    }
    pub fn resolve<'a>(&self, path: &'a str) -> Result<(Name, &'a str)> {
        validate(path)?;
        self.0
            .iter()
            .filter_map(|(base, name)| {
                let suffix = path.strip_prefix(base)?;
                if base == "/" || suffix.is_empty() || suffix.starts_with('/') {
                    Some((base.len(), *name, suffix.trim_start_matches('/')))
                } else {
                    None
                }
            })
            .max_by_key(|e| e.0)
            .map(|(_, name, s)| (name, s))
            .ok_or(Error::NotFound)
    }
    pub fn open(&self, access: &dyn Connect, path: &str, mode: Mode) -> Result<File> {
        self.open_or_create(access, path, mode, false)
    }
    pub fn create(&self, access: &dyn Connect, path: &str, mode: Mode) -> Result<File> {
        self.open_or_create(access, path, mode, true)
    }
    fn open_or_create(
        &self,
        access: &dyn Connect,
        path: &str,
        mode: Mode,
        create: bool,
    ) -> Result<File> {
        let (name, sub) = match self.resolve(path) {
            Ok(binding) => binding,
            Err(Error::NotFound) => {
                let mut children = alloc::collections::BTreeSet::new();
                for (mount, _) in self.0.iter() {
                    if let Some(sub) = mount.strip_prefix(path) {
                        if let Some(relative) = if path == "/" {
                            Some(sub)
                        } else {
                            sub.strip_prefix('/')
                        } {
                            if let Some(child) =
                                relative.split('/').next().filter(|s| !s.is_empty())
                            {
                                children.insert(child);
                            }
                        }
                    }
                }
                if children.is_empty() {
                    return Err(Error::NotFound);
                }
                if create || mode != Mode::Read {
                    return Err(Error::Denied);
                }
                let data = children
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into_bytes();
                return Ok(File {
                    backing: Backing::Mounts(data),
                    offset: 0,
                });
            }
            Err(e) => return Err(e),
        };
        let (walk, leaf) = if create {
            if sub.is_empty() {
                return Err(Error::Conflict);
            }
            let (parent, leaf) = sub.rsplit_once('/').unwrap_or(("", sub));
            (parent, Some(leaf))
        } else {
            (sub, None)
        };
        let session = access.connect(name)?;
        let setup = (|| {
            let done = |op| match session.call(op)? {
                Response::Done => Ok(()),
                _ => Err(Error::Invalid),
            };
            done(Op::Attach(0))?;
            done(Op::Walk {
                from: 0,
                to: 1,
                path: walk.to_string(),
            })?;
            done(Op::Clunk(0))?;
            if let Some(leaf) = leaf {
                done(Op::Create {
                    parent: 1,
                    name: leaf.into(),
                    mode,
                })?
            } else {
                done(Op::Open(1, mode))?
            }
            Ok(())
        })();
        if let Err(e) = setup {
            session.disconnect();
            return Err(e);
        }
        Ok(File {
            backing: Backing::Remote(session),
            offset: 0,
        })
    }
}
fn validate(path: &str) -> Result<()> {
    if !path.starts_with('/')
        || path.len() > MAX_PATH
        || (path != "/"
            && path[1..]
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == ".."))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
/// 打开的 fid 固定其连接。显式关闭消耗句柄并等待服务端回收。
///
/// ```compile_fail
/// fn use_after_close(mut file: service_api::File) {
///     file.close().unwrap();
///     file.read(1).unwrap();
/// }
/// ```
pub struct File {
    backing: Backing,
    offset: u64,
}
enum Backing {
    Mounts(Vec<u8>),
    Remote(Arc<dyn Session>),
}
impl File {
    pub fn read(&mut self, len: usize) -> Result<Vec<u8>> {
        let data = self.read_at(self.offset, len)?;
        self.offset = self
            .offset
            .checked_add(data.len() as u64)
            .ok_or(Error::NoCapacity)?;
        Ok(data)
    }
    pub fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if len > MAX_DATA {
            return Err(Error::NoCapacity);
        }
        let session = match &self.backing {
            Backing::Remote(s) => s,
            Backing::Mounts(data) => {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                return Ok(data[start..start + len.min(data.len() - start)].to_vec());
            }
        };
        if let Response::Data(data) = session.call(Op::Read {
            fid: 1,
            offset,
            len,
        })? {
            if data.len() <= len {
                return Ok(data);
            }
        }
        Err(Error::Invalid)
    }
    pub fn write(&mut self, bytes: &[u8]) -> Result<usize> {
        let n = self.write_at(self.offset, bytes)?;
        self.offset += n as u64;
        Ok(n)
    }
    pub fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<usize> {
        if bytes.len() > MAX_DATA || offset.checked_add(bytes.len() as u64).is_none() {
            return Err(Error::NoCapacity);
        }
        let Backing::Remote(session) = &self.backing else {
            return Err(Error::Denied);
        };
        if let Response::Written(n) = session.call(Op::Write {
            fid: 1,
            offset,
            data: bytes.into(),
        })? {
            if n <= bytes.len() {
                return Ok(n);
            }
        }
        Err(Error::Invalid)
    }
    /// 将整个普通文件替换为所给字节，包括空文件；保留普通偏移写的语义。
    pub fn replace(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_DATA {
            return Err(Error::NoCapacity);
        }
        let Backing::Remote(session) = &self.backing else {
            return Err(Error::Denied);
        };
        match session.call(Op::Replace {
            fid: 1,
            data: bytes.into(),
        })? {
            Response::Done => Ok(()),
            _ => Err(Error::WriteOutcomeUnknown),
        }
    }
    pub fn close(self) -> Result<()> {
        let Backing::Remote(session) = self.backing else {
            return Ok(());
        };
        let result = session.call(Op::Clunk(1));
        session.disconnect();
        result.map(|_| ())
    }
}
/// 串口服务得到唯一的字节接口，组件不能复制设备能力。
///
/// ```compile_fail
/// fn duplicate(uart: Box<dyn service_api::Serial>) {
///     let second = uart.clone();
/// }
/// ```
pub trait Serial: Send {
    fn read(&mut self) -> Option<u8>;
    fn write(&mut self, data: &[u8]) -> usize;
}
pub trait Inspect: Send {
    fn list(&self) -> Vec<String>;
    fn snapshot(&self, slot: usize, generation: usize) -> Result<String>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub schema: u32,
    pub revision: u64,
    pub data: Vec<u8>,
}
pub trait Admin: Send + Sync {
    fn hart(&self) -> usize;
    fn run_on(
        &self,
        hart: usize,
        job: Box<dyn FnOnce() -> Result<String> + Send>,
    ) -> Result<String>;
    fn task_id(&self) -> String;
    fn shutdown(&self) -> !;
}

/// Backend of a file subtree. Open returns optional immutable per-fid data.
/// WouldBlock is valid only before an operation has produced side effects.
pub trait FileTree: Send {
    /// 在服务所属核执行，只在就绪成功后才允许发布。
    fn start(&mut self) -> Result<()> {
        Ok(())
    }
    fn contract(&self) -> &'static str;
    fn contains(&self, path: &str) -> bool;
    /// 创建普通文件并返回其初始 fid 数据；失败时目录保持原样。
    fn create(&mut self, _parent: &str, _name: &str, _mode: Mode) -> Result<Option<Vec<u8>>> {
        Err(Error::Denied)
    }
    fn open(&mut self, path: &str, mode: Mode) -> Result<Option<Vec<u8>>>;
    fn read(&mut self, file: &Opened, offset: u64, len: usize) -> Result<Vec<u8>>;
    fn write(&mut self, file: &Opened, offset: u64, data: &[u8]) -> Result<usize>;
    /// 服务可取走工作及一次性回复权。返回的工作由运行时走普通读写路径。
    fn defer(&mut self, work: Work) -> Option<Work> {
        Some(work)
    }
    /// 在服务任务上下文推进设备完成；返回 true 表示仍有工作需要就绪通知。
    fn poll(&mut self) -> bool {
        false
    }
    fn cancel(&mut self, _connection: u64) {}
    fn replace(&mut self, _file: &Opened, _data: Vec<u8>) -> Result<()> {
        Err(Error::Denied)
    }
    fn export(&self) -> Result<Snapshot> {
        Err(Error::Denied)
    }
    fn import(&mut self, _image: Snapshot) -> Result<()> {
        Err(Error::Denied)
    }
}

pub struct Opened {
    pub path: String,
    pub snapshot: Option<Vec<u8>>,
}

/// 服务在受理请求时取得已打开对象的引用，Clunk 只释放 fid 表里的引用。
pub struct Io {
    pub file: Arc<Opened>,
    pub op: Op,
}
/// 持有一条已受理的 I/O 和唯一回复权。
///
/// ```compile_fail
/// fn reply_twice(w:service_api::Work){
///     w.complete(Ok(service_api::Response::Written(1))).unwrap();
///     w.complete(Ok(service_api::Response::Written(1))).unwrap();
/// }
/// ```
pub struct Work {
    pub connection: u64,
    io: Io,
    reply: Box<dyn FnOnce(Result<Response>) -> Result<()> + Send>,
}
impl Work {
    pub fn io(&self) -> &Io {
        &self.io
    }
    pub fn new(
        connection: u64,
        io: Io,
        reply: impl FnOnce(Result<Response>) -> Result<()> + Send + 'static,
    ) -> Self {
        Self {
            connection,
            io,
            reply: Box::new(reply),
        }
    }
    /// 消耗回复权。接收者断开时返回 Closed，已经完成的设备写不重试。
    pub fn complete(self, result: Result<Response>) -> Result<()> {
        let checked = match (&self.io.op, &result) {
            (Op::Read { len, .. }, Ok(Response::Data(data))) if data.len() <= *len => result,
            (Op::Write { data, .. }, Ok(Response::Written(n))) if *n <= data.len() => result,
            (_, Err(_)) => result,
            (Op::Write { .. }, _) => Err(Error::WriteOutcomeUnknown),
            _ => Err(Error::Invalid),
        };
        (self.reply)(checked)
    }
}
