//! Public, unauthenticated GitHub update protocol. Downloads require an explicit UI action.
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const REPOSITORY: &str = "siarheisinelnikau/disk-migrator";
const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;
#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Available {
    pub version: String,
    pub filename: String,
    pub url: String,
    pub checksum_url: String,
    pub bytes: u64,
}
#[derive(Clone, Debug)]
pub struct StagedUpdate {
    pub path: PathBuf,
    pub sha256: String,
}
pub fn startup_check_enabled(prefs: &crate::preferences::Preferences) -> bool {
    prefs.startup_updates
}
pub fn select(releases: &[Release], installed: &str) -> Result<Option<Available>> {
    let installed = semver::Version::parse(installed).map_err(|e| invalid(e.to_string()))?;
    let mut candidates = Vec::new();
    for r in releases {
        if r.draft || r.prerelease {
            continue;
        }
        let Ok(version) = semver::Version::parse(r.tag_name.trim_start_matches('v')) else {
            continue;
        };
        if !version.pre.is_empty() || version <= installed || r.tag_name != format!("v{version}") {
            continue;
        }
        let filename = format!("DiskMigrator-v{version}.exe");
        let Some(asset) = r.assets.iter().find(|a| a.name == filename) else {
            continue;
        };
        let Some(checksum) = r.assets.iter().find(|a| a.name == "SHA256SUMS") else {
            continue;
        };
        let prefix = format!(
            "https://github.com/{REPOSITORY}/releases/download/{}/",
            r.tag_name
        );
        if asset.browser_download_url != format!("{prefix}{filename}")
            || checksum.browser_download_url != format!("{prefix}SHA256SUMS")
            || asset.size == 0
            || asset.size > MAX_DOWNLOAD
        {
            continue;
        }
        candidates.push((
            version.clone(),
            Available {
                version: version.to_string(),
                filename,
                url: asset.browser_download_url.clone(),
                checksum_url: checksum.browser_download_url.clone(),
                bytes: asset.size,
            },
        ));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(candidates.into_iter().next().map(|(_, a)| a))
}
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_per_read(Some(Duration::from_secs(20)))
        .build()
        .into()
}
pub fn check() -> Result<Option<Available>> {
    let mut response = agent()
        .get(format!(
            "https://api.github.com/repos/{REPOSITORY}/releases?per_page=30"
        ))
        .header("User-Agent", "DiskMigrator")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| invalid(format!("Update check failed: {e}")))?;
    let mut raw = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut raw)?;
    if raw.len() > 1024 * 1024 {
        return Err(invalid("Release response is oversized."));
    }
    let releases: Vec<Release> = serde_json::from_slice(&raw)?;
    if !releases.iter().any(|r| !r.draft && !r.prerelease) {
        return Err(invalid("No stable releases have been published yet."));
    }
    select(&releases, env!("CARGO_PKG_VERSION"))
}
pub fn checksum_for(text: &str, filename: &str) -> Result<String> {
    let entries: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let file = parts.next()?.trim_start_matches('*');
            (file == filename && parts.next().is_none()).then_some(hash)
        })
        .collect();
    if entries.len() != 1
        || entries[0].len() != 64
        || !entries[0].bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(invalid(
            "Missing, ambiguous or invalid SHA-256 checksum for the executable.",
        ));
    }
    Ok(entries[0].to_lowercase())
}
pub fn copy_verified(
    mut input: impl Read,
    destination: &Path,
    bytes: u64,
    expected: &str,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    if bytes == 0 || bytes > MAX_DOWNLOAD {
        return Err(invalid("Invalid update size."));
    }
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    let result = (|| -> Result<()> {
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        let mut done = 0;
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done > bytes {
                return Err(invalid("Update exceeds its declared size."));
            }
            hash.update(&buffer[..n]);
            f.write_all(&buffer[..n])?;
            progress(done, bytes);
        }
        if done != bytes || format!("{:x}", hash.finalize()) != expected {
            return Err(invalid("Update size or SHA-256 verification failed."));
        }
        f.sync_all()?;
        Ok(())
    })();
    drop(f);
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}
pub fn download(
    update: &Available,
    parent: &Path,
    progress: impl FnMut(u64, u64),
) -> Result<StagedUpdate> {
    let prefix = format!(
        "https://github.com/{REPOSITORY}/releases/download/v{}/",
        update.version
    );
    if update.filename != format!("DiskMigrator-v{}.exe", update.version)
        || update.url != format!("{prefix}{}", update.filename)
        || update.checksum_url != format!("{prefix}SHA256SUMS")
    {
        return Err(invalid("Update origin is not the canonical release."));
    }
    let client = agent();
    let mut sums = client
        .get(&update.checksum_url)
        .header("User-Agent", "DiskMigrator")
        .call()
        .map_err(|e| invalid(e.to_string()))?;
    let mut raw = Vec::new();
    sums.body_mut()
        .as_reader()
        .take(64 * 1024 + 1)
        .read_to_end(&mut raw)?;
    if raw.len() > 64 * 1024 {
        return Err(invalid("Checksum file is oversized."));
    }
    let checksum = checksum_for(
        std::str::from_utf8(&raw).map_err(|e| invalid(e.to_string()))?,
        &update.filename,
    )?;
    let mut response = client
        .get(&update.url)
        .header("User-Agent", "DiskMigrator")
        .call()
        .map_err(|e| invalid(format!("Download failed: {e}")))?;
    let path = parent.join(format!("{}.download", update.filename));
    copy_verified(
        response.body_mut().as_reader(),
        &path,
        update.bytes,
        &checksum,
        progress,
    )?;
    if let Err(e) = validate_pe(&path) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok(StagedUpdate {
        path,
        sha256: checksum,
    })
}
pub fn validate_pe(path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut f = File::open(path)?;
    let mut dos = [0; 64];
    f.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Err(invalid("Update is not a Windows executable."));
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if offset > 1024 * 1024 || offset + 24 > f.metadata()?.len() {
        return Err(invalid("Invalid PE header offset."));
    }
    f.seek(SeekFrom::Start(offset))?;
    let mut header = [0; 24];
    f.read_exact(&mut header)?;
    if &header[..4] != b"PE\0\0" || header[4..6] != [0x64, 0x86] {
        return Err(invalid(
            "Update must be a compatible Windows x64 executable.",
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub fn launch_installer(staged: &StagedUpdate, update: &Available) -> Result<()> {
    use std::{os::windows::process::CommandExt, process::Command, time::Instant};
    let current = std::env::current_exe()?;
    let parent = current
        .parent()
        .ok_or_else(|| invalid("Executable directory is missing."))?;
    if staged.path.parent() != Some(parent) {
        return Err(invalid("Update must be staged beside the executable."));
    }
    let helper = parent.join(format!(".DiskMigrator-updater-{}.exe", std::process::id()));
    let mut to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&helper)?;
    std::io::copy(&mut File::open(&current)?, &mut to)?;
    to.sync_all()?;
    drop(to);
    let ready = helper.with_extension("ready");
    let error = helper.with_extension("error");
    let mut child = Command::new(&helper)
        .arg("--apply-update")
        .arg(std::process::id().to_string())
        .arg(&current)
        .arg(&staged.path)
        .arg(parent.join(&update.filename))
        .arg(&staged.sha256)
        .arg(&ready)
        .creation_flags(0x08000000)
        .spawn()?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        if ready.exists() {
            let _ = std::fs::remove_file(&ready);
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            let message = std::fs::read_to_string(&error).unwrap_or_else(|_| {
                "The update helper could not start. Installed version was preserved.".into()
            });
            let _ = std::fs::remove_file(&error);
            let _ = std::fs::remove_file(&helper);
            return Err(invalid(message));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&helper);
    Err(invalid(
        "Update helper readiness timed out. Installed version was preserved.",
    ))
}
#[cfg(not(windows))]
pub fn launch_installer(_: &StagedUpdate, _: &Available) -> Result<()> {
    Err(invalid("Portable updates require Windows x64."))
}

fn hash_file(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    let mut f = File::open(path)?;
    let mut buf = [0; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
/// File transaction can be tested using generated files and a fake launcher.
fn replace_portable(
    old: &Path,
    staged: &Path,
    target: &Path,
    mut launch: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let backup = old.with_extension("exe.previous");
    if backup.exists() || target.exists() && target != old {
        return Err(invalid(
            "Update target or backup already exists. Installed version was preserved.",
        ));
    }
    std::fs::rename(old, &backup)?;
    if let Err(e) = std::fs::rename(staged, target) {
        std::fs::rename(&backup, old).map_err(|r| {
            invalid(format!(
                "Update failed: {e}; rollback failed: {r}. Previous executable: {}",
                backup.display()
            ))
        })?;
        return Err(e.into());
    }
    if let Err(e) = launch(target) {
        std::fs::rename(target, staged)?;
        std::fs::rename(&backup, old)?;
        return Err(e);
    }
    Ok(())
}
#[cfg(windows)]
pub fn run_helper(args: &[std::ffi::OsString]) -> Result<()> {
    use std::{os::windows::process::CommandExt, process::Command};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    if args.len() != 8 {
        return Err(invalid("Invalid update helper arguments."));
    }
    let pid: u32 = args[2]
        .to_string_lossy()
        .parse()
        .map_err(|_| invalid("Invalid parent process."))?;
    let old = PathBuf::from(&args[3]);
    let staged = PathBuf::from(&args[4]);
    let target = PathBuf::from(&args[5]);
    let expected_sha = args[6].to_string_lossy();
    let ready = PathBuf::from(&args[7]);
    let helper = std::env::current_exe()?;
    let parent = helper
        .parent()
        .ok_or_else(|| invalid("Missing helper directory."))?;
    if old.parent() != Some(parent)
        || staged.parent() != Some(parent)
        || target.parent() != Some(parent)
        || ready != helper.with_extension("ready")
        || expected_sha.len() != 64
        || !expected_sha.bytes().all(|b| b.is_ascii_hexdigit())
        || !target
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("DiskMigrator-v"))
        || staged
            != target.with_file_name(format!(
                "{}.download",
                target.file_name().unwrap().to_string_lossy()
            ))
    {
        return Err(invalid("Invalid update paths."));
    }
    validate_pe(&staged)?;
    if hash_file(&staged)? != expected_sha {
        return Err(invalid(
            "Staged executable changed after checksum verification.",
        ));
    }
    if old.with_extension("exe.previous").exists() || target.exists() && target != old {
        return Err(invalid("An update target or rollback copy already exists."));
    }
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    // A readiness handshake prevents exiting the app before the helper owns its process handle.
    if let Err(e) = File::create_new(&ready).and_then(|f| f.sync_all()) {
        unsafe { CloseHandle(handle) };
        return Err(e.into());
    }
    let wait = unsafe { WaitForSingleObject(handle, 60_000) };
    unsafe { CloseHandle(handle) };
    if wait != WAIT_OBJECT_0 {
        return Err(invalid(
            "Application did not exit. The installed executable was preserved.",
        ));
    }
    if hash_file(&staged)? != expected_sha {
        return Err(invalid(
            "Staged executable changed while waiting for shutdown.",
        ));
    }
    if let Err(e) = replace_portable(&old, &staged, &target, |exe| {
        Command::new(exe).creation_flags(0x08000000).spawn()?;
        Ok(())
    }) {
        let _ = Command::new(&old)
            .arg("--update-error")
            .arg(e.to_string())
            .creation_flags(0x08000000)
            .spawn();
        return Err(e);
    }
    // Keep the previous executable for manual rollback. Preferences are in a separate user directory.
    Ok(())
}
#[cfg(windows)]
pub fn record_helper_error(error: &crate::Error) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::write(exe.with_extension("error"), error.to_string());
        if let Some(parent) = exe.parent() {
            let _ = std::fs::write(
                parent.join(".DiskMigrator-update-error.txt"),
                error.to_string(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn release(version: &str) -> Release {
        let base = format!("https://github.com/{REPOSITORY}/releases/download/v{version}/");
        Release {
            tag_name: format!("v{version}"),
            draft: false,
            prerelease: false,
            assets: vec![
                Asset {
                    name: format!("DiskMigrator-v{version}.exe"),
                    browser_download_url: format!("{base}DiskMigrator-v{version}.exe"),
                    size: 4096,
                },
                Asset {
                    name: "SHA256SUMS".into(),
                    browser_download_url: format!("{base}SHA256SUMS"),
                    size: 100,
                },
            ],
        }
    }
    #[test]
    fn compares_semver_and_rejects_untrusted_assets() {
        let mut r = release("0.10.0");
        assert_eq!(
            select(&[r.clone()], "0.9.0").unwrap().unwrap().version,
            "0.10.0"
        );
        r.assets[0].browser_download_url = "https://evil.test/update.exe".into();
        assert!(select(&[r], "0.1.0").unwrap().is_none());
        let mut r = release("1.0.0");
        r.prerelease = true;
        assert!(select(&[r], "0.1.0").unwrap().is_none());
    }
    #[test]
    fn opt_out_makes_no_startup_request() {
        let p = crate::preferences::Preferences {
            startup_updates: false,
            ..Default::default()
        };
        assert!(!startup_check_enabled(&p));
    }
    #[test]
    fn verifies_download_and_removes_bad_payload() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("update.download");
        let bytes = b"fixture executable";
        let sha = crate::digest(bytes);
        copy_verified(&bytes[..], &path, bytes.len() as u64, &sha, |_, _| {}).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let bad = d.path().join("bad.download");
        assert!(
            copy_verified(
                &bytes[..],
                &bad,
                bytes.len() as u64,
                &"0".repeat(64),
                |_, _| {}
            )
            .is_err()
        );
        assert!(!bad.exists());
        assert!(checksum_for("broken", "a.exe").is_err());
    }
    #[test]
    fn portable_replacement_rolls_back_when_fake_launch_fails() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("old.exe");
        let staged = d.path().join("new.download");
        let target = d.path().join("new.exe");
        std::fs::write(&old, b"working version").unwrap();
        std::fs::write(&staged, b"new version").unwrap();
        assert!(
            replace_portable(&old, &staged, &target, |_| Err(invalid(
                "fake launch failure"
            )))
            .is_err()
        );
        assert_eq!(std::fs::read(&old).unwrap(), b"working version");
        assert!(!target.exists());
        assert!(staged.exists());
        replace_portable(&old, &staged, &target, |_| Ok(())).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new version");
        assert_eq!(
            std::fs::read(old.with_extension("exe.previous")).unwrap(),
            b"working version"
        );
        assert_eq!(hash_file(&target).unwrap(), crate::digest(b"new version"));
    }
    #[test]
    fn checksum_duplicates_truncation_and_pe_architecture_are_rejected() {
        let hash = "a".repeat(64);
        assert!(checksum_for(&format!("{hash} a.exe\n{hash} a.exe"), "a.exe").is_err());
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("bad.download");
        assert!(copy_verified(&b"tiny"[..], &path, 12, &hash, |_, _| {}).is_err());
        assert!(!path.exists());
        let mut pe = vec![0; 256];
        pe[..2].copy_from_slice(b"MZ");
        pe[60..64].copy_from_slice(&64u32.to_le_bytes());
        pe[64..68].copy_from_slice(b"PE\0\0");
        std::fs::write(&path, &pe).unwrap();
        assert!(validate_pe(&path).is_err());
        pe[68..70].copy_from_slice(&[0x64, 0x86]);
        std::fs::write(&path, &pe).unwrap();
        validate_pe(&path).unwrap();
    }
}
