use std::{collections::VecDeque, num::NonZeroUsize};

use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use thingbuf::ThingBuf;

const EXTRA_CAPACITY_CONSUME: usize = 4;
const EXTRA_CAPACITY_THRESHOLD: usize = 64;
const EXTRA_CAPACITY_THRESHOLD_LIMIT: usize = 256;

pub struct RingStrStorage {
    buf: VecDeque<String>,
    offset: u128,
}

impl RingStrStorage {
    fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity),
            offset: 0,
        }
    }

    fn push(&mut self, v: &String) {
        self.raw_push(v);
        self.offset = self.offset.wrapping_add(1);
    }

    fn raw_push(&mut self, v: impl AsRef<str>) {
        if self.buf.capacity() == 0 {
            return;
        }
        if self.buf.len() >= self.buf.capacity() {
            let mut item = self
                .buf
                .pop_front()
                .expect("Could not pop from VecDeque even though the check above allows it");
            item.clear();
            item.push_str(v.as_ref());
            self.buf.push_back(item);
        } else {
            self.buf.push_back(v.as_ref().into());
        }
    }

    fn raw_extend<I>(&mut self, other: I)
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        for i in other {
            self.raw_push(i);
        }
    }

    fn sync_data(&mut self, other: &RingStrStorage) {
        if self.offset >= other.offset {
            return;
        }

        let other_len = other.buf.len() as u128;
        let n = other.offset - self.offset;
        let cap = self.buf.capacity() as u128;
        if n >= cap || n > other_len {
            self.raw_extend(other.buf.iter());
            self.offset = other.offset;
            return;
        }

        let skip = other.buf.len() as u128 - n;
        self.raw_extend(other.buf.iter().skip(skip as usize));
        self.offset = other.offset;
    }

    pub fn lines(&self) -> impl Iterator<Item = &String> {
        self.buf.iter()
    }
}

impl Clone for RingStrStorage {
    fn clone(&self) -> Self {
        let mut buf: VecDeque<String> = VecDeque::with_capacity(self.buf.capacity());
        for i in self.buf.iter() {
            buf.push_back(i.clone());
        }
        Self {
            buf,
            offset: self.offset.clone(),
        }
    }
}

pub struct RingStr {
    capacity: NonZeroUsize,
    buf: RwLock<RingStrStorage>,
    queue: ThingBuf<String>,
}

impl RingStr {
    pub fn new(capacity: usize) -> Self {
        let buf = RingStrStorage::new(capacity);
        Self {
            capacity: NonZeroUsize::new(capacity).unwrap(),
            buf: RwLock::new(buf),
            queue: ThingBuf::new(capacity + EXTRA_CAPACITY_THRESHOLD_LIMIT),
        }
    }

    /// Get the iter and syncs it if needed
    pub fn create_storage(&self) -> RingStrStorage {
        let buf = self.sync_lock();
        buf.clone()
    }

    /// Get the iter and syncs it if needed
    pub fn update_storage(&self, storage: &mut RingStrStorage) {
        let buf = self.sync_lock();
        storage.sync_data(&buf);
    }

    /// Sync the data and get the internal read lock guard
    fn sync_lock(&self) -> RwLockReadGuard<'_, RingStrStorage> {
        let mut buf = self.buf.write();

        let mut limit = self.capacity.get() + EXTRA_CAPACITY_THRESHOLD_LIMIT;
        while let Some(line) = self.queue.pop_ref() {
            buf.push(&line);
            limit -= 1;
            if limit == 0 {
                break;
            }
        }
        RwLockWriteGuard::downgrade(buf)
    }

    /// Write a new line
    pub fn write_line(&self, line: impl AsRef<str>) {
        self.use_line(|s| s.push_str(line.as_ref()));
    }

    /// Write a new line
    fn use_line(&self, writer: impl FnOnce(&mut String)) {
        let len = self.queue.len();
        let capacity = self.capacity.get();
        if len >= capacity + EXTRA_CAPACITY_THRESHOLD {
            for _ in 0..EXTRA_CAPACITY_CONSUME {
                if self.queue.len() > capacity {
                    self.queue.pop_ref();
                }
            }
        }
        if let Ok(mut line) = self.queue.push_ref() {
            line.clear();
            writer(&mut line);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn log() {
        let ring = RingStr::new(3);
        let mut storage = ring.create_storage();
        ring.write_line("oi");
        ring.write_line("tudo");
        ring.write_line("bem");
        ring.write_line("oi");
        ring.write_line("tudo");
        ring.write_line("bem");
        assert_ring(&storage, &[]);
        ring.update_storage(&mut storage);
        assert_ring(&storage, &["oi", "tudo", "bem"]);
    }

    fn assert_ring(storage: &RingStrStorage, expected: &[&str]) {
        let values: Vec<String> = storage.lines().map(|s| s.clone()).collect();
        assert_eq!(&values, expected);
    }
}
