use crate::{
    Result, digest,
    image::{self, Identity, Source},
    invalid,
    layout::{self, Scheme},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Patch {
    pub offset: u64,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub source: Source,
    pub destination: PathBuf,
    pub destination_identity: Option<Identity>,
    pub destination_bytes: u64,
    pub patches: Vec<Patch>,
    pub id: String,
}
impl Plan {
    pub fn build(source: Source, destination: &Path, destination_bytes: u64) -> Result<Self> {
        image::validate_path(destination)?;
        layout::valid_size(destination_bytes, source.layout.sector_size)?;
        if destination_bytes < source.identity.bytes {
            return Err(invalid(
                "Destination is smaller than the source. Filesystem shrinking is not implemented.",
            ));
        }
        let parent = std::fs::canonicalize(
            destination
                .parent()
                .ok_or_else(|| invalid("Destination parent is missing."))?,
        )?;
        let destination = parent.join(
            destination
                .file_name()
                .ok_or_else(|| invalid("Destination filename is missing."))?,
        );
        image::validate_path(&destination)?;
        let destination_identity = if destination.exists() {
            let file = image::open_source(&destination)?;
            let id = image::identity(&file)?;
            if id.key == source.identity.key {
                return Err(invalid(
                    "Source and destination refer to the same file (possibly a hard link).",
                ));
            }
            if id.bytes != destination_bytes {
                return Err(invalid("Destination size changed. Inspect it again."));
            }
            Some(id)
        } else {
            None
        };
        let mut patches = Vec::new();
        if let Some(gpt) = &source.layout.gpt {
            let sector = source.layout.sector_size as u64;
            let old_count = source.identity.bytes / sector;
            let new_count = destination_bytes / sector;
            if old_count != new_count {
                let blocks = (gpt.entries.len() as u64).div_ceil(sector);
                let new_entries = new_count - 1 - blocks;
                // Remove the obsolete backup; source partitions never occupy this range.
                patches.push(Patch {
                    offset: gpt.backup_entries_lba * sector,
                    bytes: vec![0; (blocks * sector) as usize],
                });
                patches.push(Patch {
                    offset: (old_count - 1) * sector,
                    bytes: vec![0; sector as usize],
                });
                let mut primary = gpt.primary.clone();
                primary[32..40].copy_from_slice(&(new_count - 1).to_le_bytes());
                primary[48..56].copy_from_slice(&(new_entries - 1).to_le_bytes());
                layout::header_crc(&mut primary);
                patches.push(Patch {
                    offset: sector,
                    bytes: primary,
                });
                let mut backup = gpt.backup.clone();
                backup[24..32].copy_from_slice(&(new_count - 1).to_le_bytes());
                backup[48..56].copy_from_slice(&(new_entries - 1).to_le_bytes());
                backup[72..80].copy_from_slice(&new_entries.to_le_bytes());
                layout::header_crc(&mut backup);
                let mut entries = gpt.entries.clone();
                entries.resize((blocks * sector) as usize, 0);
                patches.push(Patch {
                    offset: new_entries * sector,
                    bytes: entries,
                });
                patches.push(Patch {
                    offset: (new_count - 1) * sector,
                    bytes: backup,
                });
                let mut mbr = source.layout.mbr.clone();
                let i = (0..4).find(|i| mbr[446 + i * 16 + 4] == 0xee).unwrap();
                mbr[446 + i * 16 + 12..446 + i * 16 + 16]
                    .copy_from_slice(&((new_count - 1).min(u32::MAX as u64) as u32).to_le_bytes());
                patches.push(Patch {
                    offset: 0,
                    bytes: mbr,
                });
            }
        }
        let mut plan = Self {
            source,
            destination,
            destination_identity,
            destination_bytes,
            patches,
            id: String::new(),
        };
        plan.id = plan.calculated_id()?;
        Ok(plan)
    }
    pub fn calculated_id(&self) -> Result<String> {
        let mut copy = self.clone();
        copy.id.clear();
        copy.destination_identity = None;
        Ok(digest(&serde_json::to_vec(&copy)?))
    }
    pub fn confirmation_name(&self) -> String {
        self.destination
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
    pub fn checkpoint_path(&self) -> PathBuf {
        let mut p = self.destination.as_os_str().to_os_string();
        p.push(".migration.json");
        PathBuf::from(p)
    }
    pub fn overlay(&self, offset: u64, buffer: &mut [u8]) {
        let end = offset + buffer.len() as u64;
        // Ordered overlays also handle a barely-larger image where old/new tables overlap.
        for p in &self.patches {
            let a = offset.max(p.offset);
            let b = end.min(p.offset + p.bytes.len() as u64);
            if a < b {
                buffer[(a - offset) as usize..(b - offset) as usize]
                    .copy_from_slice(&p.bytes[(a - p.offset) as usize..(b - p.offset) as usize]);
            }
        }
    }
    pub fn is_gpt(&self) -> bool {
        self.source.layout.scheme == Scheme::Gpt
    }
}
