use directory::{apply, Change, History, Slot};
use service_api::{Error, Name, NAMES};
#[test]
fn same_log_same_results_including_conflicts_and_republication() {
    let empty = Slot::default();
    let mut replicas = [[empty; NAMES]; 2];
    let mut hist = History::<Change, 2>::default();
    let publish = Change {
        name: Name::Store,
        expected: empty,
        next: Some(3),
    };
    let slot = Slot {
        generation: 1,
        instance: Some(3),
    };
    hist.push(publish).unwrap();
    hist.push(Change {
        name: Name::Store,
        expected: slot,
        next: None,
    })
    .unwrap();
    for c in hist.unread(0) {
        apply(&mut replicas[0], *c).unwrap();
    }
    hist.acknowledge(0, 2);
    assert_eq!(hist.push(publish), Err(Error::Busy));
    for c in hist.unread(1) {
        apply(&mut replicas[1], *c).unwrap();
    }
    hist.acknowledge(1, 2);
    for _ in 0..16 {
        hist.push(publish).unwrap();
        for (h, d) in replicas.iter_mut().enumerate() {
            for c in hist.unread(h) {
                assert_eq!(apply(d, *c), Err(Error::Conflict));
            }
            hist.acknowledge(h, hist.tail);
        }
    }
    assert_eq!(replicas[0], replicas[1]);
    assert_eq!(
        replicas[0][Name::Store as usize],
        Slot {
            instance: None,
            generation: 2
        }
    );
}
