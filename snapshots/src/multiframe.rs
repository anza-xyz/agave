//! Multi-frame zstd writer.
//!
//! The payload is split into independently compressed zstd frames, written
//! back to back, so that readers can decompress frames in parallel.
use std::io::{self, Write};

const MAX_ZSTD_FRAME_SIZE: u32 = 1 << 30;

/// Compresses a byte stream into independent zstd frames of `frame_size`
/// uncompressed bytes each, written back to back.
pub struct MultiFrameZstdWriter<W: Write> {
    inner: W,
    compressor: zstd::bulk::Compressor<'static>,
    frame_size: usize,
    buf: Vec<u8>,
    compressed: Vec<u8>,
}

impl<W: Write> MultiFrameZstdWriter<W> {
    pub fn new(inner: W, compression_level: i32, frame_size: u32) -> io::Result<Self> {
        if frame_size == 0 || frame_size > MAX_ZSTD_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid zstd frame size: {frame_size}"),
            ));
        }
        let frame_size = frame_size as usize;
        Ok(Self {
            inner,
            compressor: zstd::bulk::Compressor::new(compression_level)?,
            frame_size,
            buf: Vec::with_capacity(frame_size),
            compressed: Vec::new(),
        })
    }

    fn emit_frame(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.compressed.clear();
        self.compressed
            .reserve(zstd::zstd_safe::compress_bound(self.buf.len()));
        let compressed_size = self
            .compressor
            .compress_to_buffer(&self.buf, &mut self.compressed)?;
        self.inner.write_all(&self.compressed[..compressed_size])?;
        self.buf.clear();
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.emit_frame()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for MultiFrameZstdWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len() == self.frame_size {
            self.emit_frame()?;
        }

        let take = self
            .frame_size
            .saturating_sub(self.buf.len())
            .min(data.len());
        self.buf.extend_from_slice(&data[..take]);
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit_frame()?;
        self.inner.flush()
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use {
        super::*,
        crate::{ArchiveFormat, ArchiveFormatDecompressor, ZstdConfig},
        std::io::{BufReader, Cursor, Read},
    };

    const TEST_FRAME_SIZE: u32 = 64 * 1024;

    fn test_data(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| {
                if (i / 256) % 2 == 0 {
                    (i / 64) as u8
                } else {
                    (i as u32).wrapping_mul(2654435761) as u8
                }
            })
            .collect()
    }

    fn compress(data: &[u8], frame_size: u32) -> Vec<u8> {
        let mut writer = MultiFrameZstdWriter::new(Vec::new(), 1, frame_size).unwrap();
        writer.write_all(data).unwrap();
        writer.finish().unwrap()
    }

    fn walk_frames(archive: &[u8]) -> Vec<(usize, usize)> {
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < archive.len() {
            let rest = &archive[offset..];
            let compressed_size = zstd::zstd_safe::find_frame_compressed_size(rest).unwrap();
            let decompressed_size = zstd::zstd_safe::get_frame_content_size(rest)
                .unwrap()
                .expect("frame header carries decompressed size")
                as usize;
            frames.push((compressed_size, decompressed_size));
            offset += compressed_size;
        }
        frames
    }

    #[test]
    fn test_roundtrip_byte_identity() {
        let data = test_data(10 * TEST_FRAME_SIZE as usize + 12345);
        let archive = compress(&data, TEST_FRAME_SIZE);
        assert_eq!(zstd::decode_all(&archive[..]).unwrap(), data);
    }

    #[test]
    fn test_frame_boundaries_walkable() {
        let data = test_data(10 * TEST_FRAME_SIZE as usize + 12345);
        let archive = compress(&data, TEST_FRAME_SIZE);
        let frames = walk_frames(&archive);
        assert_eq!(frames.len(), 11);
        for &(_, decompressed_size) in &frames[..10] {
            assert_eq!(decompressed_size, TEST_FRAME_SIZE as usize);
        }
        assert_eq!(frames[10].1, 12345);
        let total: usize = frames.iter().map(|&(_, d)| d).sum();
        assert_eq!(total, data.len());
    }

    #[test]
    fn test_frames_independently_decompressible() {
        let data = test_data(4 * TEST_FRAME_SIZE as usize + 999);
        let archive = compress(&data, TEST_FRAME_SIZE);
        let mut offset = 0usize;
        let mut decompressed_offset = 0usize;
        for (compressed_size, decompressed_size) in walk_frames(&archive) {
            let frame = &archive[offset..offset + compressed_size];
            assert_eq!(
                zstd::bulk::decompress(frame, decompressed_size).unwrap(),
                data[decompressed_offset..decompressed_offset + decompressed_size]
            );
            offset += compressed_size;
            decompressed_offset += decompressed_size;
        }
    }

    #[test]
    fn test_archive_read_path_decodes_multiframe() {
        let data = test_data(3 * TEST_FRAME_SIZE as usize + 777);
        let archive = compress(&data, TEST_FRAME_SIZE);
        let mut decompressor = ArchiveFormatDecompressor::new(
            ArchiveFormat::TarZstd {
                config: ZstdConfig::default(),
            },
            BufReader::new(Cursor::new(archive)),
        )
        .unwrap();
        let mut decompressed = Vec::new();
        decompressor.read_to_end(&mut decompressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_empty_input() {
        let archive = compress(&[], TEST_FRAME_SIZE);
        assert!(archive.is_empty());
    }

    #[test]
    fn test_exact_frame_multiple() {
        let data = test_data(4 * TEST_FRAME_SIZE as usize);
        let archive = compress(&data, TEST_FRAME_SIZE);
        let frames = walk_frames(&archive);
        assert_eq!(frames.len(), 4);
        assert!(frames.iter().all(|&(_, d)| d == TEST_FRAME_SIZE as usize));
        assert_eq!(zstd::decode_all(&archive[..]).unwrap(), data);
    }

    #[test]
    fn test_input_smaller_than_frame() {
        let data = test_data(1000);
        let archive = compress(&data, TEST_FRAME_SIZE);
        let frames = walk_frames(&archive);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1, 1000);
        assert_eq!(zstd::decode_all(&archive[..]).unwrap(), data);
    }

    #[test]
    fn test_flush_cuts_frame() {
        let data = test_data(1000);
        let mut writer = MultiFrameZstdWriter::new(Vec::new(), 1, TEST_FRAME_SIZE).unwrap();
        writer.write_all(&data[..400]).unwrap();
        writer.flush().unwrap();
        writer.write_all(&data[400..]).unwrap();
        let archive = writer.finish().unwrap();
        let frames = walk_frames(&archive);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].1, 400);
        assert_eq!(frames[1].1, 600);
        assert_eq!(zstd::decode_all(&archive[..]).unwrap(), data);
    }

    #[test]
    fn test_invalid_frame_size_rejected() {
        assert!(MultiFrameZstdWriter::new(Vec::new(), 1, 0).is_err());
        assert!(MultiFrameZstdWriter::new(Vec::new(), 1, MAX_ZSTD_FRAME_SIZE + 1).is_err());
    }
}
