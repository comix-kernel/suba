//! 预链接的文件服务实现。组件只通过持有的句柄使用设备或运行时。
#![no_std]
#![forbid(unsafe_code)]
extern crate alloc;
use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use service_api::*;
mod programs;
mod server;
pub mod shell;
mod terminal;
pub use server::{Action, Server};
fn slice(bytes: &[u8], pos: u64, len: usize) -> Vec<u8> {
    let start = usize::try_from(pos).unwrap_or(usize::MAX).min(bytes.len());
    let size = len.min(bytes.len() - start);
    bytes[start..start + size].to_vec()
}

pub struct Console {
    device: Box<dyn Serial>,
    reads: alloc::collections::VecDeque<Work>,
}
impl Console {
    pub fn new(device: Box<dyn Serial>) -> Self {
        Self {
            device,
            reads: Default::default(),
        }
    }
}

impl FileTree for Console {
    fn defer(&mut self, work: Work) -> Option<Work> {
        if matches!(work.io().op, Op::Read { len: 1.., .. }) && work.io().file.snapshot.is_none() {
            self.reads.push_back(work);
            None
        } else {
            Some(work)
        }
    }
    fn poll(&mut self) -> bool {
        while let Some(work) = self.reads.pop_front() {
            let Op::Read { len, offset, .. } = work.io().op else {
                unreachable!()
            };
            match self.read(&work.io().file, offset, len) {
                Err(Error::WouldBlock) => {
                    self.reads.push_front(work);
                    break;
                }
                result => {
                    let _ = work.complete(result.map(Response::Data));
                }
            }
        }
        !self.reads.is_empty()
    }
    fn cancel(&mut self, connection: u64) {
        self.reads.retain(|work| work.connection != connection)
    }
    fn contract(&self) -> &'static str {
        "console/v1"
    }
    fn contains(&self, p: &str) -> bool {
        p == "cons"
    }
    fn open(&mut self, p: &str, mode: Mode) -> Result<Option<Vec<u8>>> {
        if p.is_empty() && mode == Mode::Read {
            return Ok(Some(b"cons\n".to_vec()));
        }
        if p == "cons" {
            Ok(None)
        } else {
            Err(Error::Denied)
        }
    }
    fn read(&mut self, _: &Opened, _: u64, len: usize) -> Result<Vec<u8>> {
        let first = self.device.read().ok_or(Error::WouldBlock)?;
        let mut v = alloc::vec![first];
        while v.len() < len {
            if let Some(b) = self.device.read() {
                v.push(b)
            } else {
                break;
            }
        }
        Ok(v)
    }
    fn write(&mut self, _: &Opened, _: u64, data: &[u8]) -> Result<usize> {
        let n = self.device.write(data);
        if n == 0 {
            Err(Error::WouldBlock)
        } else {
            Ok(n)
        }
    }
}
mod store;
pub use store::Store;
pub struct Counter {
    sink: File,
    confirmed: u64,
}
impl Counter {
    pub fn new(sink: File) -> Self {
        Self { sink, confirmed: 0 }
    }
}
impl FileTree for Counter {
    fn contract(&self) -> &'static str {
        "counter/v1"
    }
    fn contains(&self, p: &str) -> bool {
        matches!(p, "count" | "cons")
    }
    fn open(&mut self, p: &str, m: Mode) -> Result<Option<Vec<u8>>> {
        match (p, m) {
            ("", Mode::Read) => Ok(Some(b"cons\ncount\n".to_vec())),
            ("cons", Mode::Write) | ("count", Mode::Read) => Ok(None),
            _ => Err(Error::Denied),
        }
    }
    fn read(&mut self, _: &Opened, offset: u64, len: usize) -> Result<Vec<u8>> {
        Ok(slice(self.confirmed.to_string().as_bytes(), offset, len))
    }
    fn write(&mut self, _: &Opened, _: u64, data: &[u8]) -> Result<usize> {
        let max = (u64::MAX - self.confirmed).min(data.len() as u64) as usize;
        if max == 0 {
            return Err(Error::NoCapacity);
        }
        let n = self.sink.write(&data[..max])?;
        self.confirmed += n as u64;
        Ok(n)
    }
    fn export(&self) -> Result<Snapshot> {
        Ok(Snapshot {
            schema: 1,
            revision: self.confirmed,
            data: Vec::new(),
        })
    }
    fn import(&mut self, s: Snapshot) -> Result<()> {
        if s.schema != 1 || !s.data.is_empty() {
            return Err(Error::Invalid);
        }
        self.confirmed = s.revision;
        Ok(())
    }
}
pub struct Tasks(pub Box<dyn Inspect>);
fn parse_id(id: &str) -> Option<(usize, usize)> {
    let (slot, generation) = id.split_once('-')?;
    Some((slot.parse().ok()?, generation.parse().ok()?))
}
impl FileTree for Tasks {
    fn contract(&self) -> &'static str {
        "task-status/v1"
    }
    fn contains(&self, p: &str) -> bool {
        parse_id(p.strip_suffix("/status").unwrap_or(p)).is_some()
    }
    fn open(&mut self, p: &str, m: Mode) -> Result<Option<Vec<u8>>> {
        if m != Mode::Read {
            return Err(Error::Denied);
        }
        if p.is_empty() {
            return Ok(Some(self.0.list().join("\n").into_bytes()));
        }
        if let Some((s, g)) = parse_id(p) {
            self.0.snapshot(s, g)?;
            return Ok(Some(b"status\n".to_vec()));
        }
        let (s, g) = p
            .strip_suffix("/status")
            .and_then(parse_id)
            .ok_or(Error::Invalid)?;
        Ok(Some(self.0.snapshot(s, g)?.into_bytes()))
    }
    fn read(&mut self, f: &Opened, offset: u64, len: usize) -> Result<Vec<u8>> {
        Ok(slice(f.snapshot.as_ref().unwrap(), offset, len))
    }
    fn write(&mut self, _: &Opened, _: u64, _: &[u8]) -> Result<usize> {
        Err(Error::Denied)
    }
}
