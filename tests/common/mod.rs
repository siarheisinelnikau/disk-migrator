use disk_migrator::layout;
use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
};
use tempfile::TempDir;
pub fn fixture(dir: &TempDir, gpt: bool, sector: u32) -> std::path::PathBuf {
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
