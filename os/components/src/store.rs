//! 可迁移的有界 RAM 文件树。两个后端共享协议与快照格式，分别用线性表和有序索引查找文件。
use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use service_api::*;
const MAX_FILES: usize = 16;
enum Files {
    Linear(Vec<(String, Vec<u8>)>),
    Indexed(BTreeMap<String, Vec<u8>>),
}
impl Files {
    fn empty(indexed: bool) -> Self {
        if indexed {
            Self::Indexed(BTreeMap::new())
        } else {
            Self::Linear(Vec::new())
        }
    }
    fn get(&self, name: &str) -> Option<&Vec<u8>> {
        match self {
            Self::Linear(v) => v.iter().find(|(n, _)| n == name).map(|x| &x.1),
            Self::Indexed(m) => m.get(name),
        }
    }
    fn get_mut(&mut self, name: &str) -> Option<&mut Vec<u8>> {
        match self {
            Self::Linear(v) => v.iter_mut().find(|(n, _)| n == name).map(|x| &mut x.1),
            Self::Indexed(m) => m.get_mut(name),
        }
    }
    fn len(&self) -> usize {
        match self {
            Self::Linear(v) => v.len(),
            Self::Indexed(m) => m.len(),
        }
    }
    fn insert(&mut self, name: &str, bytes: Vec<u8>) -> Result<()> {
        if !valid(name) {
            return Err(Error::Invalid);
        }
        if self.get(name).is_some() {
            return Err(Error::Conflict);
        }
        if self.len() == MAX_FILES || bytes.len() > MAX_DATA {
            return Err(Error::NoCapacity);
        }
        match self {
            Self::Linear(v) => v.push((name.into(), bytes)),
            Self::Indexed(m) => {
                m.insert(name.into(), bytes);
            }
        }
        Ok(())
    }
    fn names(&self) -> Vec<&str> {
        let mut v: Vec<_> = match self {
            Self::Linear(v) => v.iter().map(|x| x.0.as_str()).collect(),
            Self::Indexed(m) => m.keys().map(String::as_str).collect(),
        };
        v.sort_unstable();
        v
    }
}
fn valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !matches!(name, "." | ".." | "version" | "revision")
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
}
pub struct Store {
    files: Files,
    revision: u64,
}
impl Store {
    pub fn new(indexed: bool) -> Self {
        let mut files = Files::empty(indexed);
        for n in ["cons", "note"] {
            files.insert(n, Vec::new()).unwrap()
        }
        Self { files, revision: 0 }
    }
}
fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    let (head, tail) = input.split_at_checked(n).ok_or(Error::Invalid)?;
    *input = tail;
    Ok(head)
}
// 覆盖修订号和完整目录数据，用于检测跨实现迁移时的损坏。
fn checksum(revision: u64, data: &[u8]) -> u32 {
    revision
        .to_le_bytes()
        .iter()
        .chain(data)
        .fold(2166136261u32, |hash, b| {
            (hash ^ u32::from(*b)).wrapping_mul(16777619)
        })
}
impl FileTree for Store {
    fn contract(&self) -> &'static str {
        "store/v1"
    }
    fn contains(&self, p: &str) -> bool {
        matches!(p, "version" | "revision") || self.files.get(p).is_some()
    }
    fn open(&mut self, p: &str, m: Mode) -> Result<Option<Vec<u8>>> {
        if p.is_empty() && m == Mode::Read {
            let mut names = self.files.names();
            names.extend(["revision", "version"]);
            return Ok(Some(names.join("\n").into_bytes()));
        }
        if self.files.get(p).is_some() || (m == Mode::Read && matches!(p, "revision" | "version")) {
            Ok(None)
        } else {
            Err(Error::Denied)
        }
    }
    fn create(&mut self, dir: &str, name: &str, _: Mode) -> Result<Option<Vec<u8>>> {
        if !dir.is_empty() {
            return Err(Error::Invalid);
        }
        let next = self.revision.checked_add(1).ok_or(Error::NoCapacity)?;
        self.files.insert(name, Vec::new())?;
        self.revision = next;
        Ok(None)
    }
    fn read(&mut self, f: &Opened, offset: u64, len: usize) -> Result<Vec<u8>> {
        Ok(match f.path.as_str() {
            "version" => super::slice(
                if matches!(self.files, Files::Linear(_)) {
                    b"v1"
                } else {
                    b"v2"
                },
                offset,
                len,
            ),
            "revision" => super::slice(self.revision.to_string().as_bytes(), offset, len),
            name => super::slice(self.files.get(name).ok_or(Error::NotFound)?, offset, len),
        })
    }
    fn write(&mut self, f: &Opened, offset: u64, data: &[u8]) -> Result<usize> {
        let start = usize::try_from(offset).map_err(|_| Error::NoCapacity)?;
        let end = start
            .checked_add(data.len())
            .filter(|&n| n <= MAX_DATA)
            .ok_or(Error::NoCapacity)?;
        let next = self.revision.checked_add(1).ok_or(Error::NoCapacity)?;
        let bytes = self.files.get_mut(&f.path).ok_or(Error::NotFound)?;
        bytes.resize(end.max(bytes.len()), 0);
        bytes[start..end].copy_from_slice(data);
        self.revision = next;
        Ok(data.len())
    }
    fn replace(&mut self, file: &Opened, data: Vec<u8>) -> Result<()> {
        if data.len() > MAX_DATA {
            return Err(Error::NoCapacity);
        }
        let next = self.revision.checked_add(1).ok_or(Error::NoCapacity)?;
        *self.files.get_mut(&file.path).ok_or(Error::NotFound)? = data;
        self.revision = next;
        Ok(())
    }
    fn export(&self) -> Result<Snapshot> {
        let mut data = Vec::new();
        for name in self.files.names() {
            let bytes = self.files.get(name).unwrap();
            data.push(name.len() as u8);
            data.extend_from_slice(name.as_bytes());
            data.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            data.extend_from_slice(bytes);
        }
        data.extend_from_slice(&checksum(self.revision, &data).to_le_bytes());
        Ok(Snapshot {
            schema: 2,
            revision: self.revision,
            data,
        })
    }
    fn import(&mut self, image: Snapshot) -> Result<()> {
        let mut files = Files::empty(matches!(self.files, Files::Indexed(_)));
        match image.schema {
            1 => {
                if image.data.len() > MAX_DATA {
                    return Err(Error::NoCapacity);
                }
                files.insert("note", image.data.clone())?;
                files.insert("cons", image.data)?;
            }
            2 => {
                let (body, check) = image
                    .data
                    .split_at_checked(image.data.len().checked_sub(4).ok_or(Error::Invalid)?)
                    .ok_or(Error::Invalid)?;
                if checksum(image.revision, body).to_le_bytes() != check {
                    return Err(Error::Invalid);
                }
                let mut input = body;
                while !input.is_empty() {
                    let n = take(&mut input, 1)?[0] as usize;
                    let name =
                        core::str::from_utf8(take(&mut input, n)?).map_err(|_| Error::Invalid)?;
                    let size =
                        u16::from_le_bytes(take(&mut input, 2)?.try_into().unwrap()) as usize;
                    if size > MAX_DATA {
                        return Err(Error::NoCapacity);
                    }
                    files.insert(name, take(&mut input, size)?.to_vec())?;
                }
                if files.get("cons").is_none() || files.get("note").is_none() {
                    return Err(Error::Invalid);
                }
            }
            _ => return Err(Error::Invalid),
        }
        self.files = files;
        self.revision = image.revision;
        Ok(())
    }
}
