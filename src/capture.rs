//! Export a consistent source into a reproducible, regular raw image without mounting it.
use crate::{
    Error, Result, image, invalid, layout,
    migration::{Assessment, Phase, Progress},
    physical::{BlockSource, Consistency},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureVerification {
    pub bytes: u64,
    pub sha256: String,
    pub consistency: Consistency,
    pub partition_table_valid: bool,
    pub filesystem_consistency: Assessment,
    pub bootability: Assessment,
}
pub fn capture_image(
    source: &mut impl BlockSource,
    path: &Path,
    cancel: &AtomicBool,
    mut notify: impl FnMut(Progress),
) -> Result<CaptureVerification> {
    source.validate(cancel)?;
    let info = source.info();
    layout::valid_size(info.bytes, info.sector)?;
    if info.bytes != source.layout().bytes || info.sector != source.layout().sector_size {
        return Err(invalid(
            "Capture source geometry disagrees with its partition table.",
        ));
    }
    image::validate_path(path)?;
    let mut file = image::open_destination(path, None, info.bytes)?;
    let result = (|| -> Result<CaptureVerification> {
        let mut buffer = vec![0; 4 * 1024 * 1024];
        let mut hash = Sha256::new();
        let mut done = 0;
        while done < info.bytes {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let n = (info.bytes - done).min(buffer.len() as u64) as usize;
            source.read_at(done, &mut buffer[..n])?;
            file.write_all(&buffer[..n])?;
            hash.update(&buffer[..n]);
            done += n as u64;
            notify(Progress {
                phase: Phase::Copying,
                done,
                total: info.bytes,
            });
        }
        file.sync_all()?;
        source.validate(cancel)?;
        let sha = format!("{:x}", hash.finalize());
        let mut verify = Sha256::new();
        file.seek(SeekFrom::Start(0))?;
        done = 0;
        while done < info.bytes {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let n = (info.bytes - done).min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..n])?;
            verify.update(&buffer[..n]);
            done += n as u64;
            notify(Progress {
                phase: Phase::Verifying,
                done,
                total: info.bytes,
            });
        }
        if format!("{:x}", verify.finalize()) != sha {
            return Err(invalid("Captured image SHA-256 read-back mismatch."));
        }
        let actual = layout::parse(&mut file, info.bytes, info.sector)?;
        if crate::digest(&serde_json::to_vec(&actual)?)
            != crate::digest(&serde_json::to_vec(source.layout())?)
        {
            return Err(invalid(
                "Captured image partition table differs from the consistent source.",
            ));
        }
        source.validate(cancel)?;
        Ok(CaptureVerification {
            bytes: info.bytes,
            sha256: sha,
            consistency: info.consistency,
            partition_table_valid: true,
            filesystem_consistency: Assessment::NotTested,
            bootability: Assessment::NotTested,
        })
    })();
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}
