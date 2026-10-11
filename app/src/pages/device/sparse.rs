/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2026 Shomy
*/

// Android sparse image reader. Expands RAW, FILL and DONT_CARE chunks on the fly,
// avoiding a second, potentially multi-gigabyte userdata image on disk.
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};

const SPARSE_MAGIC: u32 = 0xed26_ff3a;
const CHUNK_RAW: u16 = 0xcac1;
const CHUNK_FILL: u16 = 0xcac2;
const CHUNK_DONT_CARE: u16 = 0xcac3;
const CHUNK_CRC32: u16 = 0xcac4;
const SPARSE_HEADER_LEN: u64 = 28;
const CHUNK_HEADER_LEN: u64 = 12;
const IO_BUFFER: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
struct Header {
    file_header_size: u16,
    chunk_header_size: u16,
    block_size: u32,
    total_blocks: u32,
    total_chunks: u32,
}

#[derive(Debug, Clone, Copy)]
struct Chunk {
    kind: u16,
    blocks: u32,
    total_size: u32,
}

pub struct ImageReader {
    inner: ReaderKind,
    expanded_size: u64,
    sparse: bool,
}

enum ReaderKind {
    Raw(BufReader<File>),
    Sparse(SparseReader),
}

struct SparseReader {
    file: BufReader<File>,
    header: Header,
    chunks_left: u32,
    active: ActiveChunk,
    output_remaining: u64,
}

enum ActiveChunk {
    None,
    Raw(u64),
    Fill { pattern: [u8; 4], offset: usize, remaining: u64 },
    Zero(u64),
}

impl ImageReader {
    /// Opens an image, fully validates sparse structure before any flash write,
    /// and returns the decoded output size (not the compressed file size).
    pub fn open(path: &Path, partition_size: u64) -> Result<(Self, u64, bool)> {
        let mut file = File::open(path)
            .with_context(|| format!("Cannot open image '{}'", path.display()))?;
        let mut magic_bytes = [0u8; 4];
        let is_sparse = file.read_exact(&mut magic_bytes).is_ok()
            && u32::from_le_bytes(magic_bytes) == SPARSE_MAGIC;

        if !is_sparse {
            let size = file.metadata()?.len();
            ensure!(size > 0, "Image is empty: {}", path.display());
            ensure!(
                size <= partition_size,
                "Image is too large ({} bytes; partition size {} bytes): {}",
                size, partition_size, path.display()
            );
            file.seek(SeekFrom::Start(0))?;
            return Ok((Self {
                inner: ReaderKind::Raw(BufReader::with_capacity(IO_BUFFER, file)),
                expanded_size: size,
                sparse: false,
            }, size, false));
        }

        file.seek(SeekFrom::Start(0))?;
        let header = read_header(&mut file)
            .with_context(|| format!("Invalid Android sparse image '{}'", path.display()))?;
        let expanded_size = validate_sparse_file(&mut file, header)
            .with_context(|| format!("Invalid Android sparse image '{}'", path.display()))?;
        ensure!(
            expanded_size <= partition_size,
            "Expanded sparse image is too large ({} bytes; partition size {} bytes): {}",
            expanded_size, partition_size, path.display()
        );

        file.seek(SeekFrom::Start(u64::from(header.file_header_size)))?;
        let reader = SparseReader {
            file: BufReader::with_capacity(IO_BUFFER, file),
            header,
            chunks_left: header.total_chunks,
            active: ActiveChunk::None,
            output_remaining: expanded_size,
        };
        Ok((Self {
            inner: ReaderKind::Sparse(reader),
            expanded_size,
            sparse: true,
        }, expanded_size, true))
    }

    pub fn expanded_size(&self) -> u64 {
        self.expanded_size
    }

    pub fn is_sparse(&self) -> bool {
        self.sparse
    }
}

impl Read for ImageReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            ReaderKind::Raw(reader) => reader.read(buf),
            ReaderKind::Sparse(reader) => reader.read(buf),
        }
    }
}

