use super::{
    device::{self, Volume},
    vss::WindowsVss,
};
use crate::{
    Result,
    consistent::{ConsistentDisk, RegionKind, RegionReader, SnapshotRegion},
    digest, invalid,
    layout::{self, Layout, Role, Scheme},
    physical::{BlockSource, Consistency, DiskIdentity, SourceInfo},
    snapshot::SnapshotSession,
};
use std::{
    fs::{File, OpenOptions},
    os::windows::fs::OpenOptionsExt,
    sync::atomic::AtomicBool,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME, IOCTL_DISK_GET_LENGTH_INFO,
};

pub struct LiveInspection {
    pub disk: DiskIdentity,
    pub layout: Layout,
    pub volumes: Vec<(u32, Volume)>,
}
impl LiveInspection {
    pub fn inspect(number: u32) -> Result<Self> {
        let mut raw = device::open_disk_read(number)?;
        let disk = device::identity_from_handle(&raw, number)?;
        let layout = layout::parse(&mut raw, disk.bytes, disk.sector)?;
        if layout.scheme != Scheme::Gpt
            || layout
                .partitions
                .iter()
                .filter(|p| p.role == Role::Efi)
                .count()
                != 1
        {
            return Err(invalid(
                "Live Windows copying supports basic UEFI/GPT with exactly one FAT32 EFI partition.",
            ));
        }
        let system_volume = device::volume_for_path(&device::windows_directory()?)?;
        let volumes = device::volumes_on_disk(number)?;
        let mut mappings = Vec::new();
        let mut contains_windows = false;
        for part in &layout.partitions {
            if part.role == Role::Reserved {
                continue;
            }
            if !matches!(part.role, Role::Efi | Role::Data | Role::Recovery) {
                return Err(invalid(
                    "Live source contains unsupported/dynamic partition types.",
                ));
            }
            let offset = part.first_lba * disk.sector as u64;
            let length = part.bytes(disk.sector);
            let volume=volumes.iter().find(|v|v.offset==offset&&v.bytes==length).ok_or_else(||invalid(format!("Partition {} has no exact basic-volume mapping; partial or spanned sources are unsupported.",part.index)))?;
            let boot = layout::read_at(&mut raw, offset, disk.sector as usize)?;
            let signature = super::probe_boot_sector(&boot);
            match part.role {
                Role::Efi
                    if volume.filesystem.eq_ignore_ascii_case("FAT32")
                        && signature == super::BootSectorSignature::Fat32 => {}
                Role::Data | Role::Recovery
                    if volume.filesystem.eq_ignore_ascii_case("NTFS")
                        && signature == super::BootSectorSignature::Ntfs => {}
                _ => {
                    return Err(invalid(format!(
                        "Partition {} is encrypted or is not supported NTFS/FAT32. BitLocker, ReFS, dynamic volumes and filesystem conversion are unsupported.",
                        part.index
                    )));
                }
            }
            if volume.path.eq_ignore_ascii_case(&system_volume) {
                contains_windows = part.role == Role::Data;
            }
            mappings.push((part.index, volume.clone()));
        }
        if !contains_windows {
            return Err(invalid(
                "The selected live source does not contain the running Windows volume.",
            ));
        }
        Ok(Self {
            disk,
            layout,
            volumes: mappings,
        })
    }
    pub fn prepare(self, cancel: &AtomicBool) -> Result<ConsistentDisk<WindowsLiveReader>> {
        let original_metadata = digest(&serde_json::to_vec(&self.layout)?);
        let mut handles = Vec::new();
        let mut regions = Vec::new();
        let mut ntfs = Vec::new();
        // No ESP fallback reads: if the filesystem cannot be exclusively locked, stop.
        for (index, volume) in &self.volumes {
            let part = self
                .layout
                .partitions
                .iter()
                .find(|p| p.index == *index)
                .unwrap();
            if part.role == Role::Efi {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .share_mode(3)
                    .open(volume.path.trim_end_matches('\\'))?;
                device::control(&file,FSCTL_LOCK_VOLUME,&[],&mut []).map_err(|e|invalid(format!("EFI partition could not be exclusively locked: {e}. Use an offline/WinPE image instead; live raw EFI copying is not used.")))?;
                let handle = LiveHandle {
                    token: volume.path.clone(),
                    file,
                    bytes: volume.bytes,
                    locked: true,
                };
                let extents = device::volume_extents(&handle.file)?;
                if extents != vec![(self.disk.number, volume.offset, volume.bytes)] {
                    return Err(invalid(
                        "EFI volume mapping changed during lock acquisition.",
                    ));
                }
                regions.push(SnapshotRegion {
                    partition: *index,
                    token: volume.path.clone(),
                    kind: RegionKind::LockedEfi,
                });
                handles.push(handle);
            } else {
                ntfs.push(volume.path.clone());
            }
        }
        let session = SnapshotSession::create(WindowsVss::new(), &ntfs, cancel, |_| {})?;
        for (index, volume) in &self.volumes {
            let part = self
                .layout
                .partitions
                .iter()
                .find(|p| p.index == *index)
                .unwrap();
            if part.role == Role::Efi {
                continue;
            }
            let snapshot = session
                .set
                .volumes
                .iter()
                .find(|s| s.original_volume == volume.path)
                .ok_or_else(|| invalid("VSS set is missing a planned NTFS volume."))?;
            let file = OpenOptions::new()
                .read(true)
                .share_mode(3)
                .open(&snapshot.device_path)?;
            let mut length = [0; 8];
            if device::control(&file, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut length)? < 8
                || layout::u64_at(&length, 0) < volume.bytes
            {
                return Err(invalid(
                    "VSS snapshot does not expose every sector in its source partition.",
                ));
            }
            regions.push(SnapshotRegion {
                partition: *index,
                token: snapshot.snapshot_id.clone(),
                kind: RegionKind::Snapshot,
            });
            handles.push(LiveHandle {
                token: snapshot.snapshot_id.clone(),
                file,
                bytes: volume.bytes,
                locked: false,
            });
        }
        let info = SourceInfo {
            id: digest(
                format!(
                    "{}:{}:{original_metadata}",
                    self.disk.hardware_id, session.set.id
                )
                .as_bytes(),
            ),
            bytes: self.disk.bytes,
            sector: self.disk.sector,
            storage_disk_ids: vec![self.disk.hardware_id.clone()],
            consistency: Consistency::Vss {
                set_id: session.set.id.clone(),
                snapshot_ids: session
                    .set
                    .volumes
                    .iter()
                    .map(|s| s.snapshot_id.clone())
                    .collect(),
                writers_checked: session.set.writers_checked,
            },
        };
        let raw = device::open_disk_read(self.disk.number)?;
        let reader = WindowsLiveReader {
            raw,
            expected: self.disk,
            metadata_hash: original_metadata,
            handles,
            session,
        };
        let mut source = ConsistentDisk::new(self.layout, info, regions, reader)?;
        source.validate(cancel)?;
        Ok(source)
    }
}
struct LiveHandle {
    token: String,
    file: File,
    bytes: u64,
    locked: bool,
}
impl Drop for LiveHandle {
    fn drop(&mut self) {
        if self.locked {
            let _ = device::control(&self.file, FSCTL_UNLOCK_VOLUME, &[], &mut []);
        }
    }
}
pub struct WindowsLiveReader {
    raw: File,
    expected: DiskIdentity,
    metadata_hash: String,
    handles: Vec<LiveHandle>,
    pub session: SnapshotSession<WindowsVss>,
}
impl RegionReader for WindowsLiveReader {
    fn read_region(&mut self, token: &str, offset: u64, buffer: &mut [u8]) -> Result<()> {
        let handle = self
            .handles
            .iter_mut()
            .find(|h| h.token == token)
            .ok_or_else(|| invalid("Source region handle is missing."))?;
        device::read_bounded(
            &mut handle.file,
            offset,
            buffer,
            handle.bytes,
            self.expected.sector,
        )
    }
    fn validate(&mut self, cancel: &AtomicBool) -> Result<()> {
        if device::identity_from_handle(&self.raw, self.expected.number)? != self.expected {
            return Err(invalid(
                "Live source hardware identity or geometry changed.",
            ));
        }
        let layout = layout::parse(&mut self.raw, self.expected.bytes, self.expected.sector)?;
        if digest(&serde_json::to_vec(&layout)?) != self.metadata_hash {
            return Err(invalid(
                "Live source partition metadata changed after snapshot preparation.",
            ));
        }
        self.session.validate(cancel)
    }
}
