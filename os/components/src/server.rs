//! 服务端按连接管理 fid；读写一经受理即持有打开对象的独立引用。
use alloc::{boxed::Box, collections::BTreeMap, format, string::String, sync::Arc, vec::Vec};
use service_api::*;
enum Node {
    Walked(String),
    Open(Mode, Arc<Opened>),
}
pub enum Action {
    Done(Response),
    Io(Io),
}
pub struct Server {
    pub tree: Box<dyn FileTree>,
    nodes: BTreeMap<(u64, Fid), Node>,
}
impl Server {
    pub fn new(tree: Box<dyn FileTree>) -> Self {
        Self {
            tree,
            nodes: BTreeMap::new(),
        }
    }
    pub fn fid_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn disconnect(&mut self, id: u64) {
        self.nodes.retain(|(c, _), _| *c != id)
    }
    fn insert(&mut self, key: (u64, Fid), path: String) -> Result<Action> {
        if self.nodes.contains_key(&key) {
            return Err(Error::Conflict);
        }
        if self.nodes.len() >= 128 {
            return Err(Error::NoCapacity);
        }
        self.nodes.insert(key, Node::Walked(path));
        Ok(Action::Done(Response::Done))
    }
    pub fn accept(&mut self, id: u64, op: Op) -> Result<Action> {
        op.weight()?;
        match op {
            Op::Create { parent, name, mode } => {
                let node = self.nodes.get_mut(&(id, parent)).ok_or(Error::NotFound)?;
                let Node::Walked(dir) = node else {
                    return Err(Error::Invalid);
                };
                if matches!(name.as_str(), "" | "." | "..") || name.contains('/') {
                    return Err(Error::Invalid);
                }
                let path = if dir.is_empty() {
                    name.clone()
                } else {
                    format!("{dir}/{name}")
                };
                if path.len() > MAX_PATH {
                    return Err(Error::NoCapacity);
                }
                let snapshot = self.tree.create(dir, &name, mode)?;
                *node = Node::Open(mode, Arc::new(Opened { path, snapshot }));
                Ok(Action::Done(Response::Done))
            }
            Op::Attach(fid) => self.insert((id, fid), String::new()),
            Op::Walk { from, to, path } => {
                let Node::Walked(base) = self.nodes.get(&(id, from)).ok_or(Error::NotFound)? else {
                    return Err(Error::Invalid);
                };
                let mut child = base.clone();
                if !path.is_empty() {
                    for part in path.split('/') {
                        if matches!(part, "" | "." | "..") {
                            return Err(Error::Invalid);
                        }
                        child = if child.is_empty() {
                            part.into()
                        } else {
                            format!("{child}/{part}")
                        };
                        if child.len() > MAX_PATH || !self.tree.contains(&child) {
                            return Err(Error::NotFound);
                        }
                    }
                }
                self.insert((id, to), child)
            }
            Op::Clunk(fid) => {
                self.nodes.remove(&(id, fid)).ok_or(Error::NotFound)?;
                Ok(Action::Done(Response::Done))
            }
            Op::Open(fid, mode) => {
                let n = self.nodes.get_mut(&(id, fid)).ok_or(Error::NotFound)?;
                let Node::Walked(path) = n else {
                    return Err(Error::Busy);
                };
                let snapshot = self.tree.open(path, mode)?;
                *n = Node::Open(
                    mode,
                    Arc::new(Opened {
                        path: core::mem::take(path),
                        snapshot,
                    }),
                );
                Ok(Action::Done(Response::Done))
            }
            Op::Replace { fid, data } => {
                let Node::Open(Mode::Write | Mode::ReadWrite, file) =
                    self.nodes.get(&(id, fid)).ok_or(Error::NotFound)?
                else {
                    return Err(Error::Denied);
                };
                self.tree.replace(file, data.into_vec())?;
                Ok(Action::Done(Response::Done))
            }
            Op::Read { fid, .. } | Op::Write { fid, .. } => {
                let Node::Open(mode, file) = self.nodes.get(&(id, fid)).ok_or(Error::NotFound)?
                else {
                    return Err(Error::Denied);
                };
                let want = if matches!(op, Op::Read { .. }) {
                    Mode::Read
                } else {
                    Mode::Write
                };
                if *mode != want && *mode != Mode::ReadWrite {
                    return Err(Error::Denied);
                }
                Ok(Action::Io(Io {
                    file: file.clone(),
                    op,
                }))
            }
        }
    }
    pub fn progress(&mut self, io: &Io) -> Result<Response> {
        match &io.op {
            Op::Read { offset, len, .. } => {
                let data = if let Some(bytes) = &io.file.snapshot {
                    super::slice(bytes, *offset, *len)
                } else if *len == 0 {
                    Vec::new()
                } else {
                    self.tree.read(&io.file, *offset, *len)?
                };
                if data.len() > *len {
                    return Err(Error::Invalid);
                }
                Ok(Response::Data(data))
            }
            Op::Write { offset, data, .. } => {
                let n = if data.is_empty() {
                    0
                } else {
                    self.tree.write(&io.file, *offset, data)?
                };
                if n > data.len() {
                    return Err(Error::WriteOutcomeUnknown);
                }
                Ok(Response::Written(n))
            }
            _ => unreachable!(),
        }
    }
}
