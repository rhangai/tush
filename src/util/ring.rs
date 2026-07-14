use std::{
    collections::{VecDeque, vec_deque::Iter},
    fmt::Write,
};

pub struct RingStr {
    capacity: usize,
    buf: VecDeque<String>,
}

pub struct RingStrIter<'a> {
    slice_iter: std::slice::Iter<'a, String>,
    slice_iter_next: Option<std::slice::Iter<'a, String>>,
}

impl RingStr {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            buf: VecDeque::with_capacity(capacity),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Pushes
    pub fn write_line(&mut self, writer: impl FnOnce(&mut String)) {
        if self.buf.len() >= self.buf.capacity() {
            if let Some(mut line) = self.buf.pop_front() {
                line.clear();
                writer(&mut line);
                self.buf.push_back(line);
                return;
            }
        }

        let mut s = String::new();
        writer(&mut s);
        self.buf.push_back(s);
    }

    /// Pushes
    pub fn extend_lines<'a, I>(&mut self, iter: I) -> usize
    where
        I: Iterator,
        I::Item: AsRef<str>,
    {
        let mut count = 0;
        for line in iter {
            self.write_line(|str| {
                str.write_str(line.as_ref());
            });
            count += 1;
        }
        count
    }

    /// Iterator for whole buf
    pub fn iter<'a>(&'a self) -> RingStrIter<'a> {
        let (slice_a, slice_b) = self.buf.as_slices();
        RingStrIter {
            slice_iter: slice_a.iter(),
            slice_iter_next: if slice_b.len() > 0 {
                Some(slice_b.iter())
            } else {
                None
            },
        }
    }

    /// Iterator for the latest `n` items in the buf
    pub fn iter_latest<'a>(&'a self, n: usize) -> RingStrIter<'a> {
        if n >= self.buf.len() {
            return self.iter();
        } else if n <= 0 {
            return RingStrIter {
                slice_iter: [].iter(),
                slice_iter_next: None,
            };
        }

        let (slice_a, slice_b) = self.buf.as_slices();

        let a_len = slice_a.len();
        let b_len = slice_b.len();
        if n < a_len {
            return RingStrIter {
                slice_iter: slice_a[a_len - n..].iter(),
                slice_iter_next: if b_len > 0 {
                    Some(slice_b.iter())
                } else {
                    None
                },
            };
        }
        let n = n - a_len;
        return RingStrIter {
            slice_iter: slice_b[b_len - n..].iter(),
            slice_iter_next: None,
        };
    }
}

impl<'a> Iterator for RingStrIter<'a> {
    type Item = &'a String;

    fn next(&mut self) -> Option<Self::Item> {
        let value = self.slice_iter.next();
        if value.is_some() {
            return value;
        }

        let next = self.slice_iter_next.take();
        match next {
            Some(iter) => {
                self.slice_iter = iter;
                self.slice_iter.next()
            }
            None => None,
        }
    }
}
