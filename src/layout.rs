//! Strict, allocation-bounded GPT and primary MBR parsing. No repair is implicit.
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};

const MAX_TABLE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Efi,
    Reserved,
    Data,
    Recovery,
    System,
    Other,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partition {
    pub index: u32,
    pub first_lba: u64,
    pub last_lba: u64,
    pub name: String,
    pub role: Role,
    pub aligned: bool,
}
impl Partition {
    pub fn bytes(&self, sector: u32) -> u64 {
        (self.last_lba - self.first_lba + 1) * sector as u64
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scheme {
    Mbr,
    Gpt,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gpt {
    pub primary: Vec<u8>,
    pub backup: Vec<u8>,
    pub entries: Vec<u8>,
    pub primary_entries_lba: u64,
    pub backup_entries_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layout {
    pub scheme: Scheme,
    pub sector_size: u32,
    pub bytes: u64,
    pub mbr: Vec<u8>,
    pub partitions: Vec<Partition>,
    pub gpt: Option<Gpt>,
}

pub fn u32_at(bytes: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap())
}
pub fn u64_at(bytes: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap())
}
pub fn read_at<R: Read + Seek>(reader: &mut R, offset: u64, size: usize) -> Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}
pub fn valid_size(bytes: u64, sector: u32) -> Result<()> {
    if ![512, 4096].contains(&sector)
        || bytes < 6 * sector as u64
        || bytes > MAX_IMAGE_BYTES
        || !bytes.is_multiple_of(sector as u64)
    {
        return Err(invalid(
            "Image must be sector-aligned, 6 sectors–16 TiB, with 512 or 4096 byte sectors.",
        ));
    }
    Ok(())
}

pub fn parse<R: Read + Seek>(reader: &mut R, bytes: u64, sector: u32) -> Result<Layout> {
    valid_size(bytes, sector)?;
    let count = bytes / sector as u64;
    let mbr = read_at(reader, 0, sector as usize)?;
    if mbr[510..512] != [0x55, 0xaa] {
        return Err(invalid("Missing MBR signature."));
    }
    let protective: Vec<_> = (0..4).filter(|i| mbr[446 + i * 16 + 4] == 0xee).collect();
    if !protective.is_empty() {
        if protective.len() != 1 || (0..4).any(|i| i != protective[0] && mbr[446 + i * 16 + 4] != 0)
        {
            return Err(invalid("Hybrid GPT/MBR layouts are unsupported."));
        }
        let p = 446 + protective[0] * 16;
        if u32_at(&mbr, p + 8) != 1
            || u32_at(&mbr, p + 12) as u64 != (count - 1).min(u32::MAX as u64)
        {
            return Err(invalid("Invalid protective MBR range."));
        }
        let primary = read_at(reader, sector as u64, sector as usize)?;
        check_header(&primary, 1, count - 1, count, sector)?;
        let backup = read_at(reader, (count - 1) * sector as u64, sector as usize)?;
        check_header(&backup, count - 1, 1, count, sector)?;
        for (pos, len) in [(40, 32), (80, 12)] {
            if primary[pos..pos + len] != backup[pos..pos + len] {
                return Err(invalid("GPT primary/backup fields disagree."));
            }
        }
        let n = u32_at(&primary, 80) as u64;
        let entry_size = u32_at(&primary, 84) as u64;
        let table_bytes = n
            .checked_mul(entry_size)
            .ok_or_else(|| invalid("GPT table overflow."))?;
        if n == 0
            || n > 131072
            || entry_size < 128
            || !entry_size.is_power_of_two()
            || !(16 * 1024..=MAX_TABLE_BYTES).contains(&table_bytes)
        {
            return Err(invalid("Unsupported or oversized GPT entry array."));
        }
        let blocks = table_bytes.div_ceil(sector as u64);
        let first = u64_at(&primary, 40);
        let last = u64_at(&primary, 48);
        let pe = u64_at(&primary, 72);
        let be = u64_at(&backup, 72);
        if first > last
            || pe < 2
            || pe.checked_add(blocks).is_none_or(|v| v > first)
            || be <= last
            || be.checked_add(blocks).is_none_or(|v| v > count - 1)
        {
            return Err(invalid(
                "GPT metadata overlaps usable space or exceeds the image.",
            ));
        }
        let entries = read_at(reader, pe * sector as u64, table_bytes as usize)?;
        let backup_entries = read_at(reader, be * sector as u64, table_bytes as usize)?;
        if crc32fast::hash(&entries) != u32_at(&primary, 88) || entries != backup_entries {
            return Err(invalid("GPT partition array CRC or backup mismatch."));
        }
        let mut partitions = Vec::new();
        let mut ids = std::collections::HashSet::new();
        for (i, entry) in entries.chunks_exact(entry_size as usize).enumerate() {
            if entry[..16] == [0; 16] {
                continue;
            }
            if entry[16..32] == [0; 16] || !ids.insert(entry[16..32].to_vec()) {
                return Err(invalid("Missing or duplicate partition GUID."));
            }
            let a = u64_at(entry, 32);
            let b = u64_at(entry, 40);
            if a < first || b > last || a > b {
                return Err(invalid("Partition lies outside GPT usable space."));
            }
            let name: Vec<u16> = entry[56..128]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|c| *c != 0)
                .collect();
            partitions.push(Partition {
                index: i as u32 + 1,
                first_lba: a,
                last_lba: b,
                name: String::from_utf16_lossy(&name),
                role: gpt_role(&entry[..16]),
                aligned: (a * sector as u64).is_multiple_of(1024 * 1024),
            });
        }
        check_overlap(&partitions)?;
        Ok(Layout {
            scheme: Scheme::Gpt,
            sector_size: sector,
            bytes,
            mbr,
            partitions,
            gpt: Some(Gpt {
                primary,
                backup,
                entries,
                primary_entries_lba: pe,
                backup_entries_lba: be,
                first_usable: first,
                last_usable: last,
            }),
        })
    } else {
        let mut partitions = Vec::new();
        for i in 0..4 {
            let e = &mbr[446 + i * 16..462 + i * 16];
            if e[4] == 0 {
                if u32_at(e, 8) != 0 || u32_at(e, 12) != 0 {
                    return Err(invalid("Malformed empty MBR partition."));
                }
                continue;
            }
            if ![0, 0x80].contains(&e[0]) {
                return Err(invalid("Invalid MBR boot indicator."));
            }
            if [0x05, 0x0f, 0x85, 0x42].contains(&e[4]) {
                return Err(invalid("Extended and dynamic MBR disks are unsupported."));
            }
            let a = u32_at(e, 8) as u64;
            let size = u32_at(e, 12) as u64;
            if a == 0 || size == 0 || a + size > count {
                return Err(invalid("MBR partition exceeds image bounds."));
            }
            partitions.push(Partition {
                index: i as u32 + 1,
                first_lba: a,
                last_lba: a + size - 1,
                name: String::new(),
                role: match e[4] {
                    0x27 => Role::Recovery,
                    0xef => Role::Efi,
                    _ if e[0] == 0x80 => Role::System,
                    0x07 | 0x0b | 0x0c => Role::Data,
                    _ => Role::Other,
                },
                aligned: (a * sector as u64).is_multiple_of(1024 * 1024),
            });
        }
        check_overlap(&partitions)?;
        Ok(Layout {
            scheme: Scheme::Mbr,
            sector_size: sector,
            bytes,
            mbr,
            partitions,
            gpt: None,
        })
    }
}
fn check_header(h: &[u8], current: u64, other: u64, count: u64, sector: u32) -> Result<()> {
    let size = u32_at(h, 12) as usize;
    if &h[..8] != b"EFI PART"
        || u32_at(h, 8) != 0x00010000
        || size < 92
        || size > sector as usize
        || h[20..24] != [0; 4]
        || u64_at(h, 24) != current
        || u64_at(h, 32) != other
        || u64_at(h, 48) >= count - 1
        || h[56..72] == [0; 16]
    {
        return Err(invalid(
            "Invalid GPT header signature, revision, geometry or disk GUID.",
        ));
    }
    let mut checked = h[..size].to_vec();
    checked[16..20].fill(0);
    if crc32fast::hash(&checked) != u32_at(h, 16) {
        return Err(invalid("GPT header CRC mismatch."));
    }
    Ok(())
}
fn check_overlap(partitions: &[Partition]) -> Result<()> {
    let mut ranges: Vec<_> = partitions
        .iter()
        .map(|p| (p.first_lba, p.last_lba))
        .collect();
    ranges.sort_unstable();
    if ranges.windows(2).any(|r| r[0].1 >= r[1].0) {
        return Err(invalid("Partitions overlap."));
    }
    Ok(())
}
fn gpt_role(id: &[u8]) -> Role {
    match id {
        [
            0x28,
            0x73,
            0x2a,
            0xc1,
            0x1f,
            0xf8,
            0xd2,
            0x11,
            0xba,
            0x4b,
            0,
            0xa0,
            0xc9,
            0x3e,
            0xc9,
            0x3b,
        ] => Role::Efi,
        [
            0x16,
            0xe3,
            0xc9,
            0xe3,
            0x5c,
            0x0b,
            0xb8,
            0x4d,
            0x81,
            0x7d,
            0xf9,
            0x2d,
            0xf,
            0,
            0x15,
            0xae,
        ] => Role::Reserved,
        [
            0xa2,
            0xa0,
            0xd0,
            0xeb,
            0xe5,
            0xb9,
            0x33,
            0x44,
            0x87,
            0xc0,
            0x68,
            0xb6,
            0xb7,
            0x26,
            0x99,
            0xc7,
        ] => Role::Data,
        [
            0xa4,
            0xbb,
            0x94,
            0xde,
            0xd1,
            0x06,
            0x40,
            0x4d,
            0xa1,
            0x6a,
            0xbf,
            0xd5,
            0x01,
            0x79,
            0xd6,
            0xac,
        ] => Role::Recovery,
        _ => Role::Other,
    }
}
pub fn header_crc(header: &mut [u8]) {
    let n = u32_at(header, 12) as usize;
    header[16..20].fill(0);
    let crc = crc32fast::hash(&header[..n]);
    header[16..20].copy_from_slice(&crc.to_le_bytes());
}
