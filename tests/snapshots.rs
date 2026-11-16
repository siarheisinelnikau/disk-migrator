mod common;
use disk_migrator::{
    Result, capture,
    consistent::{ConsistentDisk, RegionKind, RegionReader, SnapshotRegion},
    image, invalid,
    layout::{self, Role},
    physical::{BlockSource, Consistency, SourceInfo},
    snapshot::*,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};

const VOLUMES: [&str; 2] = [
    r"\\?\Volume{11111111-1111-1111-1111-111111111111}\",
    r"\\?\Volume{22222222-2222-2222-2222-222222222222}\",
];
const WRITERS: [&str; 3] = [
    "e8132975-6f93-4464-a53e-1050253ae220",
    "afbab4a2-367d-4d15-a586-71dbb18f8485",
    "be000cbe-11fe-4426-9c58-531aa6355fc4",
];
#[derive(Default)]
struct State {
    events: Vec<String>,
    fail: Option<String>,
    writer_failed: bool,
    path_invalid: bool,
    writer_count: usize,
    added: usize,
    status_state: u32,
    duplicate_snapshot: bool,
}
struct Provider(Rc<RefCell<State>>);
impl Provider {
    fn new() -> (Self, Rc<RefCell<State>>) {
        let state = Rc::new(RefCell::new(State {
            writer_count: 3,
            status_state: 1,
            ..Default::default()
        }));
        (Self(state.clone()), state)
    }
    fn call(&self, action: &str) -> Result<()> {
        let mut s = self.0.borrow_mut();
        s.events.push(action.into());
        if s.fail.as_deref() == Some(action) {
            Err(invalid(format!("Injected VSS {action} failure.")))
        } else {
            Ok(())
        }
    }
}
impl SnapshotProvider for Provider {
    fn initialize(&mut self) -> Result<()> {
        self.call("initialize")
    }
    fn gather_metadata(&mut self, _: &AtomicBool) -> Result<()> {
        self.call("metadata")
    }
    fn start_set(&mut self) -> Result<String> {
        self.call("begin")?;
        Ok("snapshot-set-one".into())
    }
    fn add_volume(&mut self, _: &str) -> Result<String> {
        self.call("add")?;
        let mut s = self.0.borrow_mut();
        s.added += 1;
        Ok(format!(
            "snapshot-{}",
            if s.duplicate_snapshot { 1 } else { s.added }
        ))
    }
    fn prepare(&mut self, _: &AtomicBool) -> Result<()> {
        self.call("prepare")
    }
    fn take_snapshot(&mut self, _: &AtomicBool) -> Result<()> {
        self.call("snapshot")?;
        self.0.borrow_mut().status_state = 5;
        Ok(())
    }
    fn writer_status(&mut self, _: &AtomicBool) -> Result<Vec<WriterStatus>> {
        self.call("writers")?;
        let s = self.0.borrow();
        Ok(WRITERS[..s.writer_count.min(3)]
            .iter()
            .enumerate()
            .map(|(i, id)| WriterStatus {
                id: (*id).into(),
                name: format!("Fake writer {i}"),
                state: if s.writer_failed { 9 } else { s.status_state },
                failure: if s.writer_failed { -1 } else { 0 },
            })
            .collect())
    }
    fn device_path(&mut self, id: &str) -> Result<String> {
        self.call("path")?;
        Ok(if self.0.borrow().path_invalid {
            r"\\.\PhysicalDrive0".into()
        } else {
            format!(
                r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy{}",
                id.trim_start_matches("snapshot-")
            )
        })
    }
    fn complete(&mut self, _: &AtomicBool) -> Result<()> {
        self.call("complete")?;
        self.0.borrow_mut().status_state = 1;
        Ok(())
    }
    fn abort(&mut self) -> Result<()> {
        self.call("abort")
    }
    fn delete_set(&mut self, _: &str) -> Result<()> {
        self.call("delete")
    }
}
fn volumes() -> Vec<String> {
    VOLUMES.iter().map(|s| (*s).into()).collect()
}
#[test]
fn successful_lifecycle_coordinates_writers_and_cleans_only_after_backup_complete() {
    let (provider, state) = Provider::new();
    let flag = AtomicBool::new(false);
    let mut stages = Vec::new();
    let mut session =
        SnapshotSession::create(provider, &volumes(), &flag, |s| stages.push(s)).unwrap();
    assert_eq!(session.set.volumes.len(), 2);
    assert_eq!(session.set.writers_checked, 3);
    assert!(stages.contains(&SnapshotStage::Snapshot));
    assert!(!state.borrow().events.iter().any(|e| e == "delete"));
    session.finish(&flag).unwrap();
    drop(session);
    let events = &state.borrow().events;
    assert_eq!(events.last().unwrap(), "delete");
    assert_eq!(events.iter().filter(|e| *e == "complete").count(), 1);
    assert!(!events.iter().any(|e| e == "abort"));
    let prepare = events.iter().position(|e| e == "prepare").unwrap();
    let snapshot = events.iter().position(|e| e == "snapshot").unwrap();
    assert!(prepare < snapshot);
}
#[test]
fn failed_stages_abort_and_delete_the_created_set() {
    for stage in ["metadata", "begin", "add", "prepare", "snapshot", "path"] {
        let (provider, state) = Provider::new();
        state.borrow_mut().fail = Some(stage.into());
        assert!(
            SnapshotSession::create(provider, &volumes(), &AtomicBool::new(false), |_| {}).is_err()
        );
        let events = &state.borrow().events;
        assert!(events.iter().any(|e| e == "abort"));
        if !["metadata", "begin"].contains(&stage) {
            assert!(events.iter().any(|e| e == "delete"));
        }
    }
}
#[test]
fn missing_or_failed_critical_writers_never_create_a_snapshot_set() {
    for count in [0, 2, 3] {
        let (provider, state) = Provider::new();
        state.borrow_mut().writer_count = count;
        state.borrow_mut().writer_failed = count == 3;
        assert!(
            SnapshotSession::create(provider, &volumes(), &AtomicBool::new(false), |_| {}).is_err()
        );
        assert!(!state.borrow().events.iter().any(|e| e == "begin"));
    }
}
#[test]
fn invalid_namespaces_duplicate_volumes_and_cancellation_are_rejected() {
    for bad in [
        vec![VOLUMES[0].to_string(), VOLUMES[0].to_string()],
        vec![r"C:\".into()],
        vec![],
    ] {
        let (provider, state) = Provider::new();
        assert!(SnapshotSession::create(provider, &bad, &AtomicBool::new(false), |_| {}).is_err());
        assert!(state.borrow().events.is_empty());
    }
    let (provider, state) = Provider::new();
    state.borrow_mut().path_invalid = true;
    assert!(
        SnapshotSession::create(provider, &volumes(), &AtomicBool::new(false), |_| {}).is_err()
    );
    assert!(state.borrow().events.contains(&"delete".into()));
    let (provider, state) = Provider::new();
    let cancel = AtomicBool::new(false);
    assert!(
        SnapshotSession::create(provider, &volumes(), &cancel, |s| {
            if s == SnapshotStage::Snapshot {
                cancel.store(true, Ordering::Relaxed)
            }
        })
        .is_err()
    );
    assert!(state.borrow().events.contains(&"abort".into()));
}
#[test]
fn dropping_or_failing_completion_aborts_and_removes_ephemeral_snapshots() {
    for fail in [false, true] {
        let (provider, state) = Provider::new();
        let mut session =
            SnapshotSession::create(provider, &volumes(), &AtomicBool::new(false), |_| {}).unwrap();
        if fail {
            state.borrow_mut().fail = Some("complete".into());
            assert!(session.finish(&AtomicBool::new(false)).is_err());
        }
        drop(session);
        let events = &state.borrow().events;
        assert!(events.contains(&"abort".into()));
        assert!(events.contains(&"delete".into()));
    }
}
#[test]
fn writer_failure_after_capture_is_never_certified() {
    let (provider, state) = Provider::new();
    let mut session =
        SnapshotSession::create(provider, &volumes(), &AtomicBool::new(false), |_| {}).unwrap();
    state.borrow_mut().writer_failed = true;
    assert!(session.validate(&AtomicBool::new(false)).is_err());
    assert!(session.finish(&AtomicBool::new(false)).is_err());
}
#[test]
fn duplicate_provider_snapshot_ids_are_rejected_before_snapshot_creation() {
    let (provider, state) = Provider::new();
    state.borrow_mut().duplicate_snapshot = true;
    assert!(
        SnapshotSession::create(provider, &volumes(), &AtomicBool::new(false), |_| {}).is_err()
    );
    assert!(!state.borrow().events.contains(&"snapshot".into()));
    assert!(state.borrow().events.contains(&"delete".into()));
}

struct Reader {
    regions: BTreeMap<String, Vec<u8>>,
    session: SnapshotSession<Provider>,
}
impl RegionReader for Reader {
    fn read_region(&mut self, token: &str, offset: u64, buffer: &mut [u8]) -> Result<()> {
        let region = self
            .regions
            .get(token)
            .ok_or_else(|| invalid("Unknown fake region."))?;
        let a = offset as usize;
        buffer.copy_from_slice(&region[a..a + buffer.len()]);
        Ok(())
    }
    fn validate(&mut self, cancel: &AtomicBool) -> Result<()> {
        self.session.validate(cancel)
    }
}
fn disk_fixture() -> (
    layout::Layout,
    Vec<SnapshotRegion>,
    Reader,
    Rc<RefCell<State>>,
) {
    let d = tempfile::tempdir().unwrap();
    let path = common::fixture(&d, true, 512);
    let mut raw = std::fs::read(&path).unwrap();
    let original = image::inspect(&path, 512).unwrap();
    let ids = [
        [
            0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0, 0xa0, 0xc9, 0x3e, 0xc9,
            0x3b,
        ],
        [
            0x16, 0xe3, 0xc9, 0xe3, 0x5c, 0x0b, 0xb8, 0x4d, 0x81, 0x7d, 0xf9, 0x2d, 0xf, 0, 0x15,
            0xae,
        ],
        [
            0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26,
            0x99, 0xc7,
        ],
        [
            0xa4, 0xbb, 0x94, 0xde, 0xd1, 0x06, 0x40, 0x4d, 0xa1, 0x6a, 0xbf, 0xd5, 0x01, 0x79,
            0xd6, 0xac,
        ],
    ];
    let mut entries = vec![0; 128 * 128];
    for (i, id) in ids.into_iter().enumerate() {
        let e = &mut entries[i * 128..(i + 1) * 128];
        e[..16].copy_from_slice(&id);
        e[16..32].fill(i as u8 + 1);
        let first = 2048 + i as u64 * 4096;
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&(first + 2047).to_le_bytes());
    }
    let backup = original.layout.gpt.unwrap().backup_entries_lba as usize * 512;
    raw[1024..1024 + entries.len()].copy_from_slice(&entries);
    raw[backup..backup + entries.len()].copy_from_slice(&entries);
    let n = raw.len();
    for p in [512, n - 512] {
        raw[p + 88..p + 92].copy_from_slice(&crc32fast::hash(&entries).to_le_bytes());
        layout::header_crc(&mut raw[p..p + 512]);
    }
    let layout = layout::parse(&mut std::io::Cursor::new(&raw), raw.len() as u64, 512).unwrap();
    let (provider, state) = Provider::new();
    let session =
        SnapshotSession::create(provider, &volumes(), &AtomicBool::new(false), |_| {}).unwrap();
    let mappings = vec![
        SnapshotRegion {
            partition: 1,
            token: "locked-efi".into(),
            kind: RegionKind::LockedEfi,
        },
        SnapshotRegion {
            partition: 3,
            token: "snapshot-1".into(),
            kind: RegionKind::Snapshot,
        },
        SnapshotRegion {
            partition: 4,
            token: "snapshot-2".into(),
            kind: RegionKind::Snapshot,
        },
    ];
    let regions = mappings
        .iter()
        .enumerate()
        .map(|(i, m)| (m.token.clone(), vec![0x31 + i as u8; 1024 * 1024]))
        .collect();
    (layout, mappings, Reader { regions, session }, state)
}
fn info(layout: &layout::Layout) -> SourceInfo {
    SourceInfo {
        id: "live-disk-at-snapshot".into(),
        bytes: layout.bytes,
        sector: 512,
        storage_disk_ids: vec!["physical-source".into()],
        consistency: Consistency::Vss {
            set_id: "snapshot-set-one".into(),
            snapshot_ids: vec!["snapshot-1".into(), "snapshot-2".into()],
            writers_checked: 3,
        },
    }
}
#[test]
fn composite_disk_reads_snapshot_boundaries_metadata_and_zero_gaps() {
    let (layout, mappings, reader, _) = disk_fixture();
    let mut source = ConsistentDisk::new(layout.clone(), info(&layout), mappings, reader).unwrap();
    let mut buffer = vec![0; 1024];
    source.read_at(1024 * 1024 - 512, &mut buffer).unwrap();
    assert_eq!(&buffer[..512], &[0; 512]);
    assert_eq!(&buffer[512..], &[0x31; 512]);
    source.read_at(3 * 1024 * 1024, &mut buffer).unwrap();
    assert!(buffer.iter().all(|b| *b == 0));
    source.read_at(5 * 1024 * 1024, &mut buffer).unwrap();
    assert!(buffer.iter().all(|b| *b == 0x32));
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("vss-capture.img");
    let report =
        capture::capture_image(&mut source, &path, &AtomicBool::new(false), |_| {}).unwrap();
    assert!(report.partition_table_valid);
    assert_eq!(
        image::inspect(&path, 512).unwrap().layout.partitions,
        layout.partitions
    );
    source
        .reader
        .session
        .finish(&AtomicBool::new(false))
        .unwrap();
}
#[test]
fn incomplete_snapshot_mapping_or_unlocked_efi_cannot_form_a_live_source() {
    for failure in 0..4 {
        let (mut layout, mut mappings, reader, _) = disk_fixture();
        let mut source_info = info(&layout);
        match failure {
            0 => {
                mappings.pop();
            }
            1 => mappings[0].kind = RegionKind::Snapshot,
            2 => layout.partitions[2].role = Role::Other,
            3 => {
                source_info.consistency = Consistency::Vss {
                    set_id: "another-set".into(),
                    snapshot_ids: vec!["wrong-snapshot".into()],
                    writers_checked: 3,
                }
            }
            _ => unreachable!(),
        };
        assert!(ConsistentDisk::new(layout, source_info, mappings, reader).is_err());
    }
}
#[test]
fn capture_removes_its_new_partial_file_if_a_writer_fails() {
    let (layout, mappings, reader, state) = disk_fixture();
    let mut source = ConsistentDisk::new(layout.clone(), info(&layout), mappings, reader).unwrap();
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("failed.img");
    assert!(
        capture::capture_image(&mut source, &path, &AtomicBool::new(false), |p| {
            if p.phase == disk_migrator::migration::Phase::Copying {
                state.borrow_mut().writer_failed = true
            }
        })
        .is_err()
    );
    assert!(!path.exists());
    drop(source);
    assert!(state.borrow().events.contains(&"delete".into()));
}
