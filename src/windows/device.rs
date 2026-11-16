use crate::{
    Result, digest, image, invalid, layout,
    physical::{
        self, BlockSource, BlockTarget, Consistency, DiskIdentity, SourceInfo, TargetFactory,
        TargetInfo,
    },
};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use windows_sys::Win32::{
    Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE},
    Storage::FileSystem::*,
    System::{IO::DeviceIoControl, Ioctl::*, SystemInformation::GetWindowsDirectoryW},
};

pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn text(buffer: &[u16]) -> String {
    String::from_utf16_lossy(&buffer[..buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len())])
}
pub fn control(file: &File, code: u32, input: &[u8], out: &mut [u8]) -> Result<usize> {
    let mut returned = 0;
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            if input.is_empty() {
                std::ptr::null()
            } else {
                input.as_ptr().cast()
            },
            input.len() as u32,
            if out.is_empty() {
                std::ptr::null_mut()
            } else {
                out.as_mut_ptr().cast()
            },
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if returned as usize > out.len() {
        return Err(invalid("Driver returned an invalid byte count."));
    }
    Ok(returned as usize)
}
pub fn open_disk_read(number: u32) -> Result<File> {
    if number > 4095 {
        return Err(invalid("Physical disk number is out of range."));
    }
    Ok(OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(format!(r"\\.\PhysicalDrive{number}"))?)
}
fn property(file: &File, id: i32) -> Result<Vec<u8>> {
    let mut query = [0; 12];
    query[..4].copy_from_slice(&id.to_le_bytes());
    let mut out = vec![0; 64 * 1024];
    let n = control(file, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut out)?;
    out.truncate(n);
    if n < 8 || layout::u32_at(&out, 4) as usize > n {
        return Err(invalid("Storage descriptor is truncated."));
    }
    Ok(out)
}
fn descriptor_string(bytes: &[u8], field: usize) -> Result<String> {
    let p = layout::u32_at(bytes, field) as usize;
    if p == 0 {
        return Ok(String::new());
    }
    if p >= bytes.len() {
        return Err(invalid("Invalid storage identity offset."));
    }
    let n = bytes[p..]
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| invalid("Storage identity string is not terminated."))?;
    Ok(String::from_utf8_lossy(&bytes[p..p + n]).trim().to_string())
}
pub fn identity_from_handle(file: &File, number: u32) -> Result<DiskIdentity> {
    let mut geometry = [0; 256];
    if control(file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut geometry)? < 32 {
        return Err(invalid("Truncated disk geometry."));
    }
    let sector = layout::u32_at(&geometry, 20);
    let bytes = layout::u64_at(&geometry, 24);
    layout::valid_size(bytes, sector)?;
    let mut device = [0; 12];
    if control(file, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut device)? < 12
        || layout::u32_at(&device, 4) != number
    {
        return Err(invalid("Physical disk number changed during inspection."));
    }
    let descriptor = property(file, StorageDeviceProperty)?;
    if descriptor.len() < 36 {
        return Err(invalid("Storage device descriptor is truncated."));
    }
    let serial = descriptor_string(&descriptor, 24)?;
    let model = descriptor_string(&descriptor, 16)?;
    let ids = parse_device_identifiers(&property(file, StorageDeviceIdProperty)?)?;
    if serial.is_empty() || ids.is_empty() {
        return Err(invalid(
            "Physical copying requires a hardware serial and device-associated VPD identifier; this bridge/device does not expose both.",
        ));
    }
    let hardware_id = digest(&serde_json::to_vec(&(serial.clone(), ids))?);
    Ok(DiskIdentity {
        number,
        hardware_id,
        serial,
        model,
        bytes,
        sector,
    })
}
/// Device association (0) excludes port/path IDs that change when reconnecting an SSD.
pub fn parse_device_identifiers(bytes: &[u8]) -> Result<Vec<String>> {
    if bytes.len() < 12 {
        return Err(invalid("Storage VPD identifier list is truncated."));
    }
    let count = layout::u32_at(bytes, 8);
    if count > 256 {
        return Err(invalid("Storage VPD identifier count exceeds the bound."));
    }
    let mut p = 12usize;
    let mut ids = Vec::new();
    for i in 0..count {
        if p + 16 > bytes.len() {
            return Err(invalid("Storage VPD identifier header is truncated."));
        }
        let size = u16::from_le_bytes(bytes[p + 8..p + 10].try_into().unwrap()) as usize;
        let next = u16::from_le_bytes(bytes[p + 10..p + 12].try_into().unwrap()) as usize;
        if size == 0 || p + 16 + size > bytes.len() {
            return Err(invalid("Storage VPD identifier exceeds its descriptor."));
        }
        if layout::u32_at(bytes, p + 12) == 0 {
            ids.push(format!(
                "{}:{}:{}",
                layout::u32_at(bytes, p),
                layout::u32_at(bytes, p + 4),
                crate::digest(&bytes[p + 16..p + 16 + size])
            ));
        }
        if i + 1 < count {
            if next < 16 + size || p + next <= p {
                return Err(invalid("Storage VPD linked-list offset is invalid."));
            }
            p += next;
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}
fn disk_attributes(file: &File) -> Result<(bool, bool)> {
    let mut out = [0; 16];
    if control(file, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut out)? < 16 {
        return Err(invalid("Disk attributes are unavailable."));
    }
    let attributes = layout::u64_at(&out, 8);
    Ok((
        attributes & DISK_ATTRIBUTE_OFFLINE != 0,
        attributes & DISK_ATTRIBUTE_READ_ONLY != 0,
    ))
}
#[derive(Clone, Debug)]
pub struct Volume {
    pub path: String,
    pub disk: u32,
    pub offset: u64,
    pub bytes: u64,
    pub filesystem: String,
}
pub fn volume_extents(file: &File) -> Result<Vec<(u32, u64, u64)>> {
    let mut out = vec![0; 64 * 1024];
    let n = control(file, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut out)?;
    parse_extents(&out[..n])
}
pub fn parse_extents(bytes: &[u8]) -> Result<Vec<(u32, u64, u64)>> {
    if bytes.len() < 8 {
        return Err(invalid("Volume extent list is truncated."));
    }
    let count = layout::u32_at(bytes, 0) as usize;
    if count == 0 || count > 256 || 8 + count * 24 > bytes.len() {
        return Err(invalid("Volume extent list exceeds its bound."));
    }
    let mut extents = Vec::new();
    for i in 0..count {
        let p = 8 + i * 24;
        let disk = layout::u32_at(bytes, p);
        let offset = layout::u64_at(bytes, p + 8);
        let length = layout::u64_at(bytes, p + 16);
        if length == 0 || offset.checked_add(length).is_none() {
            return Err(invalid("Invalid volume extent geometry."));
        }
        extents.push((disk, offset, length));
    }
    Ok(extents)
}
pub fn open_volume_read(path: &str) -> Result<File> {
    if !crate::snapshot::valid_volume(path) {
        return Err(invalid("Expected a local volume GUID path."));
    }
    Ok(OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(path.trim_end_matches('\\'))?)
}
pub fn filesystem(path: &str) -> Result<String> {
    let mut fs = [0; 64];
    let success = unsafe {
        GetVolumeInformationW(
            wide(path).as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if success == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(text(&fs))
}
pub fn volumes_on_disk(number: u32) -> Result<Vec<Volume>> {
    struct Search(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Search {
        fn drop(&mut self) {
            unsafe { FindVolumeClose(self.0) };
        }
    }
    let mut buffer = [0; 1024];
    let handle = unsafe { FindFirstVolumeW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let _search = Search(handle);
    let mut out = Vec::new();
    loop {
        let name = text(&buffer);
        if let Ok(file) = open_volume_read(&name)
            && let Ok(extents) = volume_extents(&file)
            && extents.iter().any(|(disk, _, _)| *disk == number)
        {
            if extents.len() != 1 {
                return Err(invalid("Spanned and dynamic volumes cannot be cloned."));
            }
            let (disk, offset, bytes) = extents[0];
            out.push(Volume {
                filesystem: filesystem(&name)?,
                path: name,
                disk,
                offset,
                bytes,
            });
        }
        if unsafe { FindNextVolumeW(handle, buffer.as_mut_ptr(), buffer.len() as u32) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(error.into());
        }
        if out.len() > 256 {
            return Err(invalid("Too many volumes on one source disk."));
        }
    }
    Ok(out)
}
pub fn windows_directory() -> Result<PathBuf> {
    let mut buffer = [0; 32768];
    let n = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if n == 0 || n as usize >= buffer.len() {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(PathBuf::from(text(&buffer)))
}
pub fn volume_for_path(path: &Path) -> Result<String> {
    let existing = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent()
            .ok_or_else(|| invalid("Path has no parent directory."))?
            .to_path_buf()
    };
    let canonical = std::fs::canonicalize(existing)?;
    let mut root = [0; 32768];
    let input = canonical.to_string_lossy();
    if unsafe { GetVolumePathNameW(wide(&input).as_ptr(), root.as_mut_ptr(), root.len() as u32) }
        == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut volume = [0; 1024];
    if unsafe {
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), volume.as_mut_ptr(), volume.len() as u32)
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let result = text(&volume);
    if !crate::snapshot::valid_volume(&result) {
        return Err(invalid("Only local basic volumes are supported."));
    }
    Ok(result)
}
pub fn disks_for_path(path: &Path) -> Result<Vec<u32>> {
    let path = volume_for_path(path)?;
    Ok(volume_extents(&open_volume_read(&path)?)?
        .into_iter()
        .map(|(disk, _, _)| disk)
        .collect())
}

pub struct WindowsTargetFactory {
    pub number: u32,
    protected_paths: Vec<PathBuf>,
}
impl WindowsTargetFactory {
    pub fn new(number: u32, source_paths: Vec<PathBuf>, checkpoint: &Path) -> Result<Self> {
        let mut protected_paths = source_paths;
        protected_paths.push(checkpoint.to_path_buf());
        protected_paths.push(std::env::current_exe()?);
        protected_paths.push(windows_directory()?);
        Ok(Self {
            number,
            protected_paths,
        })
    }
    fn protected(&self) -> Result<bool> {
        for path in &self.protected_paths {
            if disks_for_path(path)?.contains(&self.number) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
impl TargetFactory for WindowsTargetFactory {
    type Target = WindowsTarget;
    fn inspect(&mut self) -> Result<TargetInfo> {
        let file = open_disk_read(self.number)?;
        let disk = identity_from_handle(&file, self.number)?;
        let (offline, read_only) = disk_attributes(&file)?;
        Ok(TargetInfo {
            disk,
            offline,
            read_only,
            system_disk: self.protected()?,
        })
    }
    fn open_confirmed(
        &mut self,
        expected: &TargetInfo,
        confirmation: &str,
    ) -> Result<Self::Target> {
        if confirmation != physical::confirmation_for(&expected.disk)
            || !expected.offline
            || expected.read_only
            || expected.system_disk
        {
            return Err(invalid("Physical write authorization failed."));
        }
        if self.inspect()? != *expected {
            return Err(invalid(
                "Destination changed before opening it for writing.",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_WRITE_THROUGH)
            .open(format!(r"\\.\PhysicalDrive{}", self.number))?;
        let mut target = WindowsTarget {
            file,
            reviewed: expected.clone(),
        };
        if target.info()? != *expected {
            return Err(invalid("Destination changed while acquiring write access."));
        }
        Ok(target)
    }
}
pub struct WindowsTarget {
    file: File,
    reviewed: TargetInfo,
}
impl BlockTarget for WindowsTarget {
    fn info(&mut self) -> Result<TargetInfo> {
        let disk = identity_from_handle(&self.file, self.reviewed.disk.number)?;
        let (offline, read_only) = disk_attributes(&self.file)?;
        Ok(TargetInfo {
            disk,
            offline,
            read_only,
            system_disk: self.reviewed.system_disk,
        })
    }
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        read_bounded(
            &mut self.file,
            offset,
            buffer,
            self.reviewed.disk.bytes,
            self.reviewed.disk.sector,
        )
    }
    fn write_at(&mut self, offset: u64, buffer: &[u8]) -> Result<()> {
        if !offset.is_multiple_of(self.reviewed.disk.sector as u64)
            || !(buffer.len() as u64).is_multiple_of(self.reviewed.disk.sector as u64)
            || offset
                .checked_add(buffer.len() as u64)
                .is_none_or(|v| v > self.reviewed.disk.bytes)
        {
            return Err(invalid(
                "Physical write is unaligned or outside the reviewed destination.",
            ));
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buffer)?;
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}
pub fn read_bounded(
    file: &mut File,
    offset: u64,
    buffer: &mut [u8],
    length: u64,
    sector: u32,
) -> Result<()> {
    if buffer.len() > 16 * 1024 * 1024
        || offset
            .checked_add(buffer.len() as u64)
            .is_none_or(|n| n > length)
    {
        return Err(invalid("Block read is outside its source extent."));
    }
    if buffer.is_empty() {
        return Ok(());
    }
    let a = offset / (sector as u64) * sector as u64;
    let end = (offset + buffer.len() as u64).div_ceil(sector as u64) * sector as u64;
    if a == offset && end - offset == buffer.len() as u64 {
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(buffer)?;
    } else {
        let mut aligned = vec![0; (end - a) as usize];
        file.seek(SeekFrom::Start(a))?;
        file.read_exact(&mut aligned)?;
        buffer
            .copy_from_slice(&aligned[(offset - a) as usize..(offset - a) as usize + buffer.len()]);
    }
    Ok(())
}
pub struct ImageSource {
    file: File,
    source: image::Source,
    info: SourceInfo,
}
impl ImageSource {
    pub fn open(path: &Path, sector: u32) -> Result<Self> {
        let source = image::inspect(path, sector)?;
        let file = image::open_source(&source.path)?;
        let mut storage_disk_ids = Vec::new();
        for n in disks_for_path(&source.path)? {
            storage_disk_ids.push(identity_from_handle(&open_disk_read(n)?, n)?.hardware_id);
        }
        let info = SourceInfo {
            id: source.identity.key.clone(),
            bytes: source.identity.bytes,
            sector,
            storage_disk_ids,
            consistency: Consistency::OfflineImage {
                file_id: source.identity.key.clone(),
            },
        };
        Ok(Self { file, source, info })
    }
}
impl BlockSource for ImageSource {
    fn info(&self) -> SourceInfo {
        self.info.clone()
    }
    fn layout(&self) -> &layout::Layout {
        &self.source.layout
    }
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        read_bounded(
            &mut self.file,
            offset,
            buffer,
            self.info.bytes,
            self.info.sector,
        )
    }
    fn validate(&mut self, _: &AtomicBool) -> Result<()> {
        if image::identity(&self.file)? != self.source.identity
            || image::modified(&self.file)? != self.source.modified_ns
        {
            return Err(invalid("Source image changed since review."));
        }
        Ok(())
    }
}
