//! Writer-coordinated VSS lifecycle, shared by the Windows COM adapter and fake providers.
use crate::{Error, Result, invalid};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotVolume {
    pub original_volume: String,
    pub snapshot_id: String,
    pub device_path: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSet {
    pub id: String,
    pub volumes: Vec<SnapshotVolume>,
    pub writers_checked: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterStatus {
    pub id: String,
    pub name: String,
    pub state: u32,
    pub failure: i32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotStage {
    Initialize,
    Metadata,
    Begin,
    Prepare,
    Snapshot,
    WriterStatus,
    Complete,
    Cleanup,
}

pub trait SnapshotProvider {
    fn initialize(&mut self) -> Result<()>;
    fn gather_metadata(&mut self, cancel: &AtomicBool) -> Result<()>;
    fn start_set(&mut self) -> Result<String>;
    fn add_volume(&mut self, volume: &str) -> Result<String>;
    fn prepare(&mut self, cancel: &AtomicBool) -> Result<()>;
    fn take_snapshot(&mut self, cancel: &AtomicBool) -> Result<()>;
    fn writer_status(&mut self, cancel: &AtomicBool) -> Result<Vec<WriterStatus>>;
    fn device_path(&mut self, snapshot_id: &str) -> Result<String>;
    fn complete(&mut self, cancel: &AtomicBool) -> Result<()>;
    fn abort(&mut self) -> Result<()>;
    fn delete_set(&mut self, set_id: &str) -> Result<()>;
}
pub fn check_writers(writers: &[WriterStatus], after_snapshot: bool) -> Result<()> {
    if writers.is_empty() {
        return Err(invalid(
            "VSS returned no writers; a Windows-consistent source cannot be certified.",
        ));
    }
    for w in writers {
        if w.failure < 0 || !(w.state == 1 || after_snapshot && w.state == 5) {
            return Err(invalid(format!(
                "VSS writer '{}' is not healthy (state {}, HRESULT 0x{:08x}).",
                w.name, w.state, w.failure as u32
            )));
        }
    }
    for required in [
        "e8132975-6f93-4464-a53e-1050253ae220",
        "afbab4a2-367d-4d15-a586-71dbb18f8485",
        "be000cbe-11fe-4426-9c58-531aa6355fc4",
    ] {
        if !writers
            .iter()
            .any(|w| w.id.trim_matches(['{', '}']).eq_ignore_ascii_case(required))
        {
            return Err(invalid(format!(
                "Required Windows system/registry/ASR VSS writer {required} is missing."
            )));
        }
    }
    Ok(())
}
pub struct SnapshotSession<P: SnapshotProvider> {
    provider: P,
    pub set: SnapshotSet,
    closed: bool,
}
impl<P: SnapshotProvider> SnapshotSession<P> {
    pub fn create(
        mut provider: P,
        volumes: &[String],
        cancel: &AtomicBool,
        mut notify: impl FnMut(SnapshotStage),
    ) -> Result<Self> {
        if volumes.is_empty()
            || volumes.len() > 64
            || volumes.iter().any(|v| !valid_volume(v))
            || volumes.iter().collect::<BTreeSet<_>>().len() != volumes.len()
        {
            return Err(invalid(
                "VSS requires 1–64 unique canonical local volume GUID paths.",
            ));
        }
        let mut set_id = None;
        let result = (|| -> Result<SnapshotSet> {
            cancelled(cancel)?;
            notify(SnapshotStage::Initialize);
            provider.initialize()?;
            notify(SnapshotStage::Metadata);
            provider.gather_metadata(cancel)?;
            check_writers(&provider.writer_status(cancel)?, false)?;
            cancelled(cancel)?;
            notify(SnapshotStage::Begin);
            let id = provider.start_set()?;
            set_id = Some(id.clone());
            if id.is_empty() {
                return Err(invalid("VSS returned an empty snapshot-set identity."));
            }
            let mut snapshots = Vec::new();
            let mut snapshot_ids = BTreeSet::new();
            for volume in volumes {
                cancelled(cancel)?;
                let snapshot_id = provider.add_volume(volume)?;
                if snapshot_id.is_empty() || !snapshot_ids.insert(snapshot_id.clone()) {
                    return Err(invalid(
                        "VSS returned a missing or duplicate snapshot identity.",
                    ));
                }
                snapshots.push(SnapshotVolume {
                    original_volume: volume.clone(),
                    snapshot_id,
                    device_path: String::new(),
                });
            }
            notify(SnapshotStage::Prepare);
            provider.prepare(cancel)?;
            notify(SnapshotStage::Snapshot);
            provider.take_snapshot(cancel)?;
            notify(SnapshotStage::WriterStatus);
            let writers = provider.writer_status(cancel)?;
            check_writers(&writers, true)?;
            for snapshot in &mut snapshots {
                let path = provider.device_path(&snapshot.snapshot_id)?;
                if !valid_snapshot_path(&path) {
                    return Err(invalid("VSS returned a non-snapshot device namespace."));
                }
                snapshot.device_path = path;
            }
            cancelled(cancel)?;
            Ok(SnapshotSet {
                id,
                volumes: snapshots,
                writers_checked: writers.len(),
            })
        })();
        match result {
            Ok(set) => Ok(Self {
                provider,
                set,
                closed: false,
            }),
            Err(error) => {
                let abort = provider.abort();
                let delete = set_id
                    .as_ref()
                    .map(|id| provider.delete_set(id))
                    .transpose();
                if let Err(cleanup) = abort.and(delete.map(|_| ())) {
                    return Err(invalid(format!(
                        "{error}; VSS cleanup also failed: {cleanup}"
                    )));
                }
                Err(error)
            }
        }
    }
    pub fn validate(&mut self, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        check_writers(&self.provider.writer_status(cancel)?, true)
    }
    pub fn finish(&mut self, cancel: &AtomicBool) -> Result<()> {
        if self.closed {
            return Err(invalid("Snapshot session is already closed."));
        }
        self.validate(cancel)?;
        self.provider.complete(cancel)?;
        check_writers(&self.provider.writer_status(cancel)?, true)?;
        self.provider.delete_set(&self.set.id)?;
        self.closed = true;
        Ok(())
    }
    pub fn close_failed(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let abort = self.provider.abort();
        let delete = self.provider.delete_set(&self.set.id);
        self.closed = abort.is_ok() && delete.is_ok();
        abort.and(delete)
    }
}
impl<P: SnapshotProvider> Drop for SnapshotSession<P> {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.provider.abort();
            let _ = self.provider.delete_set(&self.set.id);
        }
    }
}
pub fn valid_volume(path: &str) -> bool {
    let Some(id) = path
        .strip_prefix(r"\\?\Volume{")
        .and_then(|p| p.strip_suffix("}\\"))
    else {
        return false;
    };
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}
pub fn valid_snapshot_path(path: &str) -> bool {
    path.strip_prefix(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
