use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::c_void,
    path::PathBuf,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*, System::IO::DeviceIoControl};
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Cursor {
    pub id: u64,
    pub next: i64,
}
pub struct Journal(HANDLE);
impl Drop for Journal {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
impl Journal {
    pub fn open(volume: &str) -> Result<Self> {
        unsafe {
            let h = CreateFileW(
                crate::common::wide(volume).as_ptr(),
                GENERIC_READ,
                7,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error().into());
            }
            let journal = Self(h);
            journal.query()?;
            Ok(journal)
        }
    }
    fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize> {
        unsafe {
            let mut n = 0;
            if DeviceIoControl(
                self.0,
                code,
                if input.is_empty() {
                    null()
                } else {
                    input.as_ptr() as *const c_void
                },
                input.len() as u32,
                output.as_mut_ptr() as *mut c_void,
                output.len() as u32,
                &mut n,
                null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(n as usize)
        }
    }
    pub fn query(&self) -> Result<(Cursor, i64)> {
        let mut out = [0u8; 128];
        if self.ioctl(0x900f4, &[], &mut out)? < 24 {
            bail!("Incomplete USN journal metadata")
        };
        Ok((
            Cursor {
                id: u64::from_le_bytes(out[0..8].try_into()?),
                next: i64::from_le_bytes(out[16..24].try_into()?),
            },
            i64::from_le_bytes(out[8..16].try_into()?),
        ))
    }
    pub fn changes(
        &self,
        from: Cursor,
        extensions: &[String],
    ) -> Result<(Cursor, Vec<(PathBuf, bool)>)> {
        let (latest, first) = self.query()?;
        if from.id != latest.id || from.next < first || from.next > latest.next {
            bail!("USN journal expired or changed")
        };
        let mut input = [0u8; 48];
        input[8..12].copy_from_slice(&0xb307u32.to_le_bytes());
        input[32..40].copy_from_slice(&from.id.to_le_bytes());
        input[40..42].copy_from_slice(&2u16.to_le_bytes());
        input[42..44].copy_from_slice(&3u16.to_le_bytes());
        let mut out = vec![0u8; 65536];
        let mut next = from.next;
        let mut candidates = HashMap::new();
        while next < latest.next {
            input[..8].copy_from_slice(&next.to_le_bytes());
            let count = self.ioctl(0x903ab, &input, &mut out)?;
            if count < 8 {
                bail!("Incomplete journal output")
            };
            let after = i64::from_le_bytes(out[..8].try_into()?);
            if after <= next {
                bail!("Journal did not advance")
            };
            let mut off = 8;
            while off < count {
                if off + 60 > count {
                    bail!("Incomplete journal record")
                };
                let len = u32::from_le_bytes(out[off..off + 4].try_into()?) as usize;
                let major = u16::from_le_bytes(out[off + 4..off + 6].try_into()?);
                let shift = if major == 3 {
                    16
                } else if major == 2 {
                    0
                } else {
                    bail!("Unsupported journal record")
                };
                if len < 60 + shift || off + len > count {
                    bail!("Malformed journal record")
                };
                let reason =
                    u32::from_le_bytes(out[off + 40 + shift..off + 44 + shift].try_into()?);
                let attributes =
                    u32::from_le_bytes(out[off + 52 + shift..off + 56 + shift].try_into()?);
                let name_len =
                    u16::from_le_bytes(out[off + 56 + shift..off + 58 + shift].try_into()?)
                        as usize;
                let name_offset =
                    u16::from_le_bytes(out[off + 58 + shift..off + 60 + shift].try_into()?)
                        as usize;
                if name_offset + name_len > len || name_len % 2 != 0 {
                    bail!("Malformed journal filename")
                };
                let directory = attributes & 0x10 != 0;
                let relevant = if directory {
                    reason & 0x2100 != 0
                } else if name_len == 0 {
                    // Windows may remove filenames from unprivileged journal records.
                    // Such records must be resolved to avoid losing model changes.
                    true
                } else {
                    let words = out[off + name_offset..off + name_offset + name_len]
                        .chunks_exact(2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect::<Vec<_>>();
                    let name = String::from_utf16_lossy(&words).to_lowercase();
                    extensions
                        .iter()
                        .any(|ext| name.ends_with(&ext.to_lowercase()))
                };
                if relevant {
                    let low = u64::from_le_bytes(out[off + 8..off + 16].try_into()?);
                    let high = if major == 3 {
                        u64::from_le_bytes(out[off + 16..off + 24].try_into()?)
                    } else {
                        0
                    };
                    candidates.insert((low, high), directory);
                }
                off += len;
            }
            next = after;
        }
        let mut changes = Vec::new();
        for ((low, high), directory) in candidates {
            if let Some(path) = self.resolve(low, high) {
                changes.push((path, directory));
            }
        }
        Ok((Cursor { id: from.id, next }, changes))
    }
    fn resolve(&self, low: u64, high: u64) -> Option<PathBuf> {
        unsafe {
            let mut desc: FILE_ID_DESCRIPTOR = std::mem::zeroed();
            desc.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
            desc.Type = if high == 0 { 0 } else { 2 };
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&low.to_le_bytes());
            id[8..].copy_from_slice(&high.to_le_bytes());
            desc.Anonymous.ExtendedFileId = FILE_ID_128 { Identifier: id };
            let h = OpenFileById(self.0, &desc, 0x80, 7, null(), FILE_FLAG_BACKUP_SEMANTICS);
            if h == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut buf = vec![0u16; 32768];
            let n = GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, 0) as usize;
            CloseHandle(h);
            if n == 0 || n >= buf.len() {
                return None;
            }
            let text = String::from_utf16_lossy(&buf[..n]);
            Some(PathBuf::from(
                if let Some(p) = text.strip_prefix(r"\\?\UNC\") {
                    format!(r"\\{p}")
                } else {
                    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
                },
            ))
        }
    }
}
