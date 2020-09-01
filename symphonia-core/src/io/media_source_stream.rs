// Symphonia
// Copyright (c) 2019 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::cmp;
use std::io;
use std::io::Read;

use super::{ByteStream, MediaSource};

const END_OF_STREAM_ERROR_STR: &str = "end of stream";

/// A `MediaSourceStream` is the common reader type for Symphonia. `MediaSourceStream` uses type
/// erasure to mask the inner reader from the consumer, allowing any typical source to be used.
///
/// `MediaSourceStream` is designed to provide speed and flexibility in a number of challenging I/O
/// scenarios.
///
/// First, to minimize system call overhead, dynamic dispatch overhead on the inner reader, and
/// reduce the work-per-byte read, `MediaSourceStream` implements an exponentially growing
/// read-ahead buffer. The buffer read-ahead length starts at 1kB, and doubles in length as more
/// sequential reads are performed until it reaches 32kB.
///
/// Second, to better support non-seekable sources, `MediaSourceStream` implements stream rewinding.
/// Stream rewinding allows backtracking by up-to either the last read-ahead length or the number of
/// bytes read, which ever is smaller. In other words, a stream is always guaranteed to be
/// rewindable up-to 1kB so long as 1kB has been previously read, otherwise the stream is rewindable
/// by the amount read. The rewind buffer is simply just the last read-ahead buffer, so if the
/// read-ahead length has grown, so too has the maximum rewind length. The stream may be queried for
/// the maximum rewindable length. The rewind buffer is invalidated after a `seek()`.
pub struct MediaSourceStream {
    /// The source reader.
    inner: Box<dyn MediaSource>,

    /// The combined read-ahead/rewind buffer filled from the inner reader.
    buf: Box<[u8]>,

    /// The index of the next readable byte in buf.
    pos: usize,

    /// The index last readable byte in buf.
    end_pos: usize,

    /// The capacity of the read-ahead buffer at this moment. Grows exponentially as more sequential
    /// reads are serviced.
    cur_capacity: usize,

    /// The active partition index.
    part_idx: u32,

    /// Partition information structures.
    part: [Partition; 2],
}

struct Partition {
    base_pos: u64,
    len: usize,
    capacity: usize,
}

impl MediaSourceStream {

    /// The maximum capacity of the read-ahead buffer. Must be a power-of-2.
    const MAX_CAPACITY:  usize = 32 * 1024;

    /// The initial capacity of the read-ahead buffer. Must be less than MAX_CAPACITY, and a
    /// power-of-2.
    const INIT_CAPACITY: usize =      1024;

    pub fn new(source: Box<dyn MediaSource>) -> Self {
        MediaSourceStream {
            inner: source,
            cur_capacity: Self::INIT_CAPACITY,
            buf: vec![0u8; 2 * Self::MAX_CAPACITY].into_boxed_slice(),
            pos: 0,
            end_pos: 0,
            part_idx: 0,
            part: [
                Partition { base_pos: 0, len: 0, capacity: Self::INIT_CAPACITY },
                Partition { base_pos: 0, len: 0, capacity: Self::INIT_CAPACITY },
            ],
        }
    }

    /// Invalidate the read-ahead buffer at the given position.
    fn invalidate(&mut self, base_pos: u64) {
        self.pos = 0;
        self.end_pos = 0;
        self.cur_capacity = Self::INIT_CAPACITY;
        self.part_idx = 0;
        self.part = [
            Partition { base_pos, len: 0, capacity: Self::INIT_CAPACITY },
            Partition { base_pos, len: 0, capacity: Self::INIT_CAPACITY },
        ];
    }

    /// Get the position of the inner reader.
    fn inner_pos(&self) -> u64 {
        cmp::max(
            self.part[0].base_pos + self.part[0].len as u64,
            self.part[1].base_pos + self.part[1].len as u64)
    }

    /// Get the number of bytes buffered but not yet read.
    pub fn buffered_bytes(&self) -> u64 {
        self.inner_pos() - self.pos()
    }

    /// Get the maximum number of rewinable bytes.
    pub fn rewindable_bytes(&self) -> u64 {
        self.pos() - cmp::min(self.part[0].base_pos, self.part[1].base_pos)
    }

    /// Rewinds the stream by the specified number of bytes. Returns the number of bytes actually
    /// rewound.
    pub fn rewind(&mut self, rewind_len: usize) -> usize {
        let cur_idx = self.part_idx as usize & 0x1;
        let alt_idx = cur_idx ^ 0x1;

        // Calculate the desired target position to rewind to.
        let target_pos = self.pos() - rewind_len as u64;

        // The target position is within the current active buffer partition. Rewind the read
        // position boundary.
        if target_pos >= self.part[cur_idx].base_pos {
            self.pos -= rewind_len;
        }
        // The target position is within the previous active buffer partition.
        else if target_pos >= self.part[alt_idx].base_pos {
            // Swap the active buffer index.
            self.part_idx ^= 0x1;

            // Update the read boundaries.
            self.pos = (alt_idx * Self::MAX_CAPACITY) + (target_pos - self.part[alt_idx].base_pos) as usize;
            self.end_pos = self.pos + self.part[alt_idx].len;
        }
        // The target position is outside the stream's buffer entirely.
        else {
            return 0
        }

        rewind_len
    }

