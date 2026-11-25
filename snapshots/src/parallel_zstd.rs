//! Reader that decompresses the independent frames of a multi-frame zstd archive in parallel.
use {
    crate::multiframe::ZSTD_FRAME_SIZE,
    agave_worker_pool::{WorkerPool, WorkerPoolConfig},
    crossbeam_channel::{Receiver, Sender, bounded, unbounded},
    std::{
        collections::VecDeque,
        io::{self, BufRead, Cursor, Read},
        mem,
        num::NonZeroUsize,
    },
    zstd::zstd_safe::{
        self,
        zstd_sys::{ZSTD_ErrorCode, ZSTD_getErrorCode},
    },
};

/// A mainnet archive reads at 6 GB/s with one decoder and tops out at 21 GB/s with 4, where copying
/// the decoded frames is the limit. Disk writes of the unpacked data usually limit the rate lower.
pub(crate) const NUM_DECODERS: NonZeroUsize = NonZeroUsize::new(4).unwrap();
/// Bounds the memory of the frames in flight by decoding in parallel only frames up to 4 times the
/// archiver's frame size. An archive starting with a larger frame decodes serially.
const MAX_PARALLEL_FRAME_SIZE: u64 = 4 * ZSTD_FRAME_SIZE as u64;
/// One frame being decoded and one queued per decoder, to avoid waiting for the reader.
const FRAMES_IN_FLIGHT_PER_DECODER: usize = 2;

type CompressedFrame = Vec<u8>;
type DecodedFrame = Vec<u8>;

struct DecodeJob {
    compressed_frame: CompressedFrame,
    decoded_sender: Sender<io::Result<DecodedFrame>>,
}

