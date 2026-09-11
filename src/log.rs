//! Capture of a process's output, from raw bytes to a bounded history.
//!
//! One [`Log`] holds the recent output of a unit. It survives restarts — the
//! unit owns it, not the run — so the history of a command is not lost when it
//! is stopped and started again.
//!
//! # The pipeline
//!
//! ```text
//!   child stdout
//!        │  raw bytes, split wherever the pipe felt like it
//!        ▼
//!   LogBuffer      holds a partial character across reads
//!        │  whole characters
//!        ▼
//!   LogChunk       fills until a '\n' or until it is full
//!        │  finished chunks, by swap — never copied
//!        ▼
//!   ThingBuf       lock free hand off, writers never touch the ring
//!        │  drained by the sync task
//!        ▼
//!   LocalRingBuffer<LogChunk>     the last `capacity` chunks
//! ```
//!
//! Each stage exists for one reason:
//!
//! - [`LogBuffer`] is per reader task. A read lands wherever the pipe breaks,
//!   regularly mid character, so it keeps the trailing bytes of an unfinished
//!   character and prepends them to the next read. At most three bytes are
//!   ever held back.
//! - `LogChunk` is the unit of storage: one line, or one `LOG_CHUNK_SIZE`
//!   slice of a line too long to fit. It owns a fixed buffer and only ever
//!   stores valid UTF-8, so reading it back is a borrow rather than a decode.
//! - The [`ThingBuf`] queue decouples the readers from the ring. A writer
//!   swaps its finished chunk for a recycled one and moves on; it never waits
//!   on the mutex that guards the ring.
//! - The ring is the history proper, guarded by a mutex and drained into by a
//!   single background task woken through a [`Notify`].
//!
//! # Why swapping
//!
//! Nothing is copied anywhere along that path. Every chunk is allocated once
//! and then changes hands by [`std::mem::swap`], so a log at capacity stops
//! allocating entirely. The cost is paid up front instead: a log reserves
//! `capacity * LOG_CHUNK_SIZE` bytes whether its lines turn out long or short,
//! which at the current `Unit` default of 4096 chunks is 16 MiB per unit.
//!
//! # Who talks to what
//!
//! [`Log`] is the owner and the read side. [`LogWriterRef`] is the write side,
//! handed to each process that should append; any number may exist at once and
//! their lines land in the order they arrive. The reference is weak on
//! purpose, so a reader task still draining a pipe cannot keep a log alive
//! past its unit.
//!
//! [`ThingBuf`]: thingbuf::StaticThingBuf
//! [`Notify`]: tokio::sync::Notify

mod buffer;
mod chunk;
mod line;
mod log;

#[allow(unused_imports)]
pub use buffer::LogBuffer;

#[allow(unused_imports)]
pub use log::{Log, LogWriterId, LogWriterRef};
