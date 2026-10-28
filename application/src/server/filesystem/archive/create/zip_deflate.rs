//! plugged.host: deflates the body of one big zip entry in chunks on the archive's thread pool,
//! the way pigz does for gzip, instead of on the merger's single thread.
//!
//! Each chunk is raw-deflated on its own, primed with the 32 KiB of input before it as its
//! dictionary (so its matches may reach back into the previous chunk, which the decoder has
//! already produced) and ended with a sync flush: no chunk sets BFINAL and every chunk ends on a
//! byte boundary, so the chunks written in order form one deflate stream. The entry itself is the
//! ZipWriter's own: it writes the local header, the zip64 fields, the data descriptor and the
//! central directory as for any entry. Its deflater sees no input, so when the entry is finished
//! it ends the stream with its final empty block, and the ZipWriter takes the compressed size from
//! the destination's position as usual. The CRC-32 and the size are handed to it with
//! `set_file_metadata`, which is there for bodies written past the ZipWriter.
//!
//! The body goes to the destination under the ZipWriter, which a streamed zip (`StreamWriter`,
//! counting the bytes it writes itself) cannot take: those keep upstream's one-thread path.

use crate::io::SafeSliceExt;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom, Write},
};

/// Input bytes deflated by one task.
const CHUNK_BYTES: usize = 1024 * 1024;
/// Deflate's window: how much of the input before a chunk primes its compressor.
const DICTIONARY_BYTES: usize = 32 * 1024;
/// Output room kept free before every deflate call, far over the 6 bytes a sync flush needs.
const OUTPUT_HEADROOM: usize = 64 * 1024;
/// Ceiling on chunks read but not written yet, whatever the pool's size.
const MAX_CHUNKS_IN_FLIGHT: usize = 64;

// a dictionary is the tail of the chunk before, so it must never span two chunks
const _: () = assert!(CHUNK_BYTES >= DICTIONARY_BYTES);

/// A zip destination an entry's body can be written to directly, under the ZipWriter.
pub trait DirectBody {
    /// Whether [`DirectBody::write_body`] works on this destination.
    const TAKES_BODY: bool;

    /// Writes `data` at the destination's current position.
    fn write_body(&self, data: &[u8]) -> std::io::Result<()>;
}

/// A seekable zip destination the merger can also write an entry's body to while the ZipWriter
/// owns it (it is reachable through `ZipWriter::get_ref`). The ZipWriter's own writes need no
/// borrow check: they go through `get_mut`.
pub struct Spliced<W: Write + Seek> {
    inner: RefCell<W>,
}

impl<W: Write + Seek> Spliced<W> {
    #[inline]
    pub fn new(inner: W) -> Self {
        Self {
            inner: RefCell::new(inner),
        }
    }

    #[inline]
    pub fn into_inner(self) -> W {
        self.inner.into_inner()
    }
}

impl<W: Write + Seek> Write for Spliced<W> {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.get_mut().write(buf)
    }

    #[inline]
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.get_mut().flush()
    }
}

impl<W: Write + Seek> Seek for Spliced<W> {
    #[inline]
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.get_mut().seek(pos)
    }
}

impl<W: Write + Seek> DirectBody for Spliced<W> {
    const TAKES_BODY: bool = true;

    fn write_body(&self, data: &[u8]) -> std::io::Result<()> {
        self.inner
            .try_borrow_mut()
            .map_err(|_| std::io::Error::other("zip destination is already borrowed"))?
            .write_all(data)
    }
}

impl<W: Write> DirectBody for ::zip::write::StreamWriter<W> {
    const TAKES_BODY: bool = false;

    fn write_body(&self, _data: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "a streamed zip takes no entry body past its writer",
        ))
    }
}

struct DeflatedChunk {
    data: Vec<u8>,
    crc: crc32fast::Hasher,
}

/// Raw-deflates `input`, primed with `dictionary` (the input right before it), and ends with a
/// sync flush: the output stops on a byte boundary without a final block, ready for the next
/// chunk's output to follow it.
fn deflate_chunk(
    input: &[u8],
    dictionary: &[u8],
    level: u32,
) -> Result<DeflatedChunk, anyhow::Error> {
    let mut crc = crc32fast::Hasher::new();
    crc.update(input);

    let mut compress = flate2::Compress::new(flate2::Compression::new(level), false);
    if !dictionary.is_empty() {
        compress.set_dictionary(dictionary)?;
    }

    let mut data = Vec::with_capacity(input.len() + input.len() / 8 + OUTPUT_HEADROOM);
    loop {
        if data.capacity() - data.len() < OUTPUT_HEADROOM {
            data.reserve(OUTPUT_HEADROOM);
        }

        let before = (compress.total_in(), compress.total_out());
        let consumed = usize::try_from(before.0)?;

        compress.compress_vec(
            input.get_slice(consumed..)?,
            &mut data,
            flate2::FlushCompress::Sync,
        )?;

        let all_in = compress.total_in() == input.len() as u64;
        // the flush is complete once deflate returns with output room to spare
        if all_in && data.len() < data.capacity() {
            break;
        }

        if (compress.total_in(), compress.total_out()) == before {
            // the call before filled the output exactly and had already finished the flush
            if all_in {
                break;
            }

            anyhow::bail!("deflate stopped making progress");
        }
    }

    Ok(DeflatedChunk { data, crc })
}

