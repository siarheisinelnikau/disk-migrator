//! Confirmed physical-copy coordinator. All OS access lives behind narrow adapters.
use crate::{
    Error, Result, digest, invalid,
    layout::{self, Layout},
    migration::{Assessment, Phase, Progress, atomic_json},
    plan::{Patch, partition_patches},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

const CHUNK: usize = 4 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskIdentity {
    pub number: u32,
    pub hardware_id: String,
    pub serial: String,
    pub model: String,
    pub bytes: u64,
    pub sector: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetInfo {
    pub disk: DiskIdentity,
    pub offline: bool,
    pub read_only: bool,
    pub system_disk: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Consistency {
    OfflineImage {
        file_id: String,
    },
    Vss {
        set_id: String,
        snapshot_ids: Vec<String>,
        writers_checked: usize,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub id: String,
    pub bytes: u64,
    pub sector: u32,
    pub storage_disk_ids: Vec<String>,
    pub consistency: Consistency,
}
pub trait BlockSource {
    fn info(&self) -> SourceInfo;
    fn layout(&self) -> &Layout;
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()>;
    fn validate(&mut self, cancel: &AtomicBool) -> Result<()>;
}
pub trait BlockTarget {
    fn info(&mut self) -> Result<TargetInfo>;
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()>;
    fn write_at(&mut self, offset: u64, buffer: &[u8]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
}
pub trait TargetFactory {
    type Target: BlockTarget;
    fn inspect(&mut self) -> Result<TargetInfo>;
    /// Must independently revalidate offline/system/identity guards before acquiring write access.
    fn open_confirmed(&mut self, expected: &TargetInfo, confirmation: &str)
    -> Result<Self::Target>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalPlan {
    pub source: SourceInfo,
    pub target: TargetInfo,
    pub layout: Layout,
    pub patches: Vec<Patch>,
    pub id: String,
}
impl PhysicalPlan {
    pub fn build(source: &impl BlockSource, target: TargetInfo) -> Result<Self> {
        let info = source.info();
        let layout = source.layout().clone();
        validate_target(&info, &target)?;
        if info.bytes != layout.bytes || info.sector != layout.sector_size {
            return Err(invalid(
                "Source geometry disagrees with the parsed partition table.",
            ));
        }
        let patches = partition_patches(&layout, target.disk.bytes)?;
        let mut plan = Self {
            source: info,
            target,
            layout,
            patches,
            id: String::new(),
        };
        plan.id = plan.calculated_id()?;
        Ok(plan)
    }
    pub fn calculated_id(&self) -> Result<String> {
        let mut p = self.clone();
        p.id.clear();
        Ok(digest(&serde_json::to_vec(&p)?))
    }
    pub fn confirmation(&self) -> String {
        confirmation_for(&self.target.disk)
    }
    fn overlay(&self, offset: u64, buffer: &mut [u8]) {
        for p in &self.patches {
            let a = offset.max(p.offset);
            let b = (offset + buffer.len() as u64).min(p.offset + p.bytes.len() as u64);
            if a < b {
                buffer[(a - offset) as usize..(b - offset) as usize]
                    .copy_from_slice(&p.bytes[(a - p.offset) as usize..(b - p.offset) as usize]);
            }
        }
    }
}
pub fn confirmation_for(disk: &DiskIdentity) -> String {
    format!("ERASE PhysicalDrive{} {}", disk.number, disk.serial)
}
pub fn validate_target(source: &SourceInfo, target: &TargetInfo) -> Result<()> {
    validate_target_disk(
        &source.storage_disk_ids,
        source.bytes,
        source.sector,
        target,
    )?;
    if source.id.is_empty()
        || target.disk.hardware_id.is_empty()
        || target.disk.serial.trim().is_empty()
    {
        return Err(invalid(
            "A stable source identity and target hardware ID/serial are required.",
        ));
    }
    if target.system_disk {
        return Err(invalid(
            "Refusing to overwrite a disk containing the running Windows installation, application, checkpoint, or source.",
        ));
    }
    if !target.offline || target.read_only {
        return Err(invalid(
            "The destination must already be offline and writable. Disk Migrator will not change disk online/read-only attributes.",
        ));
    }
    if source.storage_disk_ids.contains(&target.disk.hardware_id) {
        return Err(invalid(
            "Source and destination share the same underlying physical disk.",
        ));
    }
    layout::valid_size(source.bytes, source.sector)?;
    layout::valid_size(target.disk.bytes, target.disk.sector)?;
    if source.sector != target.disk.sector {
        return Err(invalid(
            "Source and destination logical sectors must match; 512/4096 translation is unsupported.",
        ));
    }
    if target.disk.bytes < source.bytes {
        return Err(invalid(
            "Destination is smaller than source. Filesystem shrinking is unsupported.",
        ));
    }
    if let Consistency::Vss {
        set_id,
        snapshot_ids,
        writers_checked,
    } = &source.consistency
        && (set_id.is_empty()
            || snapshot_ids.is_empty()
            || *writers_checked == 0
            || snapshot_ids.iter().collect::<BTreeSet<_>>().len() != snapshot_ids.len())
    {
        return Err(invalid("VSS consistency evidence is incomplete."));
    }
    Ok(())
}
pub fn validate_target_disk(
    source_disks: &[String],
    source_bytes: u64,
    source_sector: u32,
    target: &TargetInfo,
) -> Result<()> {
    if target.disk.hardware_id.is_empty()
        || target.disk.serial.trim().is_empty()
        || target.system_disk
        || target.read_only
        || !target.offline
    {
        return Err(invalid(
            "Destination must have a stable hardware identity, be offline and writable, and contain none of the running Windows, application, checkpoint or source paths.",
        ));
    }
    if source_disks.contains(&target.disk.hardware_id) {
        return Err(invalid(
            "Source and destination are on the same physical disk.",
        ));
    }
    layout::valid_size(source_bytes, source_sector)?;
    layout::valid_size(target.disk.bytes, target.disk.sector)?;
    if source_bytes > target.disk.bytes || source_sector != target.disk.sector {
        return Err(invalid(
            "Destination is too small or has an incompatible logical sector size.",
        ));
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyMode {
    Start,
    Resume,
    Restart,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalCheckpoint {
    version: u32,
    plan_id: String,
    source_sha256: String,
    target_id: String,
    copied: u64,
    checksum: String,
}
impl PhysicalCheckpoint {
    fn seal(&mut self) -> Result<()> {
        self.checksum.clear();
        self.checksum = digest(&serde_json::to_vec(self)?);
        Ok(())
    }
    fn valid(&self) -> Result<()> {
        let mut p = self.clone();
        let expected = p.checksum.clone();
        p.seal()?;
        if self.version != 1 || p.checksum != expected {
            return Err(invalid(
                "Physical migration checkpoint is corrupt or unsupported.",
            ));
        }
        Ok(())
    }
}
pub trait CheckpointStore {
    fn load(&mut self) -> Result<Option<PhysicalCheckpoint>>;
    fn save(&mut self, checkpoint: &PhysicalCheckpoint) -> Result<()>;
}
pub struct FileCheckpoint {
    path: PathBuf,
}
impl FileCheckpoint {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl CheckpointStore for FileCheckpoint {
    fn load(&mut self) -> Result<Option<PhysicalCheckpoint>> {
        crate::image::validate_state_path(&self.path)?;
        match std::fs::File::open(&self.path) {
            Ok(f) => {
                if f.metadata()?.len() > 16 * 1024 {
                    return Err(invalid("Physical checkpoint is oversized."));
                }
                let p: PhysicalCheckpoint = serde_json::from_reader(f)?;
                p.valid()?;
                Ok(Some(p))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn save(&mut self, c: &PhysicalCheckpoint) -> Result<()> {
        crate::image::validate_state_path(&self.path)?;
        atomic_json(&self.path, &serde_json::to_vec(c)?)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhysicalVerification {
    pub source_sha256: String,
    pub verified_bytes: u64,
    pub target: DiskIdentity,
    pub source_consistency: Consistency,
    pub data_matches: bool,
    pub partition_table_valid: bool,
    pub filesystem_consistency: Assessment,
    pub bootability: Assessment,
}

pub struct CopyRequest<'a> {
    pub plan: &'a PhysicalPlan,
    pub confirmation: &'a str,
    pub mode: CopyMode,
    pub cancel: &'a AtomicBool,
}
pub fn execute<S: BlockSource, F: TargetFactory, C: CheckpointStore>(
    source: &mut S,
    factory: &mut F,
    store: &mut C,
    request: CopyRequest<'_>,
    mut notify: impl FnMut(Progress),
) -> Result<PhysicalVerification> {
    let CopyRequest {
        plan,
        confirmation,
        mode,
        cancel,
    } = request;
    if confirmation != plan.confirmation() {
        return Err(invalid(
            "Exact ERASE confirmation naming the physical disk and serial is required.",
        ));
    }
    let inspected = factory.inspect()?;
    let rebuilt = PhysicalPlan::build(source, inspected)?;
    if rebuilt.id != plan.id || plan.calculated_id()? != plan.id {
        return Err(invalid(
            "Source, target identity, offline state or migration plan changed since review.",
        ));
    }
    source.validate(cancel)?;
    let previous = if mode == CopyMode::Restart {
        None
    } else {
        store.load()?
    };
    match (mode, &previous) {
        (CopyMode::Start, Some(_)) => {
            return Err(invalid(
                "A physical checkpoint exists. Use Resume, or explicitly Restart with a new confirmation.",
            ));
        }
        (CopyMode::Resume, None) => {
            return Err(invalid("No physical migration checkpoint exists."));
        }
        _ => {}
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    let mut done = 0;
    while done < plan.source.bytes {
        cancelled(cancel)?;
        let n = (plan.source.bytes - done).min(CHUNK as u64) as usize;
        source.read_at(done, &mut buffer[..n])?;
        hash.update(&buffer[..n]);
        done += n as u64;
        notify(Progress {
            phase: Phase::Fingerprinting,
            done,
            total: plan.source.bytes,
        });
    }
    let sha = format!("{:x}", hash.finalize());
    source.validate(cancel)?;
    let total = plan.target.disk.bytes;
    if let Some(c) = &previous {
        c.valid()?;
        if c.plan_id != plan.id
            || c.source_sha256 != sha
            || c.target_id != plan.target.disk.hardware_id
            || c.copied > total
            || c.copied != total && !c.copied.is_multiple_of(CHUNK as u64)
        {
            return Err(invalid(
                "Checkpoint is bound to another source/snapshot set, destination or plan. Live snapshots cannot be replaced during resume.",
            ));
        }
    }
    cancelled(cancel)?;
    let mut target = factory.open_confirmed(&plan.target, confirmation)?;
    revalidate(&mut target, &plan.target)?;
    let mut checkpoint = previous.unwrap_or(PhysicalCheckpoint {
        version: 1,
        plan_id: plan.id.clone(),
        source_sha256: sha.clone(),
        target_id: plan.target.disk.hardware_id.clone(),
        copied: 0,
        checksum: String::new(),
    });
    if mode != CopyMode::Resume {
        checkpoint.seal()?;
        store.save(&checkpoint)?;
    }
    let mut actual = vec![0; CHUNK];
    done = 0;
    while done < checkpoint.copied {
        cancelled(cancel)?;
        let n = (checkpoint.copied - done).min(CHUNK as u64) as usize;
        expected(source, plan, done, &mut buffer[..n])?;
        revalidate(&mut target, &plan.target)?;
        target.read_at(done, &mut actual[..n])?;
        if actual[..n] != buffer[..n] {
            return Err(invalid(format!(
                "Committed destination differs at byte {done}. No further writes were performed."
            )));
        }
        done += n as u64;
        notify(Progress {
            phase: Phase::Resuming,
            done,
            total: checkpoint.copied,
        });
    }
    while done < total {
        cancelled(cancel)?;
        let n = (total - done).min(CHUNK as u64) as usize;
        expected(source, plan, done, &mut buffer[..n])?;
        revalidate(&mut target, &plan.target)?;
        target.write_at(done, &buffer[..n])?;
        target.flush()?;
        done += n as u64;
        checkpoint.copied = done;
        checkpoint.seal()?;
        store.save(&checkpoint)?;
        notify(Progress {
            phase: Phase::Copying,
            done,
            total,
        });
    }
    target.flush()?;
    source.validate(cancel)?;
    done = 0;
    let mut verify_source = Sha256::new();
    while done < total {
        cancelled(cancel)?;
        let n = (total - done).min(CHUNK as u64) as usize;
        buffer[..n].fill(0);
        let source_n = plan.source.bytes.saturating_sub(done).min(n as u64) as usize;
        if source_n > 0 {
            source.read_at(done, &mut buffer[..source_n])?;
            verify_source.update(&buffer[..source_n]);
        }
        plan.overlay(done, &mut buffer[..n]);
        revalidate(&mut target, &plan.target)?;
        target.read_at(done, &mut actual[..n])?;
        if digest(&actual[..n]) != digest(&buffer[..n]) {
            return Err(invalid(format!(
                "Physical destination verification failed at byte {done}."
            )));
        }
        done += n as u64;
        notify(Progress {
            phase: Phase::Verifying,
            done,
            total,
        });
    }
    if format!("{:x}", verify_source.finalize()) != sha {
        return Err(invalid("Consistent source changed during migration."));
    }
    source.validate(cancel)?;
    revalidate(&mut target, &plan.target)?;
    let mut reader = TargetReader {
        target: &mut target,
        offset: 0,
    };
    let layout = layout::parse(&mut reader, total, plan.source.sector)?;
    if layout.partitions != plan.layout.partitions || layout.scheme != plan.layout.scheme {
        return Err(invalid(
            "Copied partition table does not match the reviewed plan.",
        ));
    }
    Ok(PhysicalVerification {
        source_sha256: sha,
        verified_bytes: total,
        target: plan.target.disk.clone(),
        source_consistency: plan.source.consistency.clone(),
        data_matches: true,
        partition_table_valid: true,
        filesystem_consistency: Assessment::NotTested,
        bootability: Assessment::NotTested,
    })
}
fn expected(
    source: &mut impl BlockSource,
    plan: &PhysicalPlan,
    offset: u64,
    buffer: &mut [u8],
) -> Result<()> {
    buffer.fill(0);
    let n = plan
        .source
        .bytes
        .saturating_sub(offset)
        .min(buffer.len() as u64) as usize;
    if n > 0 {
        source.read_at(offset, &mut buffer[..n])?;
    }
    plan.overlay(offset, buffer);
    Ok(())
}
fn revalidate(target: &mut impl BlockTarget, expected: &TargetInfo) -> Result<()> {
    if target.info()? != *expected {
        return Err(invalid(
            "Destination hardware identity, geometry or offline state changed. Copy stopped.",
        ));
    }
    Ok(())
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
struct TargetReader<'a, T: BlockTarget> {
    target: &'a mut T,
    offset: u64,
}
impl<T: BlockTarget> std::io::Read for TargetReader<'_, T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.target
            .read_at(self.offset, buf)
            .map_err(std::io::Error::other)?;
        self.offset += buf.len() as u64;
        Ok(buf.len())
    }
}
impl<T: BlockTarget> std::io::Seek for TargetReader<'_, T> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match pos {
            std::io::SeekFrom::Start(n) => self.offset = n,
            _ => return Err(std::io::Error::other("Only absolute seeks are supported.")),
        }
        Ok(self.offset)
    }
}
