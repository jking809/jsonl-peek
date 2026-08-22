//! A log-bucketed histogram for approximate percentiles over a stream.
//!
//! `stats` reports percentiles of line length over files with billions of
//! lines, without holding a single length in memory. Sorting is out (that is
//! the whole file), and a fixed-width linear histogram is wrong too: pick
//! buckets sized for a 200-byte record and a 200,000-byte one blows the
//! bucket count, pick buckets sized for the big one and every small record
//! lands in the same bucket. [`Histogram`] instead uses exact buckets for
//! small values and, above that, buckets whose width grows with the value
//! they cover, so relative error stays bounded no matter how wide the range
//! of inputs is. `min`, `max`, `mean` and the total count are tracked
//! separately and are always exact.
//!
//! ```
//! use jsonl_peek::hist::Histogram;
//!
//! let mut hist = Histogram::new();
//! for v in 1..=1000u64 {
//!     hist.add(v);
//! }
//! let p50 = hist.percentile(0.5).unwrap();
//! assert!((p50 as f64 - 500.0).abs() / 500.0 < 0.05);
//! ```

/// Values below this are tracked in one bucket per integer value, so
/// anything at or below it round-trips through the histogram exactly.
const EXACT_MAX: u64 = 16;

/// Linear subdivisions per power-of-two range above `EXACT_MAX`. A value's
/// bucket covers a span of `value / SUBDIV` either side of it at worst, so
/// this bounds the relative error contributed by bucketing to a little
/// under `1 / (2 * SUBDIV)`.
const SUBDIV: u64 = 32;

/// Largest exponent `e` (of `EXACT_MAX << e`) a `u64` value can reach
/// without `EXACT_MAX << e` overflowing.
const MAX_EXP: u32 = 60;

const BUCKET_COUNT: usize = EXACT_MAX as usize + (MAX_EXP as usize) * (SUBDIV as usize);

/// Maps a value to the index of the bucket that covers it.
fn bucket_for(value: u64) -> usize {
    if value < EXACT_MAX {
        return value as usize;
    }
    let ratio = value / EXACT_MAX;
    let e = ratio.ilog2().min(MAX_EXP - 1);
    let low = EXACT_MAX << e;
    let offset = value - low;
    let sub = ((u128::from(offset) * u128::from(SUBDIV)) / u128::from(low)) as u64;
    let sub = sub.min(SUBDIV - 1);
    EXACT_MAX as usize + (e as usize) * (SUBDIV as usize) + sub as usize
}

/// Maps a bucket index back to a representative value: the exact value, for
/// buckets below `EXACT_MAX`, or the midpoint of the bucket's span above it.
fn value_for_bucket(bucket: usize) -> u64 {
    if bucket < EXACT_MAX as usize {
        return bucket as u64;
    }
    let idx = bucket - EXACT_MAX as usize;
    let e = (idx / SUBDIV as usize) as u32;
    let sub = (idx % SUBDIV as usize) as u64;
    let low = EXACT_MAX << e;
    let mid = u128::from(low)
        + (u128::from(2 * sub + 1) * u128::from(low)) / u128::from(2 * SUBDIV);
    mid.min(u128::from(u64::MAX)) as u64
}

/// A log-bucketed histogram of `u64` values, for approximate percentiles.
#[derive(Debug, Clone)]
pub struct Histogram {
    buckets: Vec<u64>,
    count: u64,
    sum: u128,
    min: u64,
    max: u64,
}

impl Histogram {
    /// Creates an empty histogram.
    pub fn new() -> Self {
        Histogram {
            buckets: vec![0; BUCKET_COUNT],
            count: 0,
            sum: 0,
            min: u64::MAX,
            max: 0,
        }
    }

    /// Records one observation.
    pub fn add(&mut self, value: u64) {
        self.count += 1;
        self.sum += u128::from(value);
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.buckets[bucket_for(value)] += 1;
    }

    /// How many values have been recorded.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The smallest value recorded, exactly. `None` if nothing has been
    /// added yet.
    pub fn min(&self) -> Option<u64> {
        (self.count > 0).then_some(self.min)
    }

    /// The largest value recorded, exactly. `None` if nothing has been
    /// added yet.
    pub fn max(&self) -> Option<u64> {
        (self.count > 0).then_some(self.max)
    }