/// Writes the body of the entry `archive` has just started, deflated (nothing written to it yet):
/// the next `len` bytes of `reader`, deflated in [`CHUNK_BYTES`] chunks on `pool` at `level`.
///
/// At most twice the pool's threads of chunks are in memory at once (input and output), and they
/// are written strictly in order.
pub fn write_deflated_body<Z: Write + Seek + DirectBody>(
    archive: &mut ::zip::ZipWriter<Z>,
    pool: &rayon::ThreadPool,
    reader: &mut impl Read,
    len: u64,
    level: u32,
) -> Result<(), anyhow::Error> {
    let in_flight = (pool.current_num_threads() * 2).clamp(2, MAX_CHUNKS_IN_FLIGHT);
    let (sender, receiver) =
        std::sync::mpsc::channel::<(usize, Result<DeflatedChunk, anyhow::Error>)>();

    let mut pending = BTreeMap::new();
    let mut crc = crc32fast::Hasher::new();
    let mut dictionary = Vec::new();
    let mut remaining = len;
    let mut submitted = 0;
    let mut written = 0;

    loop {
        while remaining > 0 && submitted - written < in_flight {
            let size = remaining.min(CHUNK_BYTES as u64) as usize;
            let mut input = crate::io::mem_buffer(size);
            reader.read_exact(&mut input)?;
            remaining -= size as u64;

            let tail = input
                .get_slice(size.saturating_sub(DICTIONARY_BYTES)..)?
                .to_vec();
            let chunk_dictionary = std::mem::replace(&mut dictionary, tail);

            let index = submitted;
            submitted += 1;

            let sender = sender.clone();
            pool.spawn(move || {
                let chunk = deflate_chunk(&input, &chunk_dictionary, level);

                // a writer that gave up on an earlier error is already gone
                sender.send((index, chunk)).ok();
            });
        }

        if written == submitted {
            break;
        }

        let (index, chunk) = receiver
            .recv()
            .map_err(|_| anyhow::anyhow!("zip deflate worker disconnected"))?;
        pending.insert(index, chunk?);

        while let Some(chunk) = pending.remove(&written) {
            archive
                .get_ref()
                .ok_or_else(|| anyhow::anyhow!("zip writer is already closed"))?
                .write_body(&chunk.data)?;
            crc.combine(&chunk.crc);
            written += 1;
        }
    }

    // SAFETY: exactly `len` bytes of `reader` were deflated into this entry's body, and `crc` is
    // their CRC-32 (the chunks' own, combined in the order the chunks were written)
    unsafe { archive.set_file_metadata(len, crc.finalize())? };

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        io::compression::CompressionLevel,
        server::filesystem::{
            archive::create::{ArchiveProgress, CreateZipOptions, create_zip},
            cap::CapFilesystem,
            virtualfs::IsIgnoredFn,
        },
    };
    use std::{io::Cursor, path::Path};

    /// Log lines, random bytes, zero runs and copies of what came shortly before, so chunks
    /// have both matches inside them and matches reaching back into the chunk before.
    fn mixed(len: usize) -> Vec<u8> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut data: Vec<u8> = Vec::with_capacity(len + 64 * 1024);
        let mut line = 0u64;

        while data.len() < len {
            match next() % 4 {
                0 => {
                    for _ in 0..next() % 64 {
                        let text = format!(
                            "[{line:08}] player_{} joined world_{} at {} {} {}\n",
                            next() % 500,
                            next() % 3,
                            next() % 2048,
                            next() % 256,
                            next() % 2048,
                        );
                        data.extend_from_slice(text.as_bytes());
                        line += 1;
                    }
                }
                1 => {
                    for _ in 0..next() % 2048 {
                        data.push(next() as u8);
                    }
                }
                2 => {
                    let zeros = (next() % 4096) as usize;
                    data.resize(data.len() + zeros, 0);
                }
                _ => {
                    if data.len() < 1024 {
                        continue;
                    }

                    let distance = 1 + (next() as usize) % data.len().min(DICTIONARY_BYTES - 512);
                    let start = data.len() - distance;
                    for i in 0..(next() % 512) as usize + 3 {
                        let byte = data[start + i % distance];
                        data.push(byte);
                    }
                }
            }
        }

        data.truncate(len);
        data
    }

    /// One deflated entry named `body` holding `data`, written by [`write_deflated_body`].
    fn deflated_zip(data: &[u8], threads: usize) -> Result<Vec<u8>, anyhow::Error> {
        let pool = crate::threading::build_pool(threads)?;
        let mut archive = ::zip::ZipWriter::new(Spliced::new(Cursor::new(Vec::new())));

        archive.start_file(
            "body",
            ::zip::write::SimpleFileOptions::default()
                .compression_method(::zip::CompressionMethod::Deflated)
                .compression_level(Some(6))
                .large_file(true),
        )?;
        write_deflated_body(
            &mut archive,
            &pool,
            &mut Cursor::new(data),
            data.len() as u64,
            6,
        )?;

        // an entry after it, so its end has to be right for this one to be found
        archive.start_file("after", ::zip::write::SimpleFileOptions::default())?;
        archive.write_all(b"after the big entry")?;

        Ok(archive.finish()?.into_inner().into_inner())
    }

    fn check_entry(bytes: &[u8], name: &str, data: &[u8]) -> Result<(), anyhow::Error> {
        let mut archive = ::zip::ZipArchive::new(Cursor::new(bytes))?;
        let mut entry = archive.by_name(name)?;

        assert_eq!(entry.compression(), ::zip::CompressionMethod::Deflated);
        assert_eq!(entry.size(), data.len() as u64, "{name} has the wrong size");
        assert_eq!(
            entry.crc32(),
            crc32fast::hash(data),
            "{name} has the wrong crc"
        );

        // reading it to the end also checks the crc against the data
        let mut read = Vec::with_capacity(data.len());
        entry.read_to_end(&mut read)?;
        assert!(read == data, "{name} did not round-trip");

        Ok(())
    }

    /// What the ZipWriter's own deflater writes when an entry it saw no input for is finished.
    fn final_block(level: u32) -> std::io::Result<Vec<u8>> {
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level)).finish()
    }

    // deflate_chunk

    #[test]
    fn chunks_concatenate_into_one_deflate_stream() -> Result<(), anyhow::Error> {
        let data = mixed(3 * CHUNK_BYTES + 12_345);

        let mut stream = Vec::new();
        let mut crc = crc32fast::Hasher::new();
        let mut start = 0;
        while start < data.len() {
            let end = (start + CHUNK_BYTES).min(data.len());
            let dictionary = &data[start.saturating_sub(DICTIONARY_BYTES)..start];

            let chunk = deflate_chunk(&data[start..end], dictionary, 6)?;
            stream.extend_from_slice(&chunk.data);
            crc.combine(&chunk.crc);
            start = end;
        }

        stream.extend_from_slice(&final_block(6)?);

        let mut inflated = Vec::new();
        flate2::read::DeflateDecoder::new(stream.as_slice()).read_to_end(&mut inflated)?;

        assert!(inflated == data, "the chunks did not inflate to the input");
        assert_eq!(crc.finalize(), crc32fast::hash(&data));
        assert!(stream.len() < data.len(), "the chunks did not compress");

        Ok(())
    }

    #[test]
    fn incompressible_chunks_round_trip_at_every_level() -> Result<(), anyhow::Error> {
        // its deflated output is larger than the input
        let mut state = 0x7ee0u64;
        let data: Vec<u8> = (0..CHUNK_BYTES + 3)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();

        for level in [1, 4, 6, 9] {
            let chunk = deflate_chunk(&data, &[], level)?;

            let mut stream = chunk.data;
            stream.extend_from_slice(&final_block(level)?);

            let mut inflated = Vec::new();
            flate2::read::DeflateDecoder::new(stream.as_slice()).read_to_end(&mut inflated)?;
            assert!(inflated == data, "level {level} did not round-trip");
            assert_eq!(chunk.crc.finalize(), crc32fast::hash(&data));
        }

        Ok(())
    }

    // write_deflated_body

    #[test]
    fn a_body_round_trips_at_every_pool_size() -> Result<(), anyhow::Error> {
        let data = mixed(5 * CHUNK_BYTES + 777);

        for threads in [1, 2, 3, 8] {
            check_entry(&deflated_zip(&data, threads)?, "body", &data)?;
        }

        Ok(())
    }

    #[test]
    fn bodies_of_any_length_round_trip() -> Result<(), anyhow::Error> {
        for len in [
            0,
            1,
            DICTIONARY_BYTES - 1,
            DICTIONARY_BYTES,
            CHUNK_BYTES - 1,
            CHUNK_BYTES,
            CHUNK_BYTES + 1,
            2 * CHUNK_BYTES,
        ] {
            let data = mixed(len);
            let bytes = deflated_zip(&data, 4)?;

            check_entry(&bytes, "body", &data)?;

            let mut archive = ::zip::ZipArchive::new(Cursor::new(bytes))?;
            let mut after = String::new();
            archive.by_name("after")?.read_to_string(&mut after)?;
            assert_eq!(
                after, "after the big entry",
                "the entry after {len} bytes broke"
            );
        }

        Ok(())
    }

    #[test]
    fn a_short_reader_fails_the_entry() -> Result<(), anyhow::Error> {
        let pool = crate::threading::build_pool(2)?;
        let mut archive = ::zip::ZipWriter::new(Spliced::new(Cursor::new(Vec::new())));
        archive.start_file(
            "body",
            ::zip::write::SimpleFileOptions::default().large_file(true),
        )?;

        let data = mixed(CHUNK_BYTES);
        let result = write_deflated_body(
            &mut archive,
            &pool,
            &mut Cursor::new(&data),
            data.len() as u64 + 1,
            6,
        );
        assert!(
            result.is_err(),
            "a body shorter than its length was accepted"
        );

        Ok(())
    }

    // create_zip

    fn zip_tree(root: &Path, threads: usize) -> Result<Vec<u8>, anyhow::Error> {
        tokio_test::block_on(async {
            let filesystem = CapFilesystem::new(root).await?;

            let archive = create_zip(
                filesystem,
                Cursor::new(Vec::new()),
                Path::new(""),
                vec!["tree"],
                ArchiveProgress::default(),
                IsIgnoredFn::default(),
                CreateZipOptions {
                    compression_level: CompressionLevel::BestSpeed,
                    threads,
                },
            )
            .await?;

            Ok(archive.into_inner())
        })
    }

    #[test]
    fn a_big_compressible_entry_is_deflated_on_the_pool() -> Result<(), anyhow::Error> {
        let temp = tempfile::tempdir()?;
        std::fs::create_dir_all(temp.path().join("tree"))?;

        // over DIRECT_BYTES (64 MiB), and not a multiple of the chunk size
        let big = mixed(64 * 1024 * 1024 + 3 * CHUNK_BYTES + 4321);
        let small = b"level_spawn=true player_ticks=4096\n".repeat(200);
        std::fs::write(temp.path().join("tree/before.yml"), &small)?;
        std::fs::write(temp.path().join("tree/world.db"), &big)?;
        std::fs::write(temp.path().join("tree/after.yml"), &small)?;

        for threads in [2, 4] {
            let bytes = zip_tree(temp.path(), threads)?;

            check_entry(&bytes, "tree/world.db", &big)?;
            check_entry(&bytes, "tree/before.yml", &small)?;
            check_entry(&bytes, "tree/after.yml", &small)?;

            let mut archive = ::zip::ZipArchive::new(Cursor::new(bytes.as_slice()))?;
            let compressed = archive.by_name("tree/world.db")?.compressed_size();
            assert!(
                compressed < big.len() as u64,
                "the big entry was not compressed with {threads} threads"
            );

            // a third-party reader is the check that matters, ours could share a bug
            let path = temp.path().join(format!("out-{threads}.zip"));
            std::fs::write(&path, &bytes)?;
            match std::process::Command::new("unzip")
                .arg("-t")
                .arg(&path)
                .output()
            {
                Ok(output) => assert!(
                    output.status.success(),
                    "unzip -t rejected the archive: {}",
                    String::from_utf8_lossy(&output.stdout)
                ),
                Err(err) => eprintln!("skipping external unzip check: {err}"),
            }
            std::fs::remove_file(&path)?;
        }

        Ok(())
    }

    #[test]
    fn a_big_entry_in_a_one_thread_pool_keeps_the_old_path() -> Result<(), anyhow::Error> {
        let temp = tempfile::tempdir()?;
        std::fs::create_dir_all(temp.path().join("tree"))?;

        let big = mixed(64 * 1024 * 1024 + 99);
        std::fs::write(temp.path().join("tree/world.db"), &big)?;

        check_entry(&zip_tree(temp.path(), 1)?, "tree/world.db", &big)
    }
}
