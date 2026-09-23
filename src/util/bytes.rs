//! Buffers that hand out pieces of themselves and are written into again.
//!
//! What a request is built from — a path, a header value, a body — is text
//! that something else then keeps: `Uri` and `HeaderValue` hold a [`Bytes`]
//! they are handed rather than copying one. Freezing a piece here gives them
//! a view onto the buffer's own allocation, and the buffer walks back to the
//! start of it once that piece has been dropped.

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