impl Read for SparseReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.output_remaining == 0 {
            return Ok(0);
        }

        loop {
            let active = std::mem::replace(&mut self.active, ActiveChunk::None);
            match active {
                ActiveChunk::Raw(mut remaining) => {
                    let amount = remaining.min(self.output_remaining).min(buf.len() as u64) as usize;
                    if amount == 0 {
                        continue;
                    }
                    let count = self.file.read(&mut buf[..amount])?;
                    if count == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated RAW sparse chunk"));
                    }
                    remaining -= count as u64;
                    self.output_remaining -= count as u64;
                    if remaining > 0 {
                        self.active = ActiveChunk::Raw(remaining);
                    }
                    return Ok(count);
                }
                ActiveChunk::Fill { pattern, mut offset, mut remaining } => {
                    let amount = remaining.min(self.output_remaining).min(buf.len() as u64) as usize;
                    if amount == 0 {
                        continue;
                    }
                    for (i, byte) in buf[..amount].iter_mut().enumerate() {
                        *byte = pattern[(offset + i) % 4];
                    }
                    offset = (offset + amount) % 4;
                    remaining -= amount as u64;
                    self.output_remaining -= amount as u64;
                    if remaining > 0 {
                        self.active = ActiveChunk::Fill { pattern, offset, remaining };
                    }
                    return Ok(amount);
                }
                ActiveChunk::Zero(mut remaining) => {
                    let amount = remaining.min(self.output_remaining).min(buf.len() as u64) as usize;
                    if amount == 0 {
                        continue;
                    }
                    buf[..amount].fill(0);
                    remaining -= amount as u64;
                    self.output_remaining -= amount as u64;
                    if remaining > 0 {
                        self.active = ActiveChunk::Zero(remaining);
                    }
                    return Ok(amount);
                }
                ActiveChunk::None => {
                    if self.chunks_left == 0 {
                        if self.output_remaining == 0 {
                            return Ok(0);
                        }
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "sparse image ended before its declared output size"));
                    }
                    let chunk = read_chunk_header(&mut self.file, self.header)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                    self.chunks_left -= 1;
                    let data_bytes = u64::from(chunk.blocks)
                        .checked_mul(u64::from(self.header.block_size))
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "sparse chunk size overflow"))?;
                    match chunk.kind {
                        CHUNK_RAW => self.active = ActiveChunk::Raw(data_bytes),
                        CHUNK_FILL => {
                            let mut pattern = [0u8; 4];
                            self.file.read_exact(&mut pattern)?;
                            self.active = ActiveChunk::Fill { pattern, offset: 0, remaining: data_bytes };
                        }
                        CHUNK_DONT_CARE => self.active = ActiveChunk::Zero(data_bytes),
                        CHUNK_CRC32 => {
                            let mut crc = [0u8; 4];
                            self.file.read_exact(&mut crc)?;
                            // CRC chunks carry metadata only; the full file structure was preflighted.
                        }
                        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported sparse chunk type")),
                    }
                }
            }
        }
    }
}

fn read_header<R: Read>(reader: &mut R) -> Result<Header> {
    let mut bytes = [0u8; SPARSE_HEADER_LEN as usize];
    reader.read_exact(&mut bytes).context("truncated sparse header")?;
    ensure!(u32::from_le_bytes(bytes[0..4].try_into()?) == SPARSE_MAGIC, "bad sparse magic");
    ensure!(u16::from_le_bytes(bytes[4..6].try_into()?) == 1, "unsupported sparse major version");
    let file_header_size = u16::from_le_bytes(bytes[8..10].try_into()?);
    let chunk_header_size = u16::from_le_bytes(bytes[10..12].try_into()?);
    let block_size = u32::from_le_bytes(bytes[12..16].try_into()?);
    let total_blocks = u32::from_le_bytes(bytes[16..20].try_into()?);
    let total_chunks = u32::from_le_bytes(bytes[20..24].try_into()?);
    ensure!(u64::from(file_header_size) >= SPARSE_HEADER_LEN, "invalid sparse file header size");
    ensure!(u64::from(chunk_header_size) >= CHUNK_HEADER_LEN, "invalid sparse chunk header size");
    ensure!(block_size >= 4 && block_size % 4 == 0, "invalid sparse block size");
    ensure!(total_blocks > 0 && total_chunks > 0, "empty sparse image");
    Ok(Header { file_header_size, chunk_header_size, block_size, total_blocks, total_chunks })
}

fn read_chunk_header<R: Read>(reader: &mut R, header: Header) -> Result<Chunk> {
    let mut bytes = [0u8; CHUNK_HEADER_LEN as usize];
    reader.read_exact(&mut bytes).context("truncated sparse chunk header")?;
    let kind = u16::from_le_bytes(bytes[0..2].try_into()?);
    let blocks = u32::from_le_bytes(bytes[4..8].try_into()?);
    let total_size = u32::from_le_bytes(bytes[8..12].try_into()?);
    let extra = u64::from(header.chunk_header_size) - CHUNK_HEADER_LEN;
    if extra > 0 {
        let mut discard = vec![0u8; extra.min(4096) as usize];
        let mut remaining = extra;
        while remaining > 0 {
            let count = remaining.min(discard.len() as u64) as usize;
            reader.read_exact(&mut discard[..count]).context("truncated extended chunk header")?;
            remaining -= count as u64;
        }
    }
    Ok(Chunk { kind, blocks, total_size })
}