/// Returns the decoded size of the frame starting in `frame_prefix`, or `u64::MAX` if unrecorded.
fn max_decoded_frame_size(frame_prefix: &[u8]) -> u64 {
    zstd_safe::get_frame_content_size(frame_prefix)
        .ok()
        .flatten()
        .unwrap_or(u64::MAX)
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Returns the size of the frame at the start of `input`, or `Ok(None)` if `input` doesn't hold it
/// all yet. Fails if `input` isn't a valid frame.
fn find_frame_len(input: &[u8]) -> io::Result<Option<usize>> {
    fn is_src_size_wrong(code: zstd_safe::ErrorCode) -> bool {
        // Safety: the function only decodes the error code out of its argument.
        unsafe { ZSTD_getErrorCode(code) == ZSTD_ErrorCode::ZSTD_error_srcSize_wrong }
    }

    match zstd_safe::find_frame_compressed_size(input) {
        Ok(frame_len) => Ok(Some(frame_len)),
        Err(code) if is_src_size_wrong(code) => Ok(None),
        Err(code) => Err(invalid_data(zstd_safe::get_error_name(code))),
    }
}

/// Returns whether the archive that starts with `first_frame_prefix` can be decoded in parallel.
pub(crate) fn can_decode_in_parallel(first_frame_prefix: &[u8]) -> bool {
    max_decoded_frame_size(first_frame_prefix) <= MAX_PARALLEL_FRAME_SIZE
}

/// Reads the frames from `input` and decodes them on a pool of workers. The calling thread only
/// copies the compressed and the decoded frames.
pub struct ParallelZstdReader<R> {
    input: R,
    /// Compressed bytes read from `input` that aren't cut into frames yet
    uncut_input: Vec<u8>,
    decoders: WorkerPool<DecodeJob>,
    buffers: BufferPool,
    /// Receivers of the results of the frames being decoded, in the input order
    pending_decoded_frames: VecDeque<Receiver<io::Result<DecodedFrame>>>,
    /// The decoded frame that `read` serves
    current_decoded_frame: Cursor<DecodedFrame>,
}

impl<R: BufRead> ParallelZstdReader<R> {
    /// Returns the reader of the frames in `input`, which must start with a sized frame.
    pub(crate) fn new(input: R, num_decoders: NonZeroUsize) -> io::Result<Self> {
        let buffers = BufferPool::new();
        let decoder_buffers = buffers.clone();
        let decoders = WorkerPool::spawn(
            WorkerPoolConfig::new("solZstdDec", num_decoders),
            move |_worker_id| {
                let mut decoder = FrameDecoder {
                    decompressor: zstd::bulk::Decompressor::new()
                        .expect("the zstd decompression context should be created"),
                    buffers: decoder_buffers.clone(),
                };
                move |job: DecodeJob| decoder.decode(job)
            },
        )?;
        Ok(Self {
            input,
            uncut_input: Vec::new(),
            decoders,
            buffers,
            pending_decoded_frames: VecDeque::new(),
            current_decoded_frame: Cursor::default(),
        })
    }

    /// Queues frames for decoding until the maximum number of them is in flight.
    fn queue_frames(&mut self) -> io::Result<()> {
        let max_frames_in_flight = self
            .decoders
            .num_workers()
            .saturating_mul(FRAMES_IN_FLIGHT_PER_DECODER);
        while self.pending_decoded_frames.len() < max_frames_in_flight {
            let Some(compressed_frame) = self.cut_frame()? else {
                break;
            };
            let (decoded_sender, decoded_receiver) = bounded(1);
            self.decoders.send(DecodeJob {
                compressed_frame,
                decoded_sender,
            });
            self.pending_decoded_frames.push_back(decoded_receiver);
        }
        Ok(())
    }

    /// Cuts the next frame out of the input, or returns `Ok(None)` at its end.
    fn cut_frame(&mut self) -> io::Result<Option<CompressedFrame>> {
        let max_frame_len = zstd_safe::compress_bound(MAX_PARALLEL_FRAME_SIZE as usize);
        loop {
            if let Some(frame_len) = find_frame_len(&self.uncut_input)? {
                let mut next = self.buffers.get();
                next.extend_from_slice(&self.uncut_input[frame_len..]);
                self.uncut_input.truncate(frame_len);
                return Ok(Some(mem::replace(&mut self.uncut_input, next)));
            }
            let room = max_frame_len.saturating_sub(self.uncut_input.len());
            if room == 0 {
                return Err(invalid_data("zstd frame too large for parallel mode"));
            }
            let chunk = self.input.fill_buf()?;
            if chunk.is_empty() {
                if self.uncut_input.is_empty() {
                    return Ok(None);
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let chunk_len = chunk.len().min(room);
            self.uncut_input.extend_from_slice(&chunk[..chunk_len]);
            self.input.consume(chunk_len);
        }
    }
}

impl<R: BufRead> Read for ParallelZstdReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let len = self.current_decoded_frame.read(buf)?;
            if len > 0 || buf.is_empty() {
                return Ok(len);
            }
            self.queue_frames()?;
            let Some(decoded_receiver) = self.pending_decoded_frames.pop_front() else {
                return Ok(0);
            };
            let decoded_frame = decoded_receiver
                .recv()
                .map_err(|_| io::Error::other("zstd decoder thread exited"))??;
            let done = mem::replace(&mut self.current_decoded_frame, Cursor::new(decoded_frame));
            self.buffers.put(done.into_inner());
        }
    }
}

/// The state of a decoding worker.
struct FrameDecoder {
    decompressor: zstd::bulk::Decompressor<'static>,
    buffers: BufferPool,
}

impl FrameDecoder {
    fn decode(&mut self, job: DecodeJob) {
        let result = self.decode_frame(&job.compressed_frame);
        // The reader may be dropped before it takes the result.
        let _ = job.decoded_sender.send(result);
        self.buffers.put(job.compressed_frame);
    }

    fn decode_frame(&mut self, compressed_frame: &[u8]) -> io::Result<DecodedFrame> {
        let decoded_size = max_decoded_frame_size(compressed_frame);
        if decoded_size > MAX_PARALLEL_FRAME_SIZE {
            return Err(invalid_data(
                "missing or non-uniform zstd frame size in parallel mode",
            ));
        }
        let mut output = self.buffers.get();
        output.reserve(decoded_size as usize);
        self.decompressor
            .decompress_to_buffer(compressed_frame, &mut output)?;
        Ok(output)
    }
}