    /// The exact arithmetic mean of every value recorded. `None` if nothing
    /// has been added yet.
    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then_some(self.sum as f64 / self.count as f64)
    }

    /// An approximate percentile, using the nearest-rank method: `p = 0.5`
    /// is the median, `p = 0.99` is the 99th percentile. `None` if nothing
    /// has been added yet.
    ///
    /// `p = 0.0` and `p = 1.0` are special-cased to the exact min and max
    /// rather than a bucket midpoint. Everything else is exact for inputs
    /// at or below 16, and carries bucketing error of a little under 1.6%
    /// above that; see the module documentation.
    ///
    /// # Panics
    ///
    /// Panics if `p` is not in `[0.0, 1.0]`.
    pub fn percentile(&self, p: f64) -> Option<u64> {
        assert!((0.0..=1.0).contains(&p), "percentile out of range: {p}");
        if p == 0.0 {
            return self.min();
        }
        if p == 1.0 {
            return self.max();
        }
        if self.count == 0 {
            return None;
        }
        let rank = ((p * self.count as f64).ceil() as u64).clamp(1, self.count);
        let mut cumulative = 0u64;
        for (bucket, &count) in self.buckets.iter().enumerate() {
            cumulative += count;
            if cumulative >= rank {
                return Some(value_for_bucket(bucket));
            }
        }
        // Every recorded value has a bucket, so this is unreachable.
        self.max()
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_has_no_summary() {
        let hist = Histogram::new();
        assert_eq!(hist.count(), 0);
        assert_eq!(hist.min(), None);
        assert_eq!(hist.max(), None);
        assert_eq!(hist.mean(), None);
        assert_eq!(hist.percentile(0.5), None);
    }

    #[test]
    fn small_values_round_trip_exactly() {
        let mut hist = Histogram::new();
        for v in [0u64, 1, 5, 16] {
            hist.add(v);
        }
        // Each of these is its own bucket, so the median of this 4-item set
        // (rank 2 under nearest-rank) is exactly 1.
        assert_eq!(hist.percentile(0.5), Some(1));
        assert_eq!(hist.min(), Some(0));
        assert_eq!(hist.max(), Some(16));
    }

    #[test]
    fn min_max_mean_are_exact_over_a_wide_range() {
        let mut hist = Histogram::new();
        for v in 1..=10_000u64 {
            hist.add(v);
        }
        assert_eq!(hist.count(), 10_000);
        assert_eq!(hist.min(), Some(1));
        assert_eq!(hist.max(), Some(10_000));
        assert_eq!(hist.mean(), Some(5_000.5));
    }

    #[test]
    fn percentiles_stay_within_error_bound_above_the_exact_range() {
        let mut hist = Histogram::new();
        let n = 1_000_000u64;
        for v in 1..=n {
            hist.add(v);
        }
        for &p in &[0.01, 0.1, 0.5, 0.9, 0.99, 0.999] {
            let want = (p * n as f64).ceil();
            let got = hist.percentile(p).unwrap();
            let relative_error = (got as f64 - want).abs() / want;
            assert!(
                relative_error < 0.02,
                "p{p}: got {got}, want ~{want}, relative error {relative_error}"
            );
        }
    }

    #[test]
    fn percentile_of_a_single_value_is_that_value() {
        let mut hist = Histogram::new();
        hist.add(42);
        assert_eq!(hist.percentile(0.0), Some(42));
        assert_eq!(hist.percentile(1.0), Some(42));
    }

    #[test]
    fn handles_a_value_near_the_top_of_the_range() {
        let mut hist = Histogram::new();
        hist.add(u64::MAX);
        hist.add(1);
        assert_eq!(hist.min(), Some(1));
        assert_eq!(hist.max(), Some(u64::MAX));
        assert_eq!(hist.percentile(1.0), Some(u64::MAX));
    }

    #[test]
    fn bucket_count_matches_the_declared_layout() {
        assert_eq!(
            BUCKET_COUNT,
            EXACT_MAX as usize + MAX_EXP as usize * SUBDIV as usize
        );
        // The largest ratio a u64 value can produce (value / EXACT_MAX) is
        // just under 2^60, so its exponent must fit under MAX_EXP.
        let e = (u64::MAX / EXACT_MAX).ilog2();
        assert!(e < MAX_EXP);
    }
}
