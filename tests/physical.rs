mod common;
use disk_migrator::{
    Error, Result, capture, image, invalid,
    layout::{self, Layout},
    migration::{Assessment, Phase},
    physical::*,
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};

struct Source {
    bytes: Rc<RefCell<Vec<u8>>>,
    layout: Layout,
    info: SourceInfo,
    fail_validation: bool,
}
impl Source {
    fn new(gpt: bool, sector: u32) -> Self {
        let d = tempfile::tempdir().unwrap();
        let path = common::fixture(&d, gpt, sector);
        let bytes = std::fs::read(&path).unwrap();
        let layout = image::inspect(&path, sector).unwrap().layout;
        Self {
            info: SourceInfo {
                id: "source-snapshot".into(),
                bytes: bytes.len() as u64,
                sector,
                storage_disk_ids: vec!["source-disk".into()],
                consistency: Consistency::OfflineImage {
                    file_id: "generated-image".into(),
                },
            },
            bytes: Rc::new(RefCell::new(bytes)),
            layout,
            fail_validation: false,
        }
    }
}
impl BlockSource for Source {
    fn info(&self) -> SourceInfo {
        self.info.clone()
    }
    fn layout(&self) -> &Layout {
        &self.layout
    }
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        buffer
            .copy_from_slice(&self.bytes.borrow()[offset as usize..offset as usize + buffer.len()]);
        Ok(())
    }
    fn validate(&mut self, _: &AtomicBool) -> Result<()> {
        if self.fail_validation {
            Err(invalid("Fake writer validation failure"))
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    Flush,
    PartialWrite,
    CorruptRead,
    HotSwap,
    Online,
}
struct State {
    bytes: Vec<u8>,
    info: TargetInfo,
    opens: usize,
    writes: usize,
    flushes: usize,
    fault: Fault,
}
struct Factory(Rc<RefCell<State>>);
struct Target(Rc<RefCell<State>>);
impl Factory {
    fn new(source: &Source, extra: u64) -> Self {
        Self(Rc::new(RefCell::new(State {
            bytes: vec![0x93; (source.info.bytes + extra) as usize],
            info: TargetInfo {
                disk: DiskIdentity {
                    number: 7,
                    hardware_id: "target-hardware-id".into(),
                    serial: "SSD-TEST-123".into(),
                    model: "Synthetic SSD".into(),
                    bytes: source.info.bytes + extra,
                    sector: source.info.sector,
                },
                offline: true,
                read_only: false,
                system_disk: false,
            },
            opens: 0,
            writes: 0,
            flushes: 0,
            fault: Fault::None,
        })))
    }
    fn plan(&mut self, source: &Source) -> PhysicalPlan {
        PhysicalPlan::build(source, self.inspect().unwrap()).unwrap()
    }
}
impl TargetFactory for Factory {
    type Target = Target;
    fn inspect(&mut self) -> Result<TargetInfo> {
        Ok(self.0.borrow().info.clone())
    }
    fn open_confirmed(&mut self, expected: &TargetInfo, confirmation: &str) -> Result<Target> {
        assert_eq!(&self.0.borrow().info, expected);
        assert_eq!(confirmation, confirmation_for(&expected.disk));
        self.0.borrow_mut().opens += 1;
        Ok(Target(self.0.clone()))
    }
}
impl BlockTarget for Target {
    fn info(&mut self) -> Result<TargetInfo> {
        let s = self.0.borrow();
        let mut info = s.info.clone();
        if s.writes > 0 {
            match s.fault {
                Fault::HotSwap => info.disk.hardware_id = "another-disk".into(),
                Fault::Online => info.offline = false,
                _ => {}
            }
        }
        Ok(info)
    }
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        let s = self.0.borrow();
        buffer.copy_from_slice(&s.bytes[offset as usize..offset as usize + buffer.len()]);
        if s.fault == Fault::CorruptRead && !buffer.is_empty() {
            buffer[0] ^= 0xff;
        }
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buffer: &[u8]) -> Result<()> {
        let mut s = self.0.borrow_mut();
        s.writes += 1;
        if s.fault == Fault::PartialWrite && s.writes == 2 {
            let n = buffer.len() / 2;
            s.bytes[offset as usize..offset as usize + n].copy_from_slice(&buffer[..n]);
            return Err(invalid("Injected partial device write."));
        }
        s.bytes[offset as usize..offset as usize + buffer.len()].copy_from_slice(buffer);
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        let mut s = self.0.borrow_mut();
        s.flushes += 1;
        if s.fault == Fault::Flush {
            s.fault = Fault::None;
            Err(invalid("Injected device flush failure."))
        } else {
            Ok(())
        }
    }
}
#[derive(Default)]
struct Store {
    checkpoint: Option<PhysicalCheckpoint>,
    saves: usize,
    fail_at: Option<usize>,
}
impl CheckpointStore for Store {
    fn load(&mut self) -> Result<Option<PhysicalCheckpoint>> {
        Ok(self.checkpoint.clone())
    }
    fn save(&mut self, c: &PhysicalCheckpoint) -> Result<()> {
        self.saves += 1;
        if self.fail_at == Some(self.saves) {
            return Err(invalid("Injected checkpoint persistence failure."));
        }
        self.checkpoint = Some(c.clone());
        Ok(())
    }
}
fn request<'a>(
    plan: &'a PhysicalPlan,
    confirmation: &'a str,
    mode: CopyMode,
    cancel: &'a AtomicBool,
) -> CopyRequest<'a> {
    CopyRequest {
        plan,
        confirmation,
        mode,
        cancel,
    }
}
fn copy(
    source: &mut Source,
    factory: &mut Factory,
    store: &mut Store,
    plan: &PhysicalPlan,
    mode: CopyMode,
) -> Result<PhysicalVerification> {
    execute(
        source,
        factory,
        store,
        request(plan, &plan.confirmation(), mode, &AtomicBool::new(false)),
        |_| {},
    )
}