    fn fetch_buffer(&mut self) -> io::Result<&[u8]> {
        // Reached the fill length of the active buffer.
        if self.pos >= self.end_pos {

            let cur_idx = self.part_idx as usize & 0x1;
            let alt_idx = cur_idx ^ 0x1;

            // The active buffer partition has a base position less than the previously active
            // buffer partition. That means the stream was rewound. Simply increment the active
            // buffer partition.
            if self.part[cur_idx].base_pos < self.part[alt_idx].base_pos {
                // Update the read boundaries.
                self.pos = alt_idx * Self::MAX_CAPACITY;
                self.end_pos = self.pos + self.part[alt_idx].len;

                // Swap the buffer partitions.
                self.part_idx ^= 0x1;
            }
            // The active buffer partition has a base position greater than the previously active
            // buffer partition. The active partition is at the front of the stream.
            else {
                // The fill length *may* be less than the maximum capacity of the active buffer
                // partition. To maintain the invariant that the rewind buffer partition is always
                // at capacity, then the current active buffer partition must be filled to capacity
                // before swapping.
                if self.part[cur_idx].len < self.part[cur_idx].capacity {
                    let amount = self.part[cur_idx].capacity - self.part[cur_idx].len;
                    let len = self.inner.read(&mut self.buf[self.pos..self.pos + amount])?;

                    // Update the partition information now that the read has succeeded.
                    self.part[cur_idx].len += len;

                    // Update the read boundary.
                    self.end_pos += len;
                }
                // The read-ahead buffer has been filled to capacity, and subsequently read fully.
                // Swap the active buffer partition with the old rewind buffer partition and read in
                // new data from the inner reader.
                else {
                    // Grow the buffer partition capacity exponentially to reduce the overhead of
                    // buffering on seeking.
                    let capacity = cmp::min(self.cur_capacity << 1, Self::MAX_CAPACITY);

                    // Read into the active buffer partition.
                    let pos = alt_idx * Self::MAX_CAPACITY;
                    let len = self.inner.read(&mut self.buf[pos..pos + capacity])?;

                    // Update partition information now that the read has succeeded.
                    self.part[alt_idx].base_pos = self.part[cur_idx].base_pos + self.part[cur_idx].len as u64;
                    self.part[alt_idx].capacity = self.cur_capacity;
                    self.part[alt_idx].len = len;

                    // Swap the active buffer index.
                    self.part_idx ^= 0x1;

                    // Update the current capacity after the read was successful.
                    self.cur_capacity = capacity;

                    // Update the read boundaries.
                    self.pos = pos;
                    self.end_pos = pos + len;
                }
            }
        }

        Ok(&self.buf[self.pos..self.end_pos])
    }

    fn fetch_buffer_or_eof(&mut self) -> io::Result<()> {
        let buffer = self.fetch_buffer()?;

        // The returned buffer will have a length of 0 when EoF is reached. Return an
        // UnexpectedEof in this case since the caller is responsible for ensuring reading past the
        // end of the stream does not occur when using the ByteStream interface.
        if buffer.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, END_OF_STREAM_ERROR_STR));
        }

        Ok(())
    }

}

impl MediaSource for MediaSourceStream {

    #[inline]
    fn is_seekable(&self) -> bool {
        self.inner.is_seekable()
    }

    #[inline]
    fn len(&self) -> Option<u64> {
        self.inner.len()
    }

}

impl io::Read for MediaSourceStream {

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let buf_len = buf.len();

        let mut rem = buf;
        let mut read = 0;

        while !rem.is_empty() {
            // Fetch the readable portion of the MediaSourceStream read-ahead buffer, and read bytes
            // into the remaining portion of the caller's buffer.
            match self.fetch_buffer()?.read(rem) {
                Ok(0) => break,
                Ok(count) => {
                    rem = &mut rem[count..];
                    read += count;

                    self.pos += count;
                },
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => { },
                Err(e) => return Err(e),
            }
        }

        // Return an end-of-stream error if 0 bytes were read when atleast 1 byte was requested.
        if read == 0 && buf_len > 0 {
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, END_OF_STREAM_ERROR_STR))
        }
        else {
            Ok(read)
        }
    }

}

