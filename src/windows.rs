//! Windows physical adapters are reached only through explicit advanced commands.
pub mod device;
pub mod live;
pub mod vss;
use crate::{Result, invalid};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
};
use windows_sys::Win32::System::{
    IO::DeviceIoControl,
    Ioctl::{
        IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
    },
};

#[derive(Clone, Debug)]
pub struct PhysicalDiskInfo {
    pub number: u32,
    pub bytes: u64,
    pub logical_sector: u32,
    pub serial: String,
    pub model: String,
    pub removable: bool,
}
pub struct PhysicalSource {
    file: File,
    pub info: PhysicalDiskInfo,
}
impl PhysicalSource {
    /// Opens only GENERIC_READ. Reading a running Windows disk is NOT a consistent clone.
    pub fn open_read_only(number: u32) -> Result<Self> {
        if number > 4095 {
            return Err(invalid("Physical disk index is out of bounds."));
        }
        let file = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(format!(r"\\.\PhysicalDrive{number}"))?;
        let mut geometry = [0u8; 256];
        let returned = ioctl(&file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, None, &mut geometry)?;
        if returned < 32 {
            return Err(invalid("Truncated disk geometry."));
        }
        let sector = u32::from_le_bytes(geometry[20..24].try_into().unwrap());
        let bytes = i64::from_le_bytes(geometry[24..32].try_into().unwrap());
        if bytes <= 0 || ![512, 4096].contains(&sector) {
            return Err(invalid("Unsupported disk geometry."));
        }
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut descriptor = [0u8; 4096];
        let count = ioctl(
            &file,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query),
            &mut descriptor,
        )?;
        if count < 36 {
            return Err(invalid("Truncated storage identity descriptor."));
        }
        let field = |offset: usize| -> Result<String> {
            let start =
                u32::from_le_bytes(descriptor[offset..offset + 4].try_into().unwrap()) as usize;
            if start == 0 {
                return Ok(String::new());
            }
            if start >= count {
                return Err(invalid("Invalid storage string offset."));
            }
            let end = descriptor[start..count]
                .iter()
                .position(|c| *c == 0)
                .ok_or_else(|| invalid("Unterminated storage identity string."))?;
            Ok(String::from_utf8_lossy(&descriptor[start..start + end])
                .trim()
                .into())
        };
        let info = PhysicalDiskInfo {
            number,
            bytes: bytes as u64,
            logical_sector: sector,
            serial: field(24)?,
            model: field(16)?,
            removable: descriptor[10] != 0,
        };
        Ok(Self { file, info })
    }
    pub fn read_sectors(&mut self, lba: u64, sectors: u32) -> Result<Vec<u8>> {
        let offset = lba
            .checked_mul(self.info.logical_sector as u64)
            .ok_or_else(|| invalid("Read offset overflow."))?;
        let size = (sectors as u64) * self.info.logical_sector as u64;
        if size > 16 * 1024 * 1024 || offset.checked_add(size).is_none_or(|v| v > self.info.bytes) {
            return Err(invalid("Physical read is outside bounds."));
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; size as usize];
        self.file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
    pub fn migration_availability(&self) -> Availability {
        Availability::Unavailable(
            "Direct live physical reads cannot establish Windows consistency. Use the coordinated VSS source or an offline image through an explicitly confirmed advanced command.",
        )
    }
}
fn ioctl(
    file: &File,
    code: u32,
    input: Option<&STORAGE_PROPERTY_QUERY>,
    out: &mut [u8],
) -> Result<usize> {
    let mut returned = 0;
    let (ptr, len) = input
        .map(|q| {
            (
                q as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(q) as u32,
            )
        })
        .unwrap_or((std::ptr::null(), 0));
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            ptr,
            len,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if returned as usize > out.len() {
        return Err(invalid("Driver returned an invalid output length."));
    }
    Ok(returned as usize)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    Available(&'static str),
    Unavailable(&'static str),
}
pub fn vss_availability() -> Availability {
    Availability::Available(
        "Writer-coordinated VSS is implemented through the Windows SDK. Explicit advanced commands require administrator privileges, basic UEFI/GPT, NTFS data/recovery, an exclusively lockable FAT32 ESP, and healthy writers. Hardware validation is pending.",
    )
}
pub fn boot_repair_availability() -> Availability {
    Availability::Unavailable(
        "Boot repair is disabled. A separate authorized VM/WinPE validation and explicit destination authorization are required.",
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootSectorSignature {
    Ntfs,
    Fat32,
    BitLocker,
    Unknown,
}
/// A signature probe is evidence of content type, never filesystem health or bootability.
pub fn probe_boot_sector(bytes: &[u8]) -> BootSectorSignature {
    if bytes.len() < 512 || bytes[510..512] != [0x55, 0xaa] {
        return BootSectorSignature::Unknown;
    }
    if &bytes[3..11] == b"NTFS    " {
        BootSectorSignature::Ntfs
    } else if &bytes[3..11] == b"-FVE-FS-" {
        BootSectorSignature::BitLocker
    } else if &bytes[82..90] == b"FAT32   " {
        BootSectorSignature::Fat32
    } else {
        BootSectorSignature::Unknown
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probes_only_generated_sectors() {
        let mut b = [0; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        assert_eq!(probe_boot_sector(&b), BootSectorSignature::Unknown);
        b[510..512].copy_from_slice(&[0x55, 0xaa]);
        assert_eq!(probe_boot_sector(&b), BootSectorSignature::Ntfs);
    }
}