#[test]
fn physical_copy_verifies_all_bytes_and_relocates_gpt_for_both_sector_sizes() {
    for sector in [512, 4096] {
        for gpt in [false, true] {
            for extra in [0, sector as u64, 1024 * 1024] {
                let mut source = Source::new(gpt, sector);
                let before = source.bytes.borrow().clone();
                let mut target = Factory::new(&source, extra);
                let plan = target.plan(&source);
                let report = copy(
                    &mut source,
                    &mut target,
                    &mut Store::default(),
                    &plan,
                    CopyMode::Start,
                )
                .unwrap();
                assert!(report.data_matches && report.partition_table_valid);
                assert_eq!(report.verified_bytes, plan.target.disk.bytes);
                assert_eq!(report.bootability, Assessment::NotTested);
                assert_eq!(report.filesystem_consistency, Assessment::NotTested);
                let state = target.0.borrow();
                let parsed = layout::parse(
                    &mut std::io::Cursor::new(&state.bytes),
                    state.info.disk.bytes,
                    sector,
                )
                .unwrap();
                assert_eq!(parsed.partitions, source.layout.partitions);
                assert_eq!(*source.bytes.borrow(), before);
            }
        }
    }
}
#[test]
fn wrong_confirmation_and_changed_target_never_open_a_write_handle() {
    let mut source = Source::new(true, 512);
    let mut factory = Factory::new(&source, 0);
    let plan = factory.plan(&source);
    let mut store = Store::default();
    assert!(
        execute(
            &mut source,
            &mut factory,
            &mut store,
            request(
                &plan,
                "ERASE PhysicalDrive0 WRONG",
                CopyMode::Start,
                &AtomicBool::new(false)
            ),
            |_| {}
        )
        .is_err()
    );
    assert_eq!(factory.0.borrow().opens, 0);
    factory.0.borrow_mut().info.disk.hardware_id = "replacement-device".into();
    assert!(
        copy(
            &mut source,
            &mut factory,
            &mut store,
            &plan,
            CopyMode::Start
        )
        .is_err()
    );
    assert_eq!(factory.0.borrow().opens, 0);
}
#[test]
fn refuses_system_online_read_only_same_disk_and_incompatible_geometry() {
    let source = Source::new(true, 512);
    for i in 0..7 {
        let mut factory = Factory::new(&source, 0);
        let mut info = factory.inspect().unwrap();
        match i {
            0 => info.system_disk = true,
            1 => info.offline = false,
            2 => info.read_only = true,
            3 => info.disk.hardware_id = "source-disk".into(),
            4 => info.disk.sector = 4096,
            5 => info.disk.bytes -= 512,
            6 => info.disk.serial.clear(),
            _ => unreachable!(),
        };
        assert!(PhysicalPlan::build(&source, info).is_err());
        assert_eq!(factory.0.borrow().opens, 0);
    }
}
#[test]
fn cancellation_checkpoints_only_durable_chunks_and_resumes_after_dirty_partial_write() {
    let mut source = Source::new(true, 512);
    let mut factory = Factory::new(&source, 1024 * 1024);
    let plan = factory.plan(&source);
    let mut store = Store::default();
    factory.0.borrow_mut().fault = Fault::PartialWrite;
    assert!(
        copy(
            &mut source,
            &mut factory,
            &mut store,
            &plan,
            CopyMode::Start
        )
        .is_err()
    );
    let checkpoint = serde_json::to_value(store.checkpoint.as_ref().unwrap()).unwrap();
    assert_eq!(checkpoint["copied"], 4 * 1024 * 1024);
    factory.0.borrow_mut().fault = Fault::None;
    copy(
        &mut source,
        &mut factory,
        &mut store,
        &plan,
        CopyMode::Resume,
    )
    .unwrap();
    let mut source = Source::new(false, 4096);
    let mut factory = Factory::new(&source, 0);
    let plan = factory.plan(&source);
    let mut store = Store::default();
    let cancel = AtomicBool::new(false);
    let result = execute(
        &mut source,
        &mut factory,
        &mut store,
        request(&plan, &plan.confirmation(), CopyMode::Start, &cancel),
        |p| {
            if p.phase == Phase::Copying {
                cancel.store(true, Ordering::Relaxed)
            }
        },
    );
    assert!(matches!(result, Err(Error::Cancelled)));
    copy(
        &mut source,
        &mut factory,
        &mut store,
        &plan,
        CopyMode::Resume,
    )
    .unwrap();
}
#[test]
fn checkpoint_never_advances_past_a_failed_flush_or_save() {
    for fail_flush in [false, true] {
        let mut source = Source::new(true, 512);
        let mut factory = Factory::new(&source, 0);
        let plan = factory.plan(&source);
        let mut store = Store::default();
        if fail_flush {
            factory.0.borrow_mut().fault = Fault::Flush;
        } else {
            store.fail_at = Some(2);
        }
        assert!(
            copy(
                &mut source,
                &mut factory,
                &mut store,
                &plan,
                CopyMode::Start
            )
            .is_err()
        );
        assert_eq!(
            serde_json::to_value(store.checkpoint.as_ref().unwrap()).unwrap()["copied"],
            0
        );
        store.fail_at = None;
        copy(
            &mut source,
            &mut factory,
            &mut store,
            &plan,
            CopyMode::Resume,
        )
        .unwrap();
    }
}
#[test]
fn hardware_swap_or_online_transition_stops_before_the_next_chunk() {
    for fault in [Fault::HotSwap, Fault::Online] {
        let mut source = Source::new(true, 512);
        let mut factory = Factory::new(&source, 0);
        let plan = factory.plan(&source);
        factory.0.borrow_mut().fault = fault;
        assert!(
            copy(
                &mut source,
                &mut factory,
                &mut Store::default(),
                &plan,
                CopyMode::Start
            )
            .is_err()
        );
        assert_eq!(factory.0.borrow().writes, 1);
    }
}
#[test]
fn rejects_corrupt_checkpoint_or_committed_prefix_without_further_writes() {
    for corrupt_checkpoint in [false, true] {
        let mut source = Source::new(false, 512);
        let mut factory = Factory::new(&source, 0);
        let plan = factory.plan(&source);
        let mut store = Store::default();
        let cancel = AtomicBool::new(false);
        let _ = execute(
            &mut source,
            &mut factory,
            &mut store,
            request(&plan, &plan.confirmation(), CopyMode::Start, &cancel),
            |p| {
                if p.phase == Phase::Copying {
                    cancel.store(true, Ordering::Relaxed)
                }
            },
        );
        if corrupt_checkpoint {
            let mut value = serde_json::to_value(store.checkpoint.as_ref().unwrap()).unwrap();
            value["checksum"] = serde_json::json!("broken");
            store.checkpoint = Some(serde_json::from_value(value).unwrap());
        } else {
            factory.0.borrow_mut().bytes[1024 * 1024] ^= 17;
        }
        let before = factory.0.borrow().writes;
        assert!(
            copy(
                &mut source,
                &mut factory,
                &mut store,
                &plan,
                CopyMode::Resume
            )
            .is_err()
        );
        assert_eq!(factory.0.borrow().writes, before);
        copy(
            &mut source,
            &mut factory,
            &mut store,
            &plan,
            CopyMode::Restart,
        )
        .unwrap();
    }
}
#[test]
fn a_new_vss_set_cannot_resume_an_old_snapshot_even_with_identical_bytes() {
    let mut source = Source::new(true, 512);
    source.info.consistency = Consistency::Vss {
        set_id: "set-one".into(),
        snapshot_ids: vec!["snapshot-one".into()],
        writers_checked: 3,
    };
    let mut factory = Factory::new(&source, 0);
    let plan = factory.plan(&source);
    let mut store = Store::default();
    let cancel = AtomicBool::new(false);
    let _ = execute(
        &mut source,
        &mut factory,
        &mut store,
        request(&plan, &plan.confirmation(), CopyMode::Start, &cancel),
        |p| {
            if p.phase == Phase::Copying {
                cancel.store(true, Ordering::Relaxed)
            }
        },
    );
    source.info.consistency = Consistency::Vss {
        set_id: "set-two".into(),
        snapshot_ids: vec!["snapshot-two".into()],
        writers_checked: 3,
    };
    let plan = factory.plan(&source);
    let opens = factory.0.borrow().opens;
    assert!(
        copy(
            &mut source,
            &mut factory,
            &mut store,
            &plan,
            CopyMode::Resume
        )
        .is_err()
    );
    assert_eq!(factory.0.borrow().opens, opens);
}
#[test]
fn verifies_destination_and_detects_source_changes_during_copy() {
    let mut source = Source::new(true, 512);
    let mut factory = Factory::new(&source, 0);
    let plan = factory.plan(&source);
    factory.0.borrow_mut().fault = Fault::CorruptRead;
    assert!(
        copy(
            &mut source,
            &mut factory,
            &mut Store::default(),
            &plan,
            CopyMode::Start
        )
        .is_err()
    );
    let mut source = Source::new(true, 512);
    let mut factory = Factory::new(&source, 0);
    let plan = factory.plan(&source);
    let bytes = source.bytes.clone();
    assert!(
        execute(
            &mut source,
            &mut factory,
            &mut Store::default(),
            request(
                &plan,
                &plan.confirmation(),
                CopyMode::Start,
                &AtomicBool::new(false)
            ),
            |p| if p.phase == Phase::Copying && p.done == 4 * 1024 * 1024 {
                bytes.borrow_mut()[1024 * 1024] ^= 1;
            }
        )
        .is_err()
    );
}
#[test]
fn source_validation_or_cancellation_before_copy_never_opens_a_write_handle() {
    for cancelled in [false, true] {
        let mut source = Source::new(true, 512);
        let mut factory = Factory::new(&source, 0);
        let plan = factory.plan(&source);
        source.fail_validation = !cancelled;
        assert!(
            execute(
                &mut source,
                &mut factory,
                &mut Store::default(),
                request(
                    &plan,
                    &plan.confirmation(),
                    CopyMode::Start,
                    &AtomicBool::new(cancelled)
                ),
                |_| {}
            )
            .is_err()
        );
        assert_eq!(factory.0.borrow().opens, 0);
    }
}
#[test]
fn capture_writes_and_verifies_a_generated_image_without_overwriting_existing_files() {
    let mut source = Source::new(true, 4096);
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("captured.img");
    let report =
        capture::capture_image(&mut source, &path, &AtomicBool::new(false), |_| {}).unwrap();
    assert!(report.partition_table_valid);
    assert_eq!(
        report.sha256,
        disk_migrator::digest(&std::fs::read(&path).unwrap())
    );
    assert!(capture::capture_image(&mut source, &path, &AtomicBool::new(false), |_| {}).is_err());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), source.info.bytes);
    let cancelled = d.path().join("cancelled.img");
    let flag = AtomicBool::new(false);
    assert!(
        capture::capture_image(&mut source, &cancelled, &flag, |_| flag
            .store(true, Ordering::Relaxed))
        .is_err()
    );
    assert!(!cancelled.exists());
}
#[cfg(windows)]
#[test]
fn bounded_parsers_reject_malformed_driver_buffers() {
    use disk_migrator::windows::device::{parse_device_identifiers, parse_extents};
    assert!(parse_extents(&[0; 7]).is_err());
    let mut extents = vec![0; 32];
    extents[..4].copy_from_slice(&1u32.to_le_bytes());
    extents[8..12].copy_from_slice(&9u32.to_le_bytes());
    extents[16..24].copy_from_slice(&4096u64.to_le_bytes());
    extents[24..32].copy_from_slice(&8192u64.to_le_bytes());
    assert_eq!(parse_extents(&extents).unwrap(), vec![(9, 4096, 8192)]);
    let mut ids = vec![0; 32];
    ids[8..12].copy_from_slice(&1u32.to_le_bytes());
    ids[20..22].copy_from_slice(&4u16.to_le_bytes());
    ids[28..32].copy_from_slice(b"test");
    assert_eq!(parse_device_identifiers(&ids).unwrap().len(), 1);
    ids[20..22].copy_from_slice(&100u16.to_le_bytes());
    assert!(parse_device_identifiers(&ids).is_err());
}
#[test]
fn file_checkpoint_roundtrip_rejects_corruption_before_device_write_access() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("checkpoint.json");
    let mut source = Source::new(true, 512);
    let mut factory = Factory::new(&source, 0);
    let plan = factory.plan(&source);
    let cancel = AtomicBool::new(false);
    let mut store = FileCheckpoint::new(path.clone());
    let result = execute(
        &mut source,
        &mut factory,
        &mut store,
        request(&plan, &plan.confirmation(), CopyMode::Start, &cancel),
        |p| {
            if p.phase == Phase::Copying {
                cancel.store(true, Ordering::Relaxed)
            }
        },
    );
    assert!(matches!(result, Err(Error::Cancelled)));
    let mut resumed = FileCheckpoint::new(path.clone());
    execute(
        &mut source,
        &mut factory,
        &mut resumed,
        request(
            &plan,
            &plan.confirmation(),
            CopyMode::Resume,
            &AtomicBool::new(false),
        ),
        |_| {},
    )
    .unwrap();
    std::fs::write(&path, b"corrupt state").unwrap();
    let writes = factory.0.borrow().writes;
    let opens = factory.0.borrow().opens;
    assert!(
        execute(
            &mut source,
            &mut factory,
            &mut resumed,
            request(
                &plan,
                &plan.confirmation(),
                CopyMode::Resume,
                &AtomicBool::new(false)
            ),
            |_| {}
        )
        .is_err()
    );
    assert_eq!(factory.0.borrow().writes, writes);
    assert_eq!(factory.0.borrow().opens, opens);
}
