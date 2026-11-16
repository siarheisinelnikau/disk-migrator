//! Construct a complete GPT disk stream from immutable NTFS snapshots and a locked ESP.
use crate::{
    Result, invalid,
    layout::{Layout, Role, Scheme},
    physical::{BlockSource, Consistency, SourceInfo},
    plan::Patch,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::atomic::AtomicBool};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegionKind {
    Snapshot,
    LockedEfi,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRegion {
    pub partition: u32,
    pub token: String,
    pub kind: RegionKind,
}
pub trait RegionReader {
    fn read_region(&mut self, token: &str, offset: u64, buffer: &mut [u8]) -> Result<()>;
    fn validate(&mut self, cancel: &AtomicBool) -> Result<()>;
}
pub struct ConsistentDisk<R: RegionReader> {
    layout: Layout,
    info: SourceInfo,
    regions: Vec<SnapshotRegion>,
    metadata: Vec<Patch>,
    pub reader: R,
}
impl<R: RegionReader> ConsistentDisk<R> {
    pub fn new(
        layout: Layout,
        info: SourceInfo,
        regions: Vec<SnapshotRegion>,
        reader: R,
    ) -> Result<Self> {
        if layout.scheme != Scheme::Gpt
            || layout.bytes != info.bytes
            || layout.sector_size != info.sector
        {
            return Err(invalid(
                "Live Windows migration requires a matching basic GPT source.",
            ));
        }
        let Consistency::Vss {
            set_id,
            snapshot_ids,
            writers_checked,
        } = &info.consistency
        else {
            return Err(invalid(
                "Live Windows source requires VSS snapshot evidence.",
            ));
        };
        if set_id.is_empty()
            || *writers_checked == 0
            || layout
                .partitions
                .iter()
                .filter(|p| p.role == Role::Efi)
                .count()
                != 1
            || !layout.partitions.iter().any(|p| p.role == Role::Data)
        {
            return Err(invalid(
                "Live source needs one EFI partition, a Windows data partition and healthy VSS writers.",
            ));
        }
        let mut indices = BTreeSet::new();
        let mut tokens = BTreeSet::new();
        let mut mapped_snapshots = BTreeSet::new();
        for region in &regions {
            if !indices.insert(region.partition)
                || region.token.is_empty()
                || !tokens.insert(region.token.clone())
            {
                return Err(invalid("Duplicate or missing snapshot region identity."));
            }
            let p = layout
                .partitions
                .iter()
                .find(|p| p.index == region.partition)
                .ok_or_else(|| invalid("Snapshot region does not match a source partition."))?;
            match (&p.role, &region.kind) {
                (Role::Efi, RegionKind::LockedEfi) => {}
                (Role::Data | Role::Recovery, RegionKind::Snapshot) => {
                    mapped_snapshots.insert(region.token.clone());
                }
                _ => {
                    return Err(invalid(
                        "Every data/recovery partition needs its own snapshot, and EFI must be locked.",
                    ));
                }
            }
        }
        for p in &layout.partitions {
            if p.role == Role::Reserved {
                continue;
            }
            if !matches!(p.role, Role::Efi | Role::Data | Role::Recovery)
                || !indices.contains(&p.index)
            {
                return Err(invalid(
                    "Unsupported or unsnapshotted live partition. Dynamic disks, unknown filesystems and partial snapshots are rejected.",
                ));
            }
        }
        let ids = snapshot_ids.iter().cloned().collect::<BTreeSet<_>>();
        if ids.len() != snapshot_ids.len() || ids != mapped_snapshots || ids.is_empty() {
            return Err(invalid(
                "Snapshot-set membership does not cover the complete live partition plan.",
            ));
        }
        let gpt = layout
            .gpt
            .as_ref()
            .ok_or_else(|| invalid("GPT metadata is missing."))?;
        let s = layout.sector_size as u64;
        let metadata = vec![
            Patch {
                offset: 0,
                bytes: layout.mbr.clone(),
            },
            Patch {
                offset: s,
                bytes: gpt.primary.clone(),
            },
            Patch {
                offset: gpt.primary_entries_lba * s,
                bytes: gpt.entries.clone(),
            },
            Patch {
                offset: gpt.backup_entries_lba * s,
                bytes: gpt.entries.clone(),
            },
            Patch {
                offset: layout.bytes - s,
                bytes: gpt.backup.clone(),
            },
        ];
        Ok(Self {
            layout,
            info,
            regions,
            metadata,
            reader,
        })
    }
}
impl<R: RegionReader> BlockSource for ConsistentDisk<R> {
    fn info(&self) -> SourceInfo {
        self.info.clone()
    }
    fn layout(&self) -> &Layout {
        &self.layout
    }
    fn validate(&mut self, cancel: &AtomicBool) -> Result<()> {
        self.reader.validate(cancel)
    }
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        if buffer.len() > 16 * 1024 * 1024
            || offset
                .checked_add(buffer.len() as u64)
                .is_none_or(|end| end > self.info.bytes)
        {
            return Err(invalid("Consistent-source read is outside disk bounds."));
        }
        buffer.fill(0);
        let end = offset + buffer.len() as u64;
        // Gaps and the Microsoft Reserved partition are deterministic zeros, not live reads.
        for m in &self.metadata {
            let a = offset.max(m.offset);
            let b = end.min(m.offset + m.bytes.len() as u64);
            if a < b {
                buffer[(a - offset) as usize..(b - offset) as usize]
                    .copy_from_slice(&m.bytes[(a - m.offset) as usize..(b - m.offset) as usize]);
            }
        }
        for region in &self.regions {
            let p = self
                .layout
                .partitions
                .iter()
                .find(|p| p.index == region.partition)
                .unwrap();
            let start = p.first_lba * self.info.sector as u64;
            let finish = (p.last_lba + 1) * self.info.sector as u64;
            let a = offset.max(start);
            let b = end.min(finish);
            if a < b {
                self.reader.read_region(
                    &region.token,
                    a - start,
                    &mut buffer[(a - offset) as usize..(b - offset) as usize],
                )?;
            }
        }
        Ok(())
    }
}
