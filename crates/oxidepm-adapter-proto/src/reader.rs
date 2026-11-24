//! The bounded line reader.
//!
//! The reader runs before the JSON is parsed, so it cannot know the method and one limit serves
//! every frame. A line is stored only while it fits in `max_line` bytes; the moment it does not,
//! what was stored is dropped and the rest of the line is skipped without storing it, up to and
//! including the next newline, so the stream is in step again. The buffer therefore never holds
//! more than `max_line` bytes, however long a line the peer sends.

use std::io::{self, BufRead};

use crate::frame::{parse_frame, Frame, FrameError};

/// What the assembler found.
#[derive(Debug, PartialEq, Eq)]
pub enum LineEvent {
    /// A complete line, newline removed.
    Line(Vec<u8>),
    /// A line longer than the limit was skipped; `discarded` counts its bytes, newline excluded.
    Overflow { discarded: u64 },
}

/// The pure state machine under both readers: feed it chunks, it says what it consumed.
#[derive(Debug)]
pub struct LineAssembler {
    max: usize,
    buf: Vec<u8>,
    discarding: bool,
    discarded: u64,
    peak: usize,
}

impl LineAssembler {
    pub fn new(max_line: usize) -> Self {
        LineAssembler {
            max: max_line,
            buf: Vec::new(),
            discarding: false,
            discarded: 0,
            peak: 0,
        }
    }

    /// Feed a chunk. Returns how many bytes of it were used and, if a line ended, the event. Call
    /// again with the rest of the chunk until everything is consumed.
    pub fn feed(&mut self, chunk: &[u8]) -> (usize, Option<LineEvent>) {
        let nl = chunk.iter().position(|&b| b == b'\n');
        let content = nl.unwrap_or(chunk.len());
        let used = nl.map_or(chunk.len(), |p| p + 1);

        if self.discarding {
            self.discarded += content as u64;
            if nl.is_some() {
                self.discarding = false;
                let discarded = std::mem::take(&mut self.discarded);
                return (used, Some(LineEvent::Overflow { discarded }));
            }
            return (used, None);
        }

        if self.buf.len() + content > self.max {
            self.discarded = (self.buf.len() + content) as u64;
            self.buf = Vec::new();
            if nl.is_some() {
                let discarded = std::mem::take(&mut self.discarded);
                return (used, Some(LineEvent::Overflow { discarded }));
            }
            self.discarding = true;
            return (used, None);
        }

        self.reserve_for(content);
        self.buf.extend_from_slice(&chunk[..content]);
        self.peak = self.peak.max(self.buf.capacity());
        if nl.is_some() {
            return (used, Some(LineEvent::Line(std::mem::take(&mut self.buf))));
        }
        (used, None)
    }

    /// Grow the buffer by doubling, but never past `max`, so its capacity stays within the limit.
    fn reserve_for(&mut self, extra: usize) {
        let need = self.buf.len() + extra;
        if need <= self.buf.capacity() {
            return;
        }
        let target = need
            .max(self.buf.capacity().saturating_mul(2))
            .max(256)
            .min(self.max.max(need));
        self.buf.reserve_exact(target - self.buf.len());
    }

    /// Call at end of input. Returns the number of bytes of an unfinished line that were dropped
    /// (0 when the stream ended on a line boundary). An unfinished line is a truncated frame.
    pub fn finish(&mut self) -> usize {
        let pending = if self.discarding {
            self.discarded as usize
        } else {
            self.buf.len()
        };
        self.buf = Vec::new();
        self.discarding = false;
        self.discarded = 0;
        pending
    }

    /// The bytes stored right now.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The largest buffer capacity this assembler has had. Used by tests of the memory bound.
    pub fn peak_capacity(&self) -> usize {
        self.peak
    }
}

/// One result of reading a frame.
#[derive(Debug, PartialEq)]
pub enum Read {
    Frame(Frame),
    /// A line that is not a valid frame. The stream is still in step.
    Violation(FrameError),
    /// A line over the limit was skipped.
    Overflow {
        discarded: u64,
    },
    /// The input ended in the middle of a line (`bytes` stored or skipped so far).
    Truncated {
        bytes: usize,
    },
    Eof,
}

fn classify(event: LineEvent) -> Read {
    match event {
        LineEvent::Line(line) => match parse_frame(&line) {
            Ok(frame) => Read::Frame(frame),
            Err(e) => Read::Violation(e),
        },
        LineEvent::Overflow { discarded } => Read::Overflow { discarded },
    }
}

/// Frame reader over a blocking `BufRead`.
pub struct FrameReader<R> {
    inner: R,
    asm: LineAssembler,
}

impl<R: BufRead> FrameReader<R> {
    pub fn new(inner: R, max_line: usize) -> Self {
        FrameReader {
            inner,
            asm: LineAssembler::new(max_line),
        }
    }

    /// The next frame, or why there is none. Reads at most `max_line` bytes into memory per line.
    pub fn read_frame(&mut self) -> io::Result<Read> {
        loop {
            let chunk = match self.inner.fill_buf() {
                Ok(c) => c,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if chunk.is_empty() {
                return Ok(match self.asm.finish() {
                    0 => Read::Eof,
                    bytes => Read::Truncated { bytes },
                });
            }
            let (used, event) = self.asm.feed(chunk);
            self.inner.consume(used);
            if let Some(event) = event {
                return Ok(classify(event));
            }
        }
    }

    pub fn assembler(&self) -> &LineAssembler {
        &self.asm
    }
}

/// Frame reader over a tokio `AsyncBufRead`. Cancel-safe: the partial line lives in the reader.
#[cfg(feature = "tokio")]
pub struct AsyncFrameReader<R> {
    inner: R,
    asm: LineAssembler,
}

/// The result of consuming one buffer segment. Partial lines still account for their bytes.
#[cfg(feature = "tokio")]
pub struct ReadStep {
    pub consumed: usize,
    pub event: Option<Read>,
    pub overflow_started: bool,
    pub oversized_line_bytes: u64,
}

#[cfg(feature = "tokio")]
impl<R: tokio::io::AsyncBufRead + Unpin> AsyncFrameReader<R> {
    pub fn new(inner: R, max_line: usize) -> Self {
        AsyncFrameReader {
            inner,
            asm: LineAssembler::new(max_line),
        }
    }

    pub async fn read_frame(&mut self) -> io::Result<Read> {
        loop {
            if let Some(event) = self.read_step().await?.event {
                return Ok(event);
            }
        }
    }

    pub async fn read_step(&mut self) -> io::Result<ReadStep> {
        use tokio::io::AsyncBufReadExt;
        loop {
            let chunk = match self.inner.fill_buf().await {
                Ok(c) => c,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if chunk.is_empty() {
                let event = match self.asm.finish() {
                    0 => Read::Eof,
                    bytes => Read::Truncated { bytes },
                };
                return Ok(ReadStep {
                    consumed: 0,
                    event: Some(event),
                    overflow_started: false,
                    oversized_line_bytes: 0,
                });
            }
            let was_discarding = self.asm.discarding;
            let (used, event) = self.asm.feed(chunk);
            std::pin::Pin::new(&mut self.inner).consume(used);
            let oversized_line_bytes = match &event {
                Some(LineEvent::Overflow { discarded }) => *discarded,
                _ => self.asm.discarded,
            };
            return Ok(ReadStep {
                consumed: used,
                event: event.map(classify),
                overflow_started: !was_discarding && oversized_line_bytes > 0,
                oversized_line_bytes,
            });
        }
    }

    pub fn assembler(&self) -> &LineAssembler {
        &self.asm
    }
}
