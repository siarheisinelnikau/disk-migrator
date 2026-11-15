//! Durability order: write chunk -> sync target -> atomic checkpoint replacement.
//! Resume re-reads the entire committed prefix; it never trusts an offset alone.
use crate::{Error, Result, digest, image, invalid, layout, plan::Plan};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const CHUNK: usize = 4 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Fingerprinting,
    Resuming,
    Copying,
    Verifying,
}
#[derive(Clone, Debug)]
pub struct Progress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Verification {
    pub source_sha256: String,
    pub verified_bytes: u64,
    pub partition_table_valid: bool,
    pub data_matches: bool,
    pub filesystem_consistency: Assessment,
    pub bootability: Assessment,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Assessment {
    NotTested,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    plan_id: String,
    source_sha256: String,
    destination: image::Identity,
    copied: u64,
    checksum: String,
}
impl Checkpoint {
    fn sum(&self) -> Result<String> {
        let mut c = self.clone();
        c.checksum.clear();
        Ok(digest(&serde_json::to_vec(&c)?))
    }
    fn save(&mut self, path: &Path) -> Result<()> {
        self.checksum = self.sum()?;
        atomic_json(path, &serde_json::to_vec(self)?)
    }
    fn load(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        if file.metadata()?.len() > 16 * 1024 {
            return Err(invalid("Checkpoint is oversized."));
        }
        let c: Self = serde_json::from_reader(file)?;
        if c.version != 1 || c.checksum != c.sum()? {
            return Err(invalid("Checkpoint is corrupt or unsupported."));
        }
        Ok(c)
    }
}
pub fn atomic_json(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp = path.as_os_str().to_os_string();
    // An interrupted previous save must not block the next checkpoint forever.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| invalid(e.to_string()))?
        .as_nanos();
    temp.push(format!(".tmp.{}.{nonce}", std::process::id()));
    let temp = std::path::PathBuf::from(temp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| -> Result<()> {
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{
                MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
            };
            let a: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
            let b: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            if unsafe {
                MoveFileExW(
                    a.as_ptr(),
                    b.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(windows))]
        {
            std::fs::rename(&temp, path)?;
            File::open(path.parent().unwrap())?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
fn fingerprint(
    file: &mut File,
    total: u64,
    cancel: &AtomicBool,
    notify: &mut impl FnMut(Progress),
) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0; CHUNK];
    let mut done = 0;
    while done < total {
        cancelled(cancel)?;
        let n = (total - done).min(CHUNK as u64) as usize;
        file.read_exact(&mut buf[..n])?;
        hash.update(&buf[..n]);
        done += n as u64;
        notify(Progress {
            phase: Phase::Fingerprinting,
            done,
            total,
        });
    }
    Ok(format!("{:x}", hash.finalize()))
}
pub fn execute(
    plan: &Plan,
    confirmation: &str,
    resume: bool,
    cancel: &AtomicBool,
    mut notify: impl FnMut(Progress),
) -> Result<Verification> {
    if confirmation != plan.confirmation_name() {
        return Err(invalid(
            "Type the exact destination filename to authorize overwriting it.",
        ));
    }
    // Rebuild, do not trust externally altered serialized plans or patch bytes.
    let rebuilt = Plan::build(
        plan.source.clone(),
        &plan.destination,
        plan.destination_bytes,
    )?;
    // A new file becomes an existing one after an interruption.
    let mut normalized = rebuilt.clone();
    normalized.destination_identity = plan.destination_identity.clone();
    if normalized.calculated_id()? != plan.id || plan.calculated_id()? != plan.id {
        return Err(invalid("Migration plan was changed; review it again."));
    }
    let mut source = image::open_source(&plan.source.path)?;
    if image::identity(&source)? != plan.source.identity
        || image::modified(&source)? != plan.source.modified_ns
    {
        return Err(invalid("Source changed since inspection."));
    }
    let current = layout::parse(
        &mut source,
        plan.source.identity.bytes,
        plan.source.layout.sector_size,
    )?;
    if digest(&serde_json::to_vec(&current)?) != plan.source.metadata_hash {
        return Err(invalid("Source partition metadata changed."));
    }
    let cp_path = plan.checkpoint_path();
    if cp_path.exists() && !resume {
        return Err(invalid(
            "A checkpoint exists. Resume it, or explicitly remove the old destination/checkpoint before starting over.",
        ));
    }
    if resume && !cp_path.exists() {
        return Err(invalid("No checkpoint exists for this destination."));
    }
    let sha = fingerprint(&mut source, plan.source.identity.bytes, cancel, &mut notify)?;
    let mut checkpoint = if resume {
        Some(Checkpoint::load(&cp_path)?)
    } else {
        None
    };
    if let Some(c) = &checkpoint
        && (c.plan_id != plan.id
            || c.source_sha256 != sha
            || c.copied > plan.destination_bytes
            || (c.copied != plan.destination_bytes && !c.copied.is_multiple_of(CHUNK as u64)))
    {
        return Err(invalid(
            "Checkpoint does not match this plan, source content, or copy boundary.",
        ));
    }
    let expected = checkpoint
        .as_ref()
        .map(|c| &c.destination)
        .or(plan.destination_identity.as_ref());
    // Read identity before obtaining any write handle, including hard-link aliases.
    if plan.destination.exists()
        && image::identity(&image::open_source(&plan.destination)?)?.key == plan.source.identity.key
    {
        return Err(invalid("Destination aliases the source."));
    }
    let mut target = image::open_destination(&plan.destination, expected, plan.destination_bytes)?;
    let target_id = image::identity(&target)?;
    let mut c = checkpoint.take().unwrap_or(Checkpoint {
        version: 1,
        plan_id: plan.id.clone(),
        source_sha256: sha.clone(),
        destination: target_id,
        copied: 0,
        checksum: String::new(),
    });
    if !resume {
        c.save(&cp_path)?;
    }
    let total = plan.destination_bytes;
    let mut buf = vec![0; CHUNK];
    let mut compare = vec![0; CHUNK];
    let mut done = 0;
    while done < c.copied {
        cancelled(cancel)?;
        let n = (c.copied - done).min(CHUNK as u64) as usize;
        expected_chunk(plan, &mut source, done, &mut buf[..n])?;
        target.seek(SeekFrom::Start(done))?;
        target.read_exact(&mut compare[..n])?;
        if buf[..n] != compare[..n] {
            return Err(invalid(format!(
                "Resume verification failed at byte {done}. The committed destination was modified; it has not been overwritten."
            )));
        }
        done += n as u64;
        notify(Progress {
            phase: Phase::Resuming,
            done,
            total: c.copied,
        });
    }
    while done < total {
        cancelled(cancel)?;
        let n = (total - done).min(CHUNK as u64) as usize;
        expected_chunk(plan, &mut source, done, &mut buf[..n])?;
        target.seek(SeekFrom::Start(done))?;
        target.write_all(&buf[..n])?;
        target.sync_data()?;
        done += n as u64;
        c.copied = done;
        c.save(&cp_path)?;
        notify(Progress {
            phase: Phase::Copying,
            done,
            total,
        });
    }
    target.sync_all()?;
    let report = verify_handles(plan, &mut source, &mut target, &sha, cancel, &mut notify)?;
    if image::modified(&source)? != plan.source.modified_ns {
        return Err(invalid(
            "Source changed during copying; result is not certified.",
        ));
    }
    // Keep the checkpoint as an audit/resume record. Completion is reverified, never trusted.
    Ok(report)
}
fn expected_chunk(plan: &Plan, source: &mut File, offset: u64, buf: &mut [u8]) -> Result<()> {
    buf.fill(0);
    let n = plan
        .source
        .identity
        .bytes
        .saturating_sub(offset)
        .min(buf.len() as u64) as usize;
    if n > 0 {
        source.seek(SeekFrom::Start(offset))?;
        source.read_exact(&mut buf[..n])?;
    }
    plan.overlay(offset, buf);
    Ok(())
}
fn verify_handles(
    plan: &Plan,
    source: &mut File,
    target: &mut File,
    sha: &str,
    cancel: &AtomicBool,
    notify: &mut impl FnMut(Progress),
) -> Result<Verification> {
    let total = plan.destination_bytes;
    let mut done = 0;
    let mut a = vec![0; CHUNK];
    let mut b = vec![0; CHUNK];
    let mut raw_hash = Sha256::new();
    while done < total {
        cancelled(cancel)?;
        let n = (total - done).min(CHUNK as u64) as usize;
        a[..n].fill(0);
        let source_n = plan
            .source
            .identity
            .bytes
            .saturating_sub(done)
            .min(n as u64) as usize;
        if source_n > 0 {
            source.seek(SeekFrom::Start(done))?;
            source.read_exact(&mut a[..source_n])?;
            raw_hash.update(&a[..source_n]);
        }
        plan.overlay(done, &mut a[..n]);
        target.seek(SeekFrom::Start(done))?;
        target.read_exact(&mut b[..n])?;
        if digest(&a[..n]) != digest(&b[..n]) {
            return Err(invalid(format!(
                "Destination data verification failed at byte {done}."
            )));
        }
        done += n as u64;
        notify(Progress {
            phase: Phase::Verifying,
            done,
            total,
        });
    }
    if format!("{:x}", raw_hash.finalize()) != sha {
        return Err(invalid("Source content changed during migration."));
    }
    let actual = layout::parse(
        target,
        plan.destination_bytes,
        plan.source.layout.sector_size,
    )?;
    if actual.partitions != plan.source.layout.partitions
        || actual.scheme != plan.source.layout.scheme
    {
        return Err(invalid("Destination partition plan mismatch."));
    }
    Ok(Verification {
        source_sha256: sha.to_owned(),
        verified_bytes: total,
        partition_table_valid: true,
        data_matches: true,
        filesystem_consistency: Assessment::NotTested,
        bootability: Assessment::NotTested,
    })
}