fn validate_sparse_file(file: &mut File, header: Header) -> Result<u64> {
    file.seek(SeekFrom::Start(u64::from(header.file_header_size)))?;
    let file_len = file.metadata()?.len();
    let mut total_blocks = 0u64;

    for index in 0..header.total_chunks {
        let chunk = read_chunk_header(file, header)
            .with_context(|| format!("chunk {} header", index + 1))?;
        let blocks = u64::from(chunk.blocks);
        let output_bytes = blocks.checked_mul(u64::from(header.block_size))
            .context("sparse chunk output size overflow")?;
        let expected_payload = match chunk.kind {
            CHUNK_RAW => {
                ensure!(blocks > 0, "RAW chunk {} has no blocks", index + 1);
                output_bytes
            }
            CHUNK_FILL => {
                ensure!(blocks > 0, "FILL chunk {} has no blocks", index + 1);
                4
            }
            CHUNK_DONT_CARE => {
                ensure!(blocks > 0, "DONT_CARE chunk {} has no blocks", index + 1);
                0
            }
            CHUNK_CRC32 => {
                ensure!(chunk.blocks == 0, "CRC32 chunk {} must not output blocks", index + 1);
                4
            }
            _ => bail!("unsupported sparse chunk type 0x{:04x} in chunk {}", chunk.kind, index + 1),
        };
        let expected_total = u64::from(header.chunk_header_size)
            .checked_add(expected_payload).context("sparse chunk size overflow")?;
        ensure!(
            u64::from(chunk.total_size) == expected_total,
            "invalid total size for sparse chunk {} (type 0x{:04x})",
            index + 1, chunk.kind
        );
        let payload_pos = file.stream_position()?;
        let end = payload_pos.checked_add(expected_payload).context("sparse file offset overflow")?;
        ensure!(end <= file_len, "truncated payload in sparse chunk {}", index + 1);
        file.seek(SeekFrom::Start(end))?;
        if chunk.kind != CHUNK_CRC32 {
            total_blocks = total_blocks.checked_add(blocks).context("sparse block count overflow")?;
            ensure!(total_blocks <= u64::from(header.total_blocks), "sparse chunks exceed declared output size");
        }
    }

    ensure!(total_blocks == u64::from(header.total_blocks), "sparse block count does not match header");
    ensure!(file.stream_position()? == file_len, "unexpected trailing data after sparse chunks");
    u64::from(header.total_blocks)
        .checked_mul(u64::from(header.block_size))
        .context("expanded sparse image size overflow")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "penumbra-sparse-test-{}-{}.img",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    fn sparse_image(chunks: &[(u16, u32, Vec<u8>)], block_size: u32) -> Vec<u8> {
        let total_blocks: u32 = chunks.iter().filter(|(kind, _, _)| *kind != CHUNK_CRC32).map(|(_, blocks, _)| *blocks).sum();
        let mut out = Vec::new();
        out.extend_from_slice(&SPARSE_MAGIC.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&28u16.to_le_bytes());
        out.extend_from_slice(&12u16.to_le_bytes());
        out.extend_from_slice(&block_size.to_le_bytes());
        out.extend_from_slice(&total_blocks.to_le_bytes());
        out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for (kind, blocks, payload) in chunks {
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&blocks.to_le_bytes());
            out.extend_from_slice(&(12u32 + payload.len() as u32).to_le_bytes());
            out.extend_from_slice(payload);
        }
        out
    }

    #[test]
    fn expands_raw_fill_and_dont_care_chunks() {
        let bytes = sparse_image(&[
            (CHUNK_RAW, 1, b"ABCD".to_vec()),
            (CHUNK_FILL, 1, vec![0x11, 0x22, 0x33, 0x44]),
            (CHUNK_DONT_CARE, 1, vec![]),
        ], 4);
        let path = temp_file(&bytes);
        let (mut reader, size, sparse) = ImageReader::open(&path, 12).unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(sparse);
        assert_eq!(size, 12);
        assert_eq!(output, b"ABCD\x11\x22\x33\x44\0\0\0\0");
    }

    #[test]
    fn rejects_output_larger_than_partition() {
        let bytes = sparse_image(&[(CHUNK_DONT_CARE, 2, vec![])], 4096);
        let path = temp_file(&bytes);
        assert!(ImageReader::open(&path, 4096).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_truncated_raw_chunk() {
        let bytes = sparse_image(&[(CHUNK_RAW, 1, b"AB".to_vec())], 4);
        let path = temp_file(&bytes);
        assert!(ImageReader::open(&path, 4).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
