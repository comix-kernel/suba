use components::{Action, Counter, Server, Store};
use service_api::*;
use std::sync::{Arc, Mutex};

fn perform(server: &mut Server, id: u64, op: Op) -> Result<Response> {
    match server.accept(id, op)? {
        Action::Done(r) => Ok(r),
        Action::Io(io) => server.progress(&io),
    }
}
fn walk(to: u32, path: &str) -> Op {
    Op::Walk {
        from: 0,
        to,
        path: path.into(),
    }
}
fn open(server: &mut Server, id: u64, path: &str, mode: Mode) {
    perform(server, id, Op::Attach(0)).unwrap();
    perform(server, id, walk(1, path)).unwrap();
    perform(server, id, Op::Open(1, mode)).unwrap();
}
fn write(fid: u32, bytes: &[u8]) -> Op {
    Op::Write {
        fid,
        offset: 0,
        data: bytes.into(),
    }
}
#[test]
fn walk_is_atomic_and_fids_are_per_connection() {
    let mut s = Server::new(Box::new(Store::new(false)));
    open(&mut s, 1, "note", Mode::Read);
    open(&mut s, 2, "note", Mode::Write);
    assert_eq!(
        perform(&mut s, 1, walk(3, "note/missing")),
        Err(Error::NotFound)
    );
    perform(&mut s, 1, walk(3, "revision")).unwrap();
    assert_eq!(
        perform(&mut s, 1, write(1, b"forbidden")),
        Err(Error::Denied)
    );
    perform(&mut s, 2, write(1, b"abcd")).unwrap();
    perform(&mut s, 1, Op::Clunk(1)).unwrap();
    assert_eq!(perform(&mut s, 1, Op::Clunk(1)), Err(Error::NotFound));
    s.disconnect(1);
    assert!(perform(&mut s, 2, write(1, b"ok")).is_ok());
    perform(&mut s, 1, Op::Attach(0)).unwrap();
}
#[test]
fn accepted_io_survives_clunk_and_fid_reuse() {
    let mut s = Server::new(Box::new(Store::new(false)));
    open(&mut s, 1, "version", Mode::Read);
    let Action::Io(old) = s
        .accept(
            1,
            Op::Read {
                fid: 1,
                offset: 0,
                len: 4,
            },
        )
        .unwrap()
    else {
        panic!()
    };
    perform(&mut s, 1, Op::Clunk(1)).unwrap();
    perform(&mut s, 1, walk(1, "revision")).unwrap();
    perform(&mut s, 1, Op::Open(1, Mode::Read)).unwrap();
    assert_eq!(s.progress(&old), Ok(Response::Data(b"v1".to_vec())));
    assert_eq!(
        perform(
            &mut s,
            1,
            Op::Read {
                fid: 1,
                offset: 0,
                len: 4
            }
        ),
        Ok(Response::Data(b"0".to_vec()))
    );
}
struct Short {
    calls: Mutex<usize>,
}
impl Session for Short {
    fn disconnect(&self) {}
    fn call(&self, op: Op) -> Result<Response> {
        if let Op::Write { data, .. } = op {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                return Ok(Response::Written(data.len().min(2)));
            }
            return Err(Error::WriteOutcomeUnknown);
        }
        Ok(Response::Done)
    }
}
struct Source(Arc<Short>);
impl Connect for Source {
    fn connect(&self, _: Name) -> Result<Arc<dyn Session>> {
        Ok(self.0.clone())
    }
}
#[test]
fn filter_counts_only_confirmed_bytes() {
    let src = Source(Arc::new(Short {
        calls: Mutex::new(0),
    }));
    let ns = Namespace::default().mount("/dev", Name::Console).unwrap();
    let sink = ns.open(&src, "/dev/cons", Mode::Write).unwrap();
    let mut s = Server::new(Box::new(Counter::new(sink)));
    open(&mut s, 1, "cons", Mode::Write);
    open(&mut s, 2, "count", Mode::Read);
    assert_eq!(
        perform(&mut s, 1, write(1, b"abcd")),
        Ok(Response::Written(2))
    );
    assert_eq!(
        perform(&mut s, 1, write(1, b"cd")),
        Err(Error::WriteOutcomeUnknown)
    );
    assert_eq!(
        perform(
            &mut s,
            2,
            Op::Read {
                fid: 1,
                offset: 0,
                len: 32
            }
        ),
        Ok(Response::Data(b"2".to_vec()))
    );
    assert_eq!(*src.0.calls.lock().unwrap(), 2);
}
#[test]
fn namespace_uses_components_and_preserves_earlier_views() {
    let ns = Namespace::default()
        .mount("/", Name::Store)
        .unwrap()
        .mount("/dev", Name::Console)
        .unwrap();
    let private = ns.mount("/dev/log", Name::Record).unwrap();
    assert_eq!(private.resolve("/dev/log/x"), Ok((Name::Record, "x")));
    assert_eq!(
        private.resolve("/dev/logger"),
        Ok((Name::Console, "logger"))
    );
    assert_eq!(ns.resolve("/dev/log/x"), Ok((Name::Console, "log/x")));
    assert_eq!(ns.resolve("/device"), Ok((Name::Store, "device")));
}

