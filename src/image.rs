//! Regular-file identities and read/write separation. Device namespaces never enter this API.
use crate::{
    Result, digest, invalid,
    layout::{self, Layout},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub key: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub path: PathBuf,
    pub identity: Identity,
    pub modified_ns: u128,
    pub layout: Layout,
    pub metadata_hash: String,
}
pub fn validate_path(path: &Path) -> Result<()> {
    let full = path.to_string_lossy();
    // std::fs::canonicalize returns the verbatim local-drive prefix on Windows.
    // Permit that one form, but never UNC, device, volume or GLOBALROOT namespaces.
    let s = full.strip_prefix(r"\\?\").unwrap_or(&full);
    if s.starts_with(r"\\")
        || s.starts_with("//")
        || s.contains("GLOBALROOT")
        || s.contains("PhysicalDrive")
        || s.contains("..\\")
        || s.contains("../")
    {
        return Err(invalid(
            "Use an absolute local .img or .raw file, outside device namespaces.",
        ));
    }
    if !path.is_absolute()
        || !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("img") || e.eq_ignore_ascii_case("raw"))
    {
        return Err(invalid("Select an absolute local .img or .raw file."));
    }
    #[cfg(windows)]
    {
        if s.char_indices().any(|(i, c)| c == ':' && i != 1)
            || !s
                .as_bytes()
                .get(1..3)
                .is_some_and(|p| p == b":\\" || p == b":/")
        {
            return Err(invalid(
                "Use a local drive path without alternate data streams.",
            ));
        }
        for part in s.split(['\\', '/']).skip(1) {
            let name = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            if ["CON", "PRN", "AUX", "NUL"].contains(&name.as_str())
                || (name.len() == 4
                    && (name.starts_with("COM") || name.starts_with("LPT"))
                    && name.as_bytes()[3].is_ascii_digit())
            {
                return Err(invalid(
                    "Reserved Windows device names are not disk images.",
                ));
            }
        }
    }
    if path.exists() && !std::fs::metadata(path)?.is_file() {
        return Err(invalid("The selected path is not a regular file."));
    }
    Ok(())
}
pub fn identity(file: &File) -> Result<Identity> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("Only regular image files are allowed."));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid(
                "Image reparse points are unsupported. Select a regular image file directly.",
            ));
        }
    }
    #[cfg(windows)]
    let key = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        format!(
            "{}:{}:{}",
            info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
        )
    };
    #[cfg(unix)]
    let key = {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    };
    #[cfg(not(any(windows, unix)))]
    compile_error!("Stable image identity is not implemented on this platform");
    Ok(Identity {
        key,
        bytes: metadata.len(),
    })
}
pub fn modified(file: &File) -> Result<u128> {
    Ok(file
        .metadata()?
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("Invalid file modification time."))?
        .as_nanos())
}
pub fn open_source(path: &Path) -> Result<File> {
    validate_path(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1).custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    identity(&file)?;
    fs2::FileExt::try_lock_shared(&file)?;
    Ok(file)
}
pub fn inspect(path: &Path, sector: u32) -> Result<Source> {
    let mut file = open_source(path)?;
    let id = identity(&file)?;
    let stamp = modified(&file)?;
    let layout = layout::parse(&mut file, id.bytes, sector)?;
    let metadata_hash = digest(&serde_json::to_vec(&layout)?);
    Ok(Source {
        path: std::fs::canonicalize(path)?,
        identity: id,
        modified_ns: stamp,
        layout,
        metadata_hash,
    })
}
pub(crate) fn open_destination(
    path: &Path,
    expected: Option<&Identity>,
    bytes: u64,
) -> Result<File> {
    validate_path(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0).custom_flags(0x00200000); // Never follow a final device/file symlink.
    }
    if expected.is_none() {
        options.create_new(true);
    }
    let file = options.open(path)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    let actual = identity(&file)?;
    if let Some(id) = expected {
        if &actual != id {
            return Err(invalid(
                "Destination identity or size changed since review.",
            ));
        }
    } else {
        file.set_len(bytes)?;
        file.sync_all()?;
    }
    Ok(file)
}
