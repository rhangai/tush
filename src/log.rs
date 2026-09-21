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
//!   LogChunk       packs lines until it is full
//!        │  finished chunks, by swap — never copied
//!        ▼
//!   LocalRingBuffer<LogChunk>     the last `capacity` chunks
//!        │  copied, and only what is new
//!        ▼
//!   LogReader      one per view, its own copy of the same window
//! ```
//!
//! Each stage exists for one reason:
//!
//! - [`LogBufferAny`] is per reader task. A read lands wherever the pipe breaks,
//!   regularly mid character, so it keeps the trailing bytes of an unfinished
//!   character and prepends them to the next read. At most three bytes are
//!   ever held back.
//! - `LogChunk` is the unit of storage: as many whole lines as fit, plus what
//!   it could take of one too long for the room left. It only ever stores
//!   valid UTF-8, so reading it back is a borrow rather than a decode.
//! - The ring is the history proper, guarded by a mutex. A writer takes it,
//!   swaps its finished chunk for the one the ring was about to overwrite,
//!   and lets go — everything else it does happens outside.
//! - [`LogReader`] is per view. Reading from the ring means holding the lock
//!   every writer needs, so a view keeps a copy and goes to the log only for
//!   what has arrived since — which is usually a handful of chunks, and often
//!   none at all.
//!
//! # Why there is no queue between them
//!
//! There was one, with a task draining it into the ring so a writer never
//! waited on the mutex. It cost more than it saved: measured against pushing
//! under the lock it ran 1.8 to 3.2 times slower, because the queue's own
//! hand off, the notification and the task wake up add up to more than an
//! uncontended lock — and the task took that same lock anyway, so the
//! contention moved rather than went.
//!
//! What it did buy was shelter from a slow reader, and that is bought
//! instead by keeping the critical section to one swap: nothing is decoded,
//! allocated or printed while the lock is held, on either side.
//!
//! # Reading without getting in the way
//!
//! A reader holds as many chunks as the log does, which makes the whole
//! relationship one sentence: **caught up, a reader holds exactly what the
//! log holds**. Behind by no more than the ring is long, the sync fetches
//! what is missing; behind by more, what it missed is gone from the log too,
//! so it takes the ring whole and mirrors it again.
//!
//! So there is no hole to represent anywhere, and nothing counts what was
//! lost. Dropping the oldest chunks is a ring doing its job, not an event —
//! it happens constantly, reader or no reader.
//!
//! Finding out that nothing has changed costs one atomic load and no lock,
//! which is what makes a view polling five quiet units almost free.
//!
//! # Where the seam is
//!
//! Everything above the ring is assembling, and none of it needs a log to be
//! worth testing. So [`LogBufferAny`] is generic over [`LogBufferWriter`] —
//! the three calls it actually makes on a log — and [`LogBuffer`] is that
//! filled in with the real one. It is where the fiddliest code in the module
//! stops needing an arena, a ring and a runtime to exercise.
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

mod buffer;
mod chunk;
mod line;
mod log;

#[allow(unused_imports)]
pub use buffer::{LogBuffer, LogBufferAny, LogBufferWriter};

#[allow(unused_imports)]
pub use log::{
    Log, LogLine, LogReader, LogReaderIter, LogReaderRef, LogRegion, LogWriterId, LogWriterNotes,
    LogWriterRef,
};