/// Shared by the reader and the decoders for compressed and decoded frames, to avoid alloc churn.
#[derive(Clone)]
struct BufferPool {
    done_sender: Sender<Vec<u8>>,
    available_receiver: Receiver<Vec<u8>>,
}

impl BufferPool {
    fn new() -> Self {
        let (done_sender, available_receiver) = unbounded();
        Self {
            done_sender,
            available_receiver,
        }
    }

    fn get(&self) -> Vec<u8> {
        // Sized for a decoded frame of the archiver, which a compressed frame fits in too.
        self.available_receiver
            .try_recv()
            .unwrap_or_else(|_| Vec::with_capacity(ZSTD_FRAME_SIZE as usize))
    }

    fn put(&self, mut buffer: Vec<u8>) {
        buffer.clear();
        self.done_sender
            .send(buffer)
            .expect("the pool should keep its channel open");
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{ArchiveFormat, ArchiveFormatDecompressor, multiframe::MultiFrameZstdWriter},
        std::io::{BufReader, Write},
    };

    const TEST_FRAME_SIZE: u32 = 64 * 1024;
    const SMALL_INPUT_BUFFER_SIZE: usize = 1000;

    fn test_data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn compress(data: &[u8]) -> Vec<u8> {
        let mut writer = MultiFrameZstdWriter::new(Vec::new(), 1, TEST_FRAME_SIZE).unwrap();
        writer.write_all(data).unwrap();
        writer.finish().unwrap()
    }

    fn reader(archive: Vec<u8>, num_decoders: usize) -> ParallelZstdReader<impl BufRead> {
        let input = BufReader::with_capacity(SMALL_INPUT_BUFFER_SIZE, Cursor::new(archive));
        ParallelZstdReader::new(input, NonZeroUsize::new(num_decoders).unwrap()).unwrap()
    }

    #[test]
    fn test_reads_frames_in_order() {
        let data = test_data(40 * TEST_FRAME_SIZE as usize + 12345);
        let archive = compress(&data);
        for num_decoders in [1, 3, 8] {
            let mut decoded = Vec::new();
            reader(archive.clone(), num_decoders)
                .read_to_end(&mut decoded)
                .unwrap();
            assert!(decoded == data, "{num_decoders} decoders");
        }
    }

    #[test]
    fn test_truncated_archive_fails() {
        let archive = compress(&test_data(5 * TEST_FRAME_SIZE as usize));
        let truncated = archive[..archive.len() - 10].to_vec();
        let err = reader(truncated, 2)
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn test_corrupt_frame_fails_without_reading_everything() {
        let mut archive = compress(&test_data(5 * TEST_FRAME_SIZE as usize));
        let first_frame_len = zstd_safe::find_frame_compressed_size(&archive).unwrap();
        archive[first_frame_len..first_frame_len + 4].fill(0);
        let err = reader(archive, 2).read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_drop_before_reading_everything() {
        let archive = compress(&test_data(100 * TEST_FRAME_SIZE as usize));
        reader(archive, 2).read_exact(&mut [0; 10]).unwrap();
    }

    #[test]
    fn test_input_without_frame_sizes_is_decoded_serially() {
        let data = test_data(3 * TEST_FRAME_SIZE as usize);
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
        encoder.write_all(&data).unwrap();
        let input = BufReader::new(Cursor::new(encoder.finish().unwrap()));

        let format = ArchiveFormat::from_cli_arg("zstd").unwrap();
        let mut decompressor = ArchiveFormatDecompressor::new(format, input).unwrap();
        assert!(matches!(decompressor, ArchiveFormatDecompressor::Zstd(_)));
        let mut decoded = Vec::new();
        decompressor.read_to_end(&mut decoded).unwrap();
        assert!(decoded == data);
    }
}
