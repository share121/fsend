use core::ops::Range;

pub mod invert;
pub mod merge;

pub type ProgressEntry = Range<u64>;

pub trait Total {
    fn total(&self) -> u64;
}

impl Total for ProgressEntry {
    fn total(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

impl Total for Vec<ProgressEntry> {
    fn total(&self) -> u64 {
        self.iter().map(|r| r.total()).sum()
    }
}
