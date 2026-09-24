//! Buffers that hand out pieces of themselves and are written into again.
//!
//! What a request is built from — a path, a header value, a body — is text
//! that something else then keeps: `Uri` and `HeaderValue` hold a [`Bytes`]
//! they are handed rather than copying one. Freezing a piece here gives them
//! a view onto the buffer's own allocation, and the buffer walks back to the
//! start of it once that piece has been dropped.

use parking_lot::Mutex;
use serde::Serialize;
use std::fmt::Write;
use tokio_util::bytes::{BufMut, Bytes, BytesMut};

/// A handful of buffers, of which one is free.
///
/// A buffer cannot be reused while a piece cut from it is still out, so a call
/// takes the first slot that is free and falls back to a buffer of its own
/// when they are all busy. `N` is therefore one more than the pieces that may
/// be alive at the same moment: at that number or fewer it holds the memory
/// and allocates per call anyway, which is the worst of both.
pub struct BytesMutPool<const N: usize> {
    /// What every slot is built at, and what each is asked to have back before
    /// a piece is written — a slot is reclaimed whole rather than by the
    /// length of whatever is about to go into it.
    capacity: usize,
    /// The slots, settled at build time: a call that finds them all busy
    /// allocates for itself instead of growing this.
    bytes_mut: [BytesMut; N],
}

impl<const N: usize> BytesMutPool<N> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes_mut: std::array::from_fn(|_| BytesMut::with_capacity(capacity)),
            capacity,
        }
    }

    /// Hand the first free slot to `f`, or a buffer of its own when every
    /// slot is busy.
    ///
    /// **Cleared before it is asked.** [`try_reclaim`](BytesMut::try_reclaim)
    /// is what reports that the last piece has been dropped, and it counts the
    /// room it needs past whatever is still in the buffer — so bytes left
    /// behind by an `f` that returned before cutting would make that slot fail
    /// the test from then on, and it would never come back.
    ///
    /// `f` is handed the buffer whole and is the one that cuts: that is what
    /// lets a serialization that failed leave the slot to the clear above
    /// rather than unwinding it here.
    fn with_buf<T>(&mut self, size: usize, f: impl FnOnce(&mut BytesMut) -> T) -> T {
        for bytes_mut in &mut self.bytes_mut {
            bytes_mut.clear();
            if bytes_mut.try_reclaim(size) {
                return f(bytes_mut);
            }
        }
        let mut new_buf = BytesMut::with_capacity(size);
        f(&mut new_buf)
    }

    /// One piece of formatted text: a path, a header value.
    ///
    /// The write's result is dropped rather than carried out: a [`BytesMut`]
    /// grows to take what is written and reports a failure only past
    /// `isize::MAX` bytes, which is not an answer a caller could use. A format
    /// string with an argument interpolated has no length to read off it, and
    /// asks for the whole slot instead.
    pub fn write(&mut self, args: std::fmt::Arguments<'_>) -> Bytes {
        let estimate_len = args.as_str().map_or(self.capacity, |s| s.len());
        self.with_buf(estimate_len, |buf| {
            _ = buf.write_fmt(args);
            buf.split().freeze()
        })
    }

    /// One piece of serialized JSON: the body of a command.
    ///
    /// The whole slot is asked for rather than a guess at the length, a body
    /// being the longest thing written here.
    pub fn json<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<Bytes, serde_json::Error> {
        self.with_buf(self.capacity, |buf| {
            serde_json::to_writer(buf.writer(), value)?;
            Ok(buf.split().freeze())
        })
    }
}

/// The same buffers, shared between tasks.
///
/// [`BytesMutPool`] is for one owner; this is for a server answering several
/// requests at once. A slot is taken out for the write and put back after,
/// rather than the lock being held across it: a body that is slow to
/// serialize would otherwise queue every other request behind itself.
///
/// `size` is how many pieces may be alive at the same moment, plus one — for
/// a server that is responses in flight, a piece only becoming reusable once
/// the connection has written it. At that number or fewer, the pool holds its
/// memory and allocates per call anyway. `capacity` is the room each slot
/// gets.
pub struct BytesMutSyncPool {
    /// What every slot is built at, and what each is asked to have back before
    /// a piece is written into it.
    capacity: usize,
    /// The slots. One test covers both ways a slot can be unavailable: an
    /// empty one is checked out, and a full one may still be holding a piece
    /// nobody has dropped — neither passes `try_reclaim`.
    bytes_mut: Mutex<Vec<BytesMut>>,
}

impl BytesMutSyncPool {
    pub fn with_capacity(size: usize, capacity: usize) -> Self {
        let mut storage = Vec::with_capacity(size);
        storage.resize_with(size, || BytesMut::with_capacity(capacity));
        Self {
            capacity,
            bytes_mut: Mutex::new(storage),
        }
    }

