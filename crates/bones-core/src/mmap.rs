use memmap2::Mmap;
use std::fs::File;
use std::path::Path;
use anyhow::{Result, Context, anyhow};
use crate::cache::{CACHE_MAGIC, CACHE_VERSION, HEADER_SIZE, CacheHeader, CacheError};

/// Memory-mapped view of the binary columnar cache file (events.bin).
/// Provides zero-copy read access to the cache.
pub struct MmapCache {
    mmap: Mmap,
    /// Total number of events in the cache (from header).
    pub event_count: usize,
}

impl MmapCache {
    /// Open and memory-map the binary cache file.
    /// Validates the file header before returning.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open cache file {}", path.display()))?;
        // Safety: We assume the file is not modified concurrently.
        #[allow(unsafe_code)]
        let mmap = unsafe { Mmap::map(&file).with_context(|| format!("mmap cache file {}", path.display()))? };

        if mmap.len() < HEADER_SIZE {
            return Err(anyhow!(CacheError::UnexpectedEof));
        }

        // Check magic
        let magic: [u8; 4] = mmap[0..4].try_into().unwrap();
        if magic != CACHE_MAGIC {
            return Err(anyhow!(CacheError::InvalidMagic(magic)));
        }

        let version = mmap[4];
        if version > CACHE_VERSION {
            return Err(anyhow!(CacheError::UnsupportedVersion(version)));
        }
        
        let row_count = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;

        Ok(Self {
            mmap,
            event_count: row_count,
        })
    }

    /// Get a byte slice for a specific column/region of the cache.
    /// Offset and length are determined by the columnar format header.
    pub fn column_bytes(&self, offset: usize, len: usize) -> &[u8] {
        &self.mmap[offset..offset+len]
    }

    /// Check if the cache is valid (header magic, version, checksum).
    pub fn is_valid(&self) -> bool {
        if self.mmap.len() < HEADER_SIZE {
            return false;
        }

        let magic: [u8; 4] = self.mmap[0..4].try_into().unwrap();
        if magic != CACHE_MAGIC {
            return false;
        }

        let version = self.mmap[4];
        if version > CACHE_VERSION {
            return false;
        }

        let column_count = self.mmap[5] as usize;
        let stored_crc = u64::from_le_bytes(self.mmap[24..32].try_into().unwrap());

        let offsets_start = HEADER_SIZE;
        let offsets_end = offsets_start + column_count * 8;
        if self.mmap.len() < offsets_end {
            return false;
        }

        let col_data_start = offsets_end;
        if self.mmap.len() < col_data_start {
            return false;
        }
        
        let col_data = &self.mmap[col_data_start..];
        let actual_crc = checksum(col_data);
        
        actual_crc == stored_crc
    }

    /// Parse and return the cache header.
    pub fn header(&self) -> Result<CacheHeader> {
        if self.mmap.len() < HEADER_SIZE {
            return Err(anyhow!(CacheError::UnexpectedEof));
        }
        
        // We can use CacheHeader::decode but it decodes columns too.
        // We have to parse manually to avoid full decode.
        // Actually, CacheHeader::decode returns (Header, Columns). 
        // We can't use it efficiently if we just want Header.
        // But wait, CacheHeader definition is public.
        
        let version = self.mmap[4];
        let column_count = self.mmap[5];
        let row_count = u64::from_le_bytes(self.mmap[8..16].try_into().unwrap());
        let created_at_us = u64::from_le_bytes(self.mmap[16..24].try_into().unwrap());
        let data_crc64 = u64::from_le_bytes(self.mmap[24..32].try_into().unwrap());
        
        Ok(CacheHeader {
            version,
            column_count,
            row_count,
            created_at_us,
            data_crc64,
        })
    }

    /// Get the raw byte slice of the memory map.
    pub fn as_slice(&self) -> &[u8] {
        &self.mmap
    }
}

fn checksum(data: &[u8]) -> u64 {
    // Polynomial for CRC-64/XZ: 0xC96C5795D7870F42
    const POLY: u64 = 0xC96C5795D7870F42;
    let mut crc: u64 = u64::MAX;
    for &byte in data {
        crc ^= u64::from(byte) << 56;
        for _ in 0..8 {
            if crc & (1 << 63) != 0 {
                crc = (crc << 1) ^ POLY;
            } else {
                crc <<= 1;
            }
        }
    }
    !crc
}