impl io::Seek for MediaSourceStream {

    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        // The current position of the underlying reader is ahead of the current position of the
        // MediaSourceStream by how ever many bytes have not been read from the read-ahead buffer
        // yet. When seeking from the current position adjust the position delta to offset that
        // difference.
        let pos = match pos {
            io::SeekFrom::Current(0) => {
                return Ok(self.pos())
            },
            io::SeekFrom::Current(delta_pos) => {
                let delta = delta_pos - self.buffered_bytes() as i64;
                self.inner.seek(io::SeekFrom::Current(delta))
            },
            _ => {
                self.inner.seek(pos)
            }
        }?;

        self.invalidate(pos);

        Ok(pos)
    }

}

impl ByteStream for MediaSourceStream {

    #[inline(always)]
    fn read_byte(&mut self) -> io::Result<u8> {
        // This function, read_byte, is inlined for performance. To reduce code bloat, place the
        // read-ahead buffer replenishment in a seperate function. Call overhead will be negligible
        // compared to the actual underlying read.
        if self.end_pos - self.pos < 1 {
            self.fetch_buffer_or_eof()?;
        }

        self.pos += 1;
        Ok(self.buf[self.pos - 1])
    }

    // Reads two bytes from the stream and returns them in read-order or an error.
    fn read_double_bytes(&mut self) -> io::Result<[u8; 2]> {
        let mut bytes: [u8; 2] = [0u8; 2];

        // If the buffer has two bytes available, copy directly from it and skip any safety or
        // buffering checks.
        if self.end_pos - self.pos >= 2 {
            bytes.copy_from_slice(&self.buf[self.pos..self.pos + 2]);
            self.pos += 2;
        }
        // If the by buffer does not have two bytes available, copy one byte at a time from the
        // buffer, checking if it needs to be replenished.
        else {
            for byte in &mut bytes {
                if self.pos >= self.end_pos {
                    self.fetch_buffer_or_eof()?;
                }
                *byte = self.buf[self.pos];
                self.pos += 1;
            }
        }

        Ok(bytes)
    }

    // Reads three bytes from the stream and returns them in read-order or an error.
    fn read_triple_bytes(&mut self) -> io::Result<[u8; 3]> {
        let mut bytes: [u8; 3] = [0u8; 3];

        // If the buffer has three bytes available, copy directly from it and skip any safety or
        // buffering checks.
        if self.end_pos - self.pos >= 3 {
            bytes.copy_from_slice(&self.buf[self.pos..self.pos + 3]);
            self.pos += 3;
        }
        // If the by buffer does not have three bytes available, copy one byte at a time from the
        // buffer, checking if it needs to be replenished.
        else {
            for byte in &mut bytes {
                if self.pos >= self.end_pos {
                    self.fetch_buffer_or_eof()?;
                }
                *byte = self.buf[self.pos];
                self.pos += 1;
            }
        }

        Ok(bytes)
    }

    // Reads four bytes from the stream and returns them in read-order or an error.
    fn read_quad_bytes(&mut self) -> io::Result<[u8; 4]> {
        let mut bytes: [u8; 4] = [0u8; 4];

        // If the buffer has four bytes available, copy directly from it and skip any safety or
        // buffering checks.
        if self.end_pos - self.pos >= 4 {
            bytes.copy_from_slice(&self.buf[self.pos..self.pos + 4]);
            self.pos += 4;
        }
        // If the by buffer does not have four bytes available, copy one byte at a time from the
        // buffer, checking if it needs to be replenished.
        else {
            for byte in &mut bytes {
                if self.pos >= self.end_pos {
                    self.fetch_buffer_or_eof()?;
                }
                *byte = self.buf[self.pos];
                self.pos += 1;
            }
        }

        Ok(bytes)
    }

    #[inline(always)]
    fn read_buf(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Implemented via io::Read trait.
        self.read(buf)
    }

    #[inline(always)]
    fn read_buf_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        // Implemented via io::Read trait.
        self.read_exact(buf)
    }

    fn scan_bytes_aligned<'a>(
        &mut self,
        _: &[u8],
        _: usize,
        _: &'a mut [u8]
    ) -> io::Result<&'a mut [u8]> {
        // Intentionally left unimplemented.
        unimplemented!();
    }

    fn ignore_bytes(&mut self, mut count: u64) -> io::Result<()> {
        while count > 0 {
            self.fetch_buffer_or_eof()?;
            let discard_count = cmp::min((self.end_pos - self.pos) as u64, count);
            self.pos += discard_count as usize;
            count -= discard_count;
        }
        Ok(())
    }

    fn pos(&self) -> u64 {
        let idx = self.part_idx as usize & 0x1;
        self.part[idx].base_pos + self.part[idx].len as u64 - (self.end_pos as u64 - self.pos as u64)
    }
}