    /// The first slot that is free, or nothing while every one of them is
    /// still holding a piece.
    ///
    /// **Cleared before it is asked.** [`try_reclaim`](BytesMut::try_reclaim)
    /// counts the room it needs past whatever is in the buffer, so bytes left
    /// behind by a write that ended early would make that slot fail the test
    /// from then on.
    ///
    /// The lock is released before the guard is handed over, and has to be:
    /// dropping a guard takes it again, and `parking_lot` is not reentrant.
    fn try_aquire(&self, size: usize) -> Option<BytesMutSyncPoolGuard<'_>> {
        let mut lock = self.bytes_mut.lock();
        for (i, bytes_mut) in lock.iter_mut().enumerate() {
            bytes_mut.clear();
            if bytes_mut.try_reclaim(size) {
                return Some(BytesMutSyncPoolGuard {
                    pool: self,
                    bytes_mut: std::mem::take(bytes_mut),
                    index: i,
                });
            }
        }
        None
    }

    /// Hand a slot to `f`, or a buffer of its own when the pool has none free
    /// — one that dies with its piece rather than joining the pool, which is
    /// what keeps a burst from growing it.
    ///
    /// `f` cuts its piece before the guard goes out of scope at the end of the
    /// block, so the split happens on a buffer the pool has not taken back yet.
    fn with_buf<T>(&self, size: usize, f: impl FnOnce(&mut BytesMut) -> T) -> T {
        if let Some(mut guard) = self.try_aquire(size) {
            f(&mut guard.bytes_mut)
        } else {
            let mut new_buf = BytesMut::with_capacity(size);
            f(&mut new_buf)
        }
    }

    /// One piece of formatted text: a header value, or anything else short.
    ///
    /// The write's result is dropped for the reason it is in
    /// [`BytesMutPool::write`], and a format string with an argument
    /// interpolated has no length to read off it, so it asks for the whole
    /// slot.
    pub fn write(&self, args: std::fmt::Arguments<'_>) -> Bytes {
        let estimate_len = args.as_str().map_or(self.capacity, |s| s.len());
        self.with_buf(estimate_len, |buf| {
            _ = buf.write_fmt(args);
            buf.split().freeze()
        })
    }

    /// One piece of serialized JSON: a response body.
    ///
    /// The whole slot is asked for rather than a guess at the length, a body
    /// being the longest thing written here.
    pub fn json<T: ?Sized + Serialize>(&self, value: &T) -> Result<Bytes, serde_json::Error> {
        self.with_buf(self.capacity, |buf| {
            serde_json::to_writer(buf.writer(), value)?;
            Ok(buf.split().freeze())
        })
    }
}

/// One slot, out of the pool for as long as it is being written into.
///
/// A type rather than a buffer and an index handed back loose, for two
/// reasons that are the same reason. Its [`Drop`] returns the buffer down
/// every path, a panic between the acquire and the write included — which
/// otherwise retires that slot for the life of the process, since nothing
/// else ever fills it. And the index exists only in here, so there is no call
/// site that could return a buffer to a slot it did not come from.
struct BytesMutSyncPoolGuard<'a> {
    /// Where it goes back to.
    pool: &'a BytesMutSyncPool,
    /// Taken out of the slot, which is left empty until this is dropped.
    bytes_mut: BytesMut,
    /// Which slot it came from, so putting it back is an assignment rather
    /// than a search for somewhere that looks free.
    index: usize,
}
impl Drop for BytesMutSyncPoolGuard<'_> {
    /// Back to the slot it came from, whatever ended the borrow.
    fn drop(&mut self) {
        self.pool.bytes_mut.lock()[self.index] = std::mem::take(&mut self.bytes_mut);
    }
}

/// One value in its serialized form, kept until the value changes.
///
/// For an answer asked for far more often than it moves — the rows of a
/// session sitting still — where re-serializing per request is the waste and
/// the [`Bytes`] handed out is a refcount rather than a copy. The buffer that
/// produced it is kept beside it, so a rebuild writes into the same
/// allocation.
pub struct BytesReusable {
    /// Where the next value is written: the tail of the same allocation the
    /// last one was cut from.
    bytes_mut: BytesMut,
    /// The value as it stands. Empty before the first build, and again after
    /// one that failed.
    bytes: Bytes,
}

impl BytesReusable {
    pub fn new() -> Self {
        Self {
            bytes_mut: BytesMut::new(),
            bytes: Bytes::new(),
        }
    }

    /// The value as it stands, to clone into an answer.
    ///
    /// A clone still alive at the next rebuild is what stops the buffer being
    /// reclaimed, and that rebuild allocates instead — measured at four
    /// allocations against none. It costs a rebuild, not an answer.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Whether there is nothing to answer with: never built, or built by a
    /// serialization that failed. What a caller tests to decide to build.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Rebuild it from formatted text.
    pub fn write(&mut self, args: std::fmt::Arguments<'_>) {
        self.reclaim();
        _ = self.bytes_mut.write_fmt(args);
        self.bytes = self.bytes_mut.split().freeze();
    }

    /// Rebuild it by serializing `data`.
    ///
    /// A failure leaves it empty rather than stale, so the next caller to ask
    /// builds again instead of answering with what the failure replaced.
    pub fn json<T: ?Sized + Serialize>(&mut self, data: &T) {
        self.reclaim();
        let Self { bytes_mut, bytes } = self;
        let result = serde_json::to_writer(bytes_mut.writer(), data);
        if result.is_ok() {
            *bytes = bytes_mut.split().freeze();
        }
    }

    /// Let go of the last value, which is what makes the buffer writable
    /// again.
    ///
    /// The assignment is the whole of it: dropping the old [`Bytes`] is what
    /// lets the buffer be reclaimed, and `Bytes::clear` does not do that — it
    /// shortens the view and keeps the reference, measured at four
    /// allocations per rebuild against none.
    fn reclaim(&mut self) {
        self.bytes = Bytes::new();
        self.bytes_mut.clear();
    }
}
