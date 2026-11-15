use disk_migrator::{
    Error, image,
    layout::{self, Role, Scheme},
    migration::{self, Assessment, Phase},
    plan::Plan,
};
use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    sync::atomic::{AtomicBool, Ordering},
};
use tempfile::TempDir;

fn fixture(dir: &TempDir, gpt: bool, sector: u32) -> std::path::PathBuf {
    let path = dir.path().join("source.img");
    let bytes = 12 * 1024 * 1024;
    let count = bytes / sector as u64;
    let mut f = File::create(&path).unwrap();
    f.set_len(bytes).unwrap();
    let mut mbr = vec![0; sector as usize];
    mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
    let start = 1024 * 1024 / sector as u64;
    mbr[446] = 0x80;
    mbr[450] = if gpt { 0xee } else { 7 };
    mbr[454..458].copy_from_slice(&(if gpt { 1 } else { start } as u32).to_le_bytes());
    mbr[458..462].copy_from_slice(&(if gpt { count - 1 } else { start * 2 } as u32).to_le_bytes());
    f.write_all(&mbr).unwrap();
    if gpt {
        let mut entries = vec![0; 128 * 128];
        entries[..16].copy_from_slice(&[
            0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26,
            0x99, 0xc7,
        ]);
        entries[16..32].fill(0x31);
        entries[32..40].copy_from_slice(&start.to_le_bytes());
        entries[40..48].copy_from_slice(&(start * 3 - 1).to_le_bytes());
        for (i, c) in "Windows".encode_utf16().enumerate() {
            entries[56 + 2 * i..58 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        let blocks = (entries.len() as u64).div_ceil(sector as u64);
        let mut header = vec![0; sector as usize];
        header[..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x10000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[24..32].copy_from_slice(&1u64.to_le_bytes());
        header[32..40].copy_from_slice(&(count - 1).to_le_bytes());
        header[40..48].copy_from_slice(&(2 + blocks).to_le_bytes());
        header[48..56].copy_from_slice(&(count - 2 - blocks).to_le_bytes());
        header[56..72].fill(7);
        header[72..80].copy_from_slice(&2u64.to_le_bytes());
        header[80..84].copy_from_slice(&128u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        header[88..92].copy_from_slice(&crc32fast::hash(&entries).to_le_bytes());
        layout::header_crc(&mut header);
        f.seek(SeekFrom::Start(sector as u64)).unwrap();
        f.write_all(&header).unwrap();
        f.write_all(&entries).unwrap();
        header[24..32].copy_from_slice(&(count - 1).to_le_bytes());
        header[32..40].copy_from_slice(&1u64.to_le_bytes());
        header[72..80].copy_from_slice(&(count - 1 - blocks).to_le_bytes());
        layout::header_crc(&mut header);
        f.seek(SeekFrom::Start((count - 1 - blocks) * sector as u64))
            .unwrap();
        f.write_all(&entries).unwrap();
        f.seek(SeekFrom::Start((count - 1) * sector as u64))
            .unwrap();
        f.write_all(&header).unwrap();
    }
    f.seek(SeekFrom::Start(start * sector as u64)).unwrap();
    let data: Vec<u8> = (0..1024 * 1024).map(|i| (i * 17 % 251) as u8).collect();
    f.write_all(&data).unwrap();
    f.sync_all().unwrap();
    path
}
fn plan(dir: &TempDir, gpt: bool, sector: u32, extra: u64) -> Plan {
    let source = fixture(dir, gpt, sector);
    let source = image::inspect(&source, sector).unwrap();
    let bytes = source.identity.bytes + extra;
    Plan::build(source, &dir.path().join("new-ssd.img"), bytes).unwrap()
}
#[test]
fn parses_primary_mbr_and_both_sector_sizes() {
    for sector in [512, 4096] {
        let dir = TempDir::new().unwrap();
        let p = plan(&dir, false, sector, 0);
        assert_eq!(p.source.layout.scheme, Scheme::Mbr);
        assert!(p.source.layout.partitions[0].aligned);
    }
}
#[test]
fn parses_gpt_roles_name_and_backup() {
    let d = TempDir::new().unwrap();
    let p = plan(&d, true, 512, 0);
    assert_eq!(p.source.layout.partitions[0].role, Role::Data);
    assert_eq!(p.source.layout.partitions[0].name, "Windows");
}
#[test]
fn copies_gpt_and_mbr_to_equal_or_larger_images() {
    for gpt in [false, true] {
        for sector in [512, 4096] {
            for extra in [0, sector as u64, 2 * 1024 * 1024] {
                let d = TempDir::new().unwrap();
                let p = plan(&d, gpt, sector, extra);
                let v =
                    migration::execute(&p, "new-ssd.img", false, &AtomicBool::new(false), |_| {})
                        .unwrap();
                assert!(v.data_matches && v.partition_table_valid);
                assert_eq!(v.bootability, Assessment::NotTested);
                assert_eq!(v.filesystem_consistency, Assessment::NotTested);
                let target = image::inspect(&p.destination, sector).unwrap();
                assert_eq!(target.layout.partitions, p.source.layout.partitions);
                if gpt {
                    assert_eq!(
                        layout::u64_at(&target.layout.gpt.unwrap().primary, 32),
                        p.destination_bytes / sector as u64 - 1
                    );
                }
            }
        }
    }
}
#[test]
fn rejects_same_file_and_hard_links() {
    let d = TempDir::new().unwrap();
    let source = fixture(&d, false, 512);
    let s = image::inspect(&source, 512).unwrap();
    assert!(Plan::build(s.clone(), &source, s.identity.bytes).is_err());
    let alias = d.path().join("alias.img");
    std::fs::hard_link(&source, &alias).unwrap();
    assert!(Plan::build(s.clone(), &alias, s.identity.bytes).is_err());
}
#[test]
fn rejects_small_target_and_device_namespace() {
    let d = TempDir::new().unwrap();
    let p = plan(&d, false, 512, 0);
    assert!(Plan::build(p.source.clone(), &p.destination, 1024 * 1024).is_err());
    assert!(image::validate_path(std::path::Path::new(r"\\.\PhysicalDrive0")).is_err());
}
#[test]
fn rejects_bad_crc_and_out_of_bounds_mbr() {
    let d = TempDir::new().unwrap();
    let path = fixture(&d, true, 512);
    let mut f = OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(512 + 16)).unwrap();
    f.write_all(&[0; 4]).unwrap();
    drop(f);
    assert!(image::inspect(&path, 512).is_err());
    let path = fixture(&d, false, 512);
    let mut f = OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(458)).unwrap();
    f.write_all(&u32::MAX.to_le_bytes()).unwrap();
    drop(f);
    assert!(image::inspect(&path, 512).is_err());
}
#[test]
fn rejects_backup_table_corruption_and_oversized_array() {
    let d = TempDir::new().unwrap();
    let p = plan(&d, true, 512, 0);
    let mut f = OpenOptions::new().write(true).open(&p.source.path).unwrap();
    f.seek(SeekFrom::Start(
        p.source.layout.gpt.as_ref().unwrap().backup_entries_lba * 512,
    ))
    .unwrap();
    f.write_all(&[0xff]).unwrap();
    drop(f);
    assert!(image::inspect(&p.source.path, 512).is_err());
    let mut raw = std::io::Cursor::new(vec![0; 4096]);
    assert!(layout::parse(&mut raw, u64::MAX, 512).is_err());
}
#[test]
fn cancellation_resumes_from_reopened_review_plan() {
    let d = TempDir::new().unwrap();
    let p = plan(&d, true, 512, 1024 * 1024);
    let cancel = AtomicBool::new(false);
    let r = migration::execute(&p, "new-ssd.img", false, &cancel, |v| {
        if v.phase == Phase::Copying {
            cancel.store(true, Ordering::Relaxed)
        }
    });
    assert!(matches!(r, Err(Error::Cancelled)));
    let reopened = Plan::build(
        image::inspect(&p.source.path, 512).unwrap(),
        &p.destination,
        p.destination_bytes,
    )
    .unwrap();
    assert_eq!(reopened.id, p.id);
    let mut resumed = false;
    migration::execute(
        &reopened,
        "new-ssd.img",
        true,
        &AtomicBool::new(false),
        |p| {
            if p.phase == Phase::Resuming {
                resumed = true
            }
        },
    )
    .unwrap();
    assert!(resumed);
}
#[test]
fn rejects_corrupt_checkpoint_and_modified_committed_bytes() {
    for corrupt_checkpoint in [false, true] {
        let d = TempDir::new().unwrap();
        let p = plan(&d, false, 512, 0);
        let c = AtomicBool::new(false);
        let _ = migration::execute(&p, "new-ssd.img", false, &c, |v| {
            if v.phase == Phase::Copying {
                c.store(true, Ordering::Relaxed)
            }
        });
        if corrupt_checkpoint {
            std::fs::write(p.checkpoint_path(), b"broken").unwrap();
        } else {
            let mut f = OpenOptions::new().write(true).open(&p.destination).unwrap();
            f.seek(SeekFrom::Start(2 * 1024 * 1024)).unwrap();
            f.write_all(&[99]).unwrap();
        }
        assert!(
            migration::execute(&p, "new-ssd.img", true, &AtomicBool::new(false), |_| {}).is_err()
        );
    }
}
#[test]
fn confirmation_and_changed_source_fail_before_creating_target() {
    let d = TempDir::new().unwrap();
    let p = plan(&d, false, 512, 0);
    assert!(migration::execute(&p, "wrong.img", false, &AtomicBool::new(false), |_| {}).is_err());
    assert!(!p.destination.exists());
    let mut f = OpenOptions::new().write(true).open(&p.source.path).unwrap();
    f.seek(SeekFrom::Start(1024 * 1024)).unwrap();
    f.write_all(&[33]).unwrap();
    drop(f);
    assert!(migration::execute(&p, "new-ssd.img", false, &AtomicBool::new(false), |_| {}).is_err());
    assert!(!p.destination.exists());
}

#[test]
fn clears_and_verifies_existing_destination_tail() {
    let d = TempDir::new().unwrap();
    let base = plan(&d, true, 512, 1024 * 1024);
    let mut file = File::create(&base.destination).unwrap();
    file.write_all(&vec![0x7f; base.destination_bytes as usize])
        .unwrap();
    drop(file);
    let p = Plan::build(
        base.source.clone(),
        &base.destination,
        base.destination_bytes,
    )
    .unwrap();
    let before = std::fs::read(&p.source.path).unwrap();
    let v = migration::execute(&p, "new-ssd.img", false, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(v.verified_bytes, p.destination_bytes);
    let after = std::fs::read(&p.destination).unwrap();
    assert!(
        after[p.source.identity.bytes as usize..after.len() - 32 * 1024]
            .iter()
            .all(|b| *b == 0)
    );
    assert_eq!(std::fs::read(&p.source.path).unwrap(), before);
}

#[test]
fn rejects_partition_overlap_extended_and_hybrid_layouts() {
    let d = TempDir::new().unwrap();
    let path = fixture(&d, false, 512);
    let mut bytes = std::fs::read(&path).unwrap();
    let entry = bytes[446..462].to_vec();
    bytes[462..478].copy_from_slice(&entry);
    assert!(
        layout::parse(&mut std::io::Cursor::new(&bytes), bytes.len() as u64, 512)
            .unwrap_err()
            .to_string()
            .contains("overlap")
    );
    bytes[466] = 0x0f;
    assert!(
        layout::parse(&mut std::io::Cursor::new(&bytes), bytes.len() as u64, 512)
            .unwrap_err()
            .to_string()
            .contains("Extended")
    );
    let path = fixture(&d, true, 512);
    let mut bytes = std::fs::read(path).unwrap();
    bytes[466] = 7;
    assert!(
        layout::parse(&mut std::io::Cursor::new(&bytes), bytes.len() as u64, 512)
            .unwrap_err()
            .to_string()
            .contains("Hybrid")
    );
}

#[test]
fn unaligned_partition_is_preserved_without_resize() {
    let d = TempDir::new().unwrap();
    let path = fixture(&d, false, 512);
    let mut f = OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(454)).unwrap();
    f.write_all(&2049u32.to_le_bytes()).unwrap();
    drop(f);
    let source = image::inspect(&path, 512).unwrap();
    assert!(!source.layout.partitions[0].aligned);
    let p = Plan::build(source, &d.path().join("new-ssd.img"), 12 * 1024 * 1024).unwrap();
    migration::execute(&p, "new-ssd.img", false, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(
        image::inspect(&p.destination, 512)
            .unwrap()
            .layout
            .partitions[0]
            .first_lba,
        2049
    );
}

#[test]
fn rejects_oversized_gpt_table_before_allocation() {
    let d = TempDir::new().unwrap();
    let p = plan(&d, true, 512, 0);
    let mut bytes = std::fs::read(&p.source.path).unwrap();
    let n = bytes.len();
    for start in [512, n - 512] {
        bytes[start + 80..start + 84].copy_from_slice(&u32::MAX.to_le_bytes());
        layout::header_crc(&mut bytes[start..start + 512]);
    }
    let error = layout::parse(&mut std::io::Cursor::new(&bytes), bytes.len() as u64, 512)
        .unwrap_err()
        .to_string();
    assert!(error.contains("oversized"));
}

#[test]
fn rejects_modified_plan_and_replaced_destination() {
    let d = TempDir::new().unwrap();
    let mut p = plan(&d, true, 512, 1024 * 1024);
    p.patches[0].bytes[0] = 8;
    assert!(migration::execute(&p, "new-ssd.img", false, &AtomicBool::new(false), |_| {}).is_err());
    assert!(!p.destination.exists());
    let d = TempDir::new().unwrap();
    let p = plan(&d, false, 512, 0);
    File::create(&p.destination)
        .unwrap()
        .set_len(p.destination_bytes)
        .unwrap();
    let p = Plan::build(p.source.clone(), &p.destination, p.destination_bytes).unwrap();
    std::fs::rename(&p.destination, d.path().join("old-destination.img")).unwrap();
    File::create(&p.destination)
        .unwrap()
        .set_len(p.destination_bytes)
        .unwrap();
    assert!(migration::execute(&p, "new-ssd.img", false, &AtomicBool::new(false), |_| {}).is_err());
    assert_eq!(std::fs::read(&p.destination).unwrap()[510], 0);
}

#[test]
fn can_resume_after_cancelling_final_verification() {
    let d = TempDir::new().unwrap();
    let p = plan(&d, true, 4096, 1024 * 1024);
    let cancel = AtomicBool::new(false);
    let result = migration::execute(&p, "new-ssd.img", false, &cancel, |v| {
        if v.phase == Phase::Verifying {
            cancel.store(true, Ordering::Relaxed)
        }
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    migration::execute(&p, "new-ssd.img", true, &AtomicBool::new(false), |_| {}).unwrap();
}

#[test]
fn interrupted_atomic_save_does_not_block_future_checkpoint() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("checkpoint.json");
    std::fs::write(d.path().join("checkpoint.json.tmp"), b"interrupted").unwrap();
    migration::atomic_json(&path, b"first").unwrap();
    migration::atomic_json(&path, b"second").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"second");
}

#[test]
fn recognizes_windows_gpt_partition_roles_without_claiming_bootability() {
    let d = TempDir::new().unwrap();
    let p = plan(&d, true, 512, 0);
    let mut raw = std::fs::read(&p.source.path).unwrap();
    let ids = [
        [
            0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0, 0xa0, 0xc9, 0x3e, 0xc9,
            0x3b,
        ],
        [
            0x16, 0xe3, 0xc9, 0xe3, 0x5c, 0x0b, 0xb8, 0x4d, 0x81, 0x7d, 0xf9, 0x2d, 0xf, 0, 0x15,
            0xae,
        ],
        [
            0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26,
            0x99, 0xc7,
        ],
        [
            0xa4, 0xbb, 0x94, 0xde, 0xd1, 0x06, 0x40, 0x4d, 0xa1, 0x6a, 0xbf, 0xd5, 0x01, 0x79,
            0xd6, 0xac,
        ],
    ];
    let mut entries = vec![0; 128 * 128];
    for (i, id) in ids.into_iter().enumerate() {
        let e = &mut entries[i * 128..(i + 1) * 128];
        e[..16].copy_from_slice(&id);
        e[16..32].fill(i as u8 + 1);
        let first = 2048 + i as u64 * 4096;
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&(first + 2047).to_le_bytes());
    }
    let backup = p.source.layout.gpt.as_ref().unwrap().backup_entries_lba as usize * 512;
    raw[1024..1024 + entries.len()].copy_from_slice(&entries);
    raw[backup..backup + entries.len()].copy_from_slice(&entries);
    let end = raw.len();
    for at in [512, end - 512] {
        raw[at + 88..at + 92].copy_from_slice(&crc32fast::hash(&entries).to_le_bytes());
        layout::header_crc(&mut raw[at..at + 512]);
    }
    std::fs::write(&p.source.path, raw).unwrap();
    let source = image::inspect(&p.source.path, 512).unwrap();
    assert_eq!(
        source
            .layout
            .partitions
            .iter()
            .map(|p| p.role.clone())
            .collect::<Vec<_>>(),
        [Role::Efi, Role::Reserved, Role::Data, Role::Recovery]
    );
    let p = Plan::build(source, &p.destination, p.destination_bytes).unwrap();
    let v = migration::execute(&p, "new-ssd.img", false, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(v.bootability, Assessment::NotTested);
}