#[test]
fn complete_tree_migrates_both_ways_and_invalid_images_are_atomic() {
    let file = |name: &str| Opened {
        path: name.into(),
        snapshot: None,
    };
    let mut source = Store::new(false);
    source.create("", "chapter", Mode::Write).unwrap();
    source.write(&file("note"), 0, b"first").unwrap();
    source.write(&file("chapter"), 2, b"two").unwrap();
    let image = source.export().unwrap();
    let mut target = Store::new(true);
    target.import(image.clone()).unwrap();
    assert_eq!(target.export().unwrap(), image);
    assert_eq!(target.read(&file("chapter"), 0, 32).unwrap(), b"\0\0two");
    assert!(target.read(&file("cons"), 0, 32).unwrap().is_empty());
    let mut damaged = image.clone();
    damaged.data[2] ^= 1;
    assert_eq!(target.import(damaged), Err(Error::Invalid));
    let mut truncated = image.clone();
    truncated.data.pop();
    assert_eq!(target.import(truncated), Err(Error::Invalid));
    assert_eq!(target.export().unwrap(), image);
    source.import(target.export().unwrap()).unwrap();
    let legacy = Snapshot {
        schema: 1,
        revision: 4,
        data: b"old".to_vec(),
    };
    target.import(legacy).unwrap();
    for name in ["note", "cons"] {
        assert_eq!(target.read(&file(name), 0, 8).unwrap(), b"old");
    }
    assert_eq!(target.export().unwrap().revision, 4);
}

#[test]
fn file_creation_is_atomic_and_directory_reads_are_per_open_snapshots() {
    let mut server = Server::new(Box::new(Store::new(true)));
    open(&mut server, 1, "", Mode::Read);
    perform(&mut server, 2, Op::Attach(0)).unwrap();
    let create = |name: &str| Op::Create {
        parent: 0,
        name: name.into(),
        mode: Mode::Write,
    };
    assert_eq!(
        perform(&mut server, 2, create("note")),
        Err(Error::Conflict)
    );
    assert_eq!(
        perform(&mut server, 2, create("../escape")),
        Err(Error::Invalid)
    );
    perform(&mut server, 2, create("added")).unwrap();
    perform(&mut server, 2, write(0, b"data")).unwrap();
    let read_dir = Op::Read {
        fid: 1,
        offset: 0,
        len: MAX_DATA,
    };
    let expected = b"cons\nnote\nrevision\nversion";
    assert_eq!(
        perform(&mut server, 1, read_dir),
        Ok(Response::Data(expected.to_vec()))
    );
    open(&mut server, 3, "added", Mode::Read);
    assert_eq!(
        perform(
            &mut server,
            3,
            Op::Read {
                fid: 1,
                offset: 0,
                len: 4
            }
        ),
        Ok(Response::Data(b"data".to_vec()))
    );
}

#[test]
fn explicit_offsets_do_not_move_the_shared_file_cursor() {
    struct Memory(Mutex<Vec<u8>>);
    impl Session for Memory {
        fn call(&self, op: Op) -> Result<Response> {
            let mut bytes = self.0.lock().unwrap();
            Ok(match op {
                Op::Read { offset, len, .. } => Response::Data(
                    bytes
                        .iter()
                        .skip(offset as usize)
                        .take(len)
                        .copied()
                        .collect(),
                ),
                Op::Write { offset, data, .. } => {
                    let at = offset as usize;
                    bytes[at..at + data.len()].copy_from_slice(&data);
                    Response::Written(data.len())
                }
                _ => Response::Done,
            })
        }
        fn disconnect(&self) {}
    }
    struct Access(Arc<Memory>);
    impl Connect for Access {
        fn connect(&self, _: Name) -> Result<Arc<dyn Session>> {
            Ok(self.0.clone())
        }
    }
    let access = Access(Arc::new(Memory(Mutex::new(b"abcd".to_vec()))));
    let ns = Namespace::default().mount("/work", Name::Store).unwrap();
    let mut f = ns.open(&access, "/work/note", Mode::Read).unwrap();
    assert_eq!(f.read_at(2, 2).unwrap(), b"cd");
    assert_eq!(f.read(2).unwrap(), b"ab");
    let mut output = ns.open(&access, "/work/note", Mode::Write).unwrap();
    output.write_at(3, b"x").unwrap();
    output.write(b"y").unwrap();
    assert_eq!(f.read(3).unwrap(), b"cx");
    assert_eq!(f.read_at(0, 1).unwrap(), b"y");
}

#[test]
fn replace_requires_write_access_and_sets_exact_length() {
    for indexed in [false, true] {
        let mut s = Server::new(Box::new(Store::new(indexed)));
        open(&mut s, 1, "note", Mode::ReadWrite);
        open(&mut s, 2, "note", Mode::Read);
        let replace = |data: &[u8]| Op::Replace {
            fid: 1,
            data: data.into(),
        };
        let read = || Op::Read {
            fid: 1,
            offset: 0,
            len: MAX_DATA,
        };
        perform(&mut s, 1, write(1, b"long text")).unwrap();
        assert_eq!(perform(&mut s, 2, replace(b"x")), Err(Error::Denied));
        assert_eq!(
            perform(&mut s, 1, replace(&vec![0; MAX_DATA + 1])),
            Err(Error::NoCapacity)
        );
        assert_eq!(
            perform(&mut s, 1, read()),
            Ok(Response::Data(b"long text".to_vec()))
        );
        perform(&mut s, 1, replace(b"hi")).unwrap();
        assert_eq!(
            perform(&mut s, 2, read()),
            Ok(Response::Data(b"hi".to_vec()))
        );
        perform(&mut s, 1, replace(b"")).unwrap();
        assert_eq!(perform(&mut s, 2, read()), Ok(Response::Data(vec![])));
    }
}
