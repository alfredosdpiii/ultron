//! The classic deflated ZIP writer of `packages/coding-agent/src/utils/zip.ts`, used for bug-report archives.

use flate2::Compression;
use flate2::write::DeflateEncoder;
use std::io::Write;

/// Local wall-clock time stamped on every entry (the TypeScript uses `new Date()` in the local time zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// MS-DOS (time, date) words; years before 1980 clamp to 1980.
fn dos_date_time(at: LocalDateTime) -> (u16, u16) {
    let time = (u16::from(at.hour) << 11) | (u16::from(at.minute) << 5) | (u16::from(at.second) >> 1);
    let day = ((at.year.max(1980) - 1980) << 9) | (u16::from(at.month) << 5) | u16::from(at.day);
    (time, day)
}

fn deflate_raw(data: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    // Writing to a Vec cannot fail.
    let _ = encoder.write_all(data);
    encoder.finish().unwrap_or_default()
}

#[allow(clippy::cast_possible_truncation)]
fn u32_len(len: usize) -> u32 {
    len as u32
}

/// One zip with each `(name, data)` entry deflated, UTF-8 names flagged, and a central directory.
pub fn create_zip_archive(entries: &[(&str, &[u8])], at: LocalDateTime) -> Vec<u8> {
    let (time, day) = dos_date_time(at);
    let mut files = Vec::new();
    let mut directory = Vec::new();
    for (name, data) in entries {
        let compressed = deflate_raw(data);
        let checksum = crc32fast::hash(data);
        let name = name.as_bytes();
        #[allow(clippy::cast_possible_truncation)]
        let name_len = name.len() as u16;
        let offset = u32_len(files.len());

        files.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        for word in [20u16, 0x0800, 8, time, day] {
            files.extend_from_slice(&word.to_le_bytes());
        }
        for long in [checksum, u32_len(compressed.len()), u32_len(data.len())] {
            files.extend_from_slice(&long.to_le_bytes());
        }
        files.extend_from_slice(&name_len.to_le_bytes());
        files.extend_from_slice(&0u16.to_le_bytes());
        files.extend_from_slice(name);
        files.extend_from_slice(&compressed);

        directory.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        for word in [20u16, 20, 0x0800, 8, time, day] {
            directory.extend_from_slice(&word.to_le_bytes());
        }
        for long in [checksum, u32_len(compressed.len()), u32_len(data.len())] {
            directory.extend_from_slice(&long.to_le_bytes());
        }
        directory.extend_from_slice(&name_len.to_le_bytes());
        // Extra and comment lengths, disk number, internal and external attributes.
        directory.extend_from_slice(&[0; 12]);
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name);
    }
    #[allow(clippy::cast_possible_truncation)]
    let count = entries.len() as u16;
    let central_offset = u32_len(files.len());
    let central_len = u32_len(directory.len());
    files.extend_from_slice(&directory);
    files.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    files.extend_from_slice(&[0; 4]);
    files.extend_from_slice(&count.to_le_bytes());
    files.extend_from_slice(&count.to_le_bytes());
    files.extend_from_slice(&central_len.to_le_bytes());
    files.extend_from_slice(&central_offset.to_le_bytes());
    files.extend_from_slice(&[0; 2]);
    files
}
