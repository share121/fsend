use crate::progress::ProgressEntry;
use std::slice::Iter;

pub struct InvertIter<'a> {
    iter: Iter<'a, ProgressEntry>,
    prev_end: u64,
    total_size: u64,
}

impl<'a> Iterator for InvertIter<'a> {
    type Item = ProgressEntry;

    fn next(&mut self) -> Option<Self::Item> {
        for range in self.iter.by_ref() {
            if range.start > self.prev_end {
                let gap = self.prev_end..range.start;
                self.prev_end = range.end;
                return Some(gap);
            }
            self.prev_end = range.end;
        }
        if self.prev_end < self.total_size {
            let gap = self.prev_end..self.total_size;
            self.prev_end = self.total_size;
            return Some(gap);
        }
        None
    }
}

pub fn invert(progress: Iter<ProgressEntry>, total_size: u64) -> InvertIter {
    InvertIter {
        iter: progress,
        prev_end: 0,
        total_size,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::single_range_in_vec_init)]
    use super::*;

    // 辅助函数，方便测试时将迭代器转为 Vec 比较
    fn invert_vec(progress: &[ProgressEntry], total_size: u64) -> Vec<ProgressEntry> {
        invert(progress.iter(), total_size).collect()
    }

    #[test]
    fn test_reverse_progress() {
        assert_eq!(invert_vec(&[], 10), [0..10]);
        assert_eq!(invert_vec(&[0..5], 10), [5..10]);
        assert_eq!(invert_vec(&[5..10], 10), [0..5]);
        assert_eq!(invert_vec(&[0..5, 7..10], 10), [5..7]);
        assert_eq!(invert_vec(&[0..3, 5..8], 10), [3..5, 8..10]);
        assert_eq!(invert_vec(&[1..3, 5..8], 10), [0..1, 3..5, 8..10]);
    }
}
