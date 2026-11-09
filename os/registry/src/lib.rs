//! 顺序目录与有界操作历史。只在两个副本均已应用旧条目后复用环形槽。
#![no_std]
#![forbid(unsafe_code)]
use service_api::{Error, Name, Result, NAMES};
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Slot {
    pub generation: u64,
    pub instance: Option<u64>,
}
#[derive(Clone, Copy)]
pub struct Change {
    pub name: Name,
    pub expected: Slot,
    pub next: Option<u64>,
}
pub type Directory = [Slot; NAMES];
pub fn apply(directory: &mut Directory, c: Change) -> Result<Slot> {
    let slot = &mut directory[c.name as usize];
    if *slot != c.expected {
        return Err(Error::Conflict);
    }
    slot.generation = slot.generation.checked_add(1).ok_or(Error::NoCapacity)?;
    slot.instance = c.next;
    Ok(*slot)
}

/// Cursors denote the length of an applied prefix, starting at zero.
pub struct History<T, const N: usize> {
    entries: [Option<T>; N],
    pub tail: usize,
    applied: [usize; 2],
}
impl<T, const N: usize> Default for History<T, N> {
    fn default() -> Self {
        assert!(N > 0);
        Self {
            entries: core::array::from_fn(|_| None),
            tail: 0,
            applied: [0; 2],
        }
    }
}
impl<T, const N: usize> History<T, N> {
    pub fn has_room(&self) -> bool {
        self.tail - self.applied[0].min(self.applied[1]) < N
    }
    pub fn push(&mut self, value: T) -> Result<()> {
        if !self.has_room() {
            return Err(Error::Busy);
        }
        let next = self.tail.checked_add(1).ok_or(Error::NoCapacity)?;
        self.entries[self.tail % N] = Some(value);
        self.tail = next;
        Ok(())
    }
    pub fn applied(&self, h: usize) -> usize {
        self.applied[h]
    }
    pub fn unread(&self, h: usize) -> impl Iterator<Item = &T> {
        (self.applied[h]..self.tail).map(|i| self.entries[i % N].as_ref().unwrap())
    }
    pub fn acknowledge(&mut self, h: usize, end: usize) {
        assert!((self.applied[h]..=self.tail).contains(&end));
        self.applied[h] = end;
    }
}
