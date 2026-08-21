//! A seedable PRNG and a reservoir sampler, for `sample --seed`.
//!
//! `sample` needs a uniform random subset of a stream whose length it does
//! not know until the last line, without holding the stream in memory.
//! [`Reservoir`] is the textbook answer (Algorithm R): keep the first `k`
//! items, then for the `n`th item after that, replace a uniformly chosen
//! slot with probability `k/n`. [`SplitMix64`] supplies the randomness; it is
//! not cryptographically strong, but it is fast, has no third-party crate,
//! and - unlike `HashMap`'s default hasher - reproduces the same sequence
//! for the same seed across runs and platforms, which is the whole point of
//! `--seed`.
//!
//! ```
//! use jsonl_peek::rng::{Reservoir, SplitMix64};
//!
//! let mut rng = SplitMix64::new(42);
//! let mut reservoir = Reservoir::new(2);
//! for line in 0..10 {
//!     reservoir.add(line, &mut rng);
//! }
//! assert_eq!(reservoir.as_slice().len(), 2);
//! ```

/// A splitmix64 pseudorandom generator.
///
/// This is the generator David Stafford and Sebastiano Vigna describe as
/// `splitmix64`: one 64-bit addition and a fixed sequence of xor-shifts and
/// multiplies. It has no cryptographic properties, but it passes standard
/// statistical test suites, needs eight bytes of state, and - crucially for
/// `--seed` - is defined entirely by its arithmetic, so it is not tied to
/// std's unspecified `RandomState` and will not change output between Rust
/// releases.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Creates a generator from a 64-bit seed. Every seed is valid; there is
    /// no "bad" seed to avoid.
    pub fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    /// Returns the next pseudorandom `u64`.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Returns a pseudorandom integer in `[0, bound)`.
    ///
    /// Uses Lemire's rejection method rather than `next_u64() % bound`, so
    /// every output stays exactly uniform instead of favouring small values
    /// when `bound` does not divide 2^64.
    ///
    /// Panics if `bound` is zero.
    pub fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "below() requires a positive bound");
        let mut product = u128::from(self.next_u64()) * u128::from(bound);
        let mut low = product as u64;
        if low < bound {
            let threshold = bound.wrapping_neg() % bound;
            while low < threshold {
                product = u128::from(self.next_u64()) * u128::from(bound);
                low = product as u64;
            }
        }
        (product >> 64) as u64
    }
}

/// A fixed-capacity uniform random sample of a stream (Algorithm R).
///
/// Feed it every item exactly once, in order, via [`add`](Reservoir::add).
/// Once more than `capacity` items have been added, [`as_slice`] holds a
/// uniform random sample of everything seen so far, with no bias towards
/// items seen earlier or later in the stream. The sample is returned in
/// whatever order the replacements happened to leave it in, not input
/// order; a caller that needs the latter should keep enough of the
/// original item (e.g. a line number) to sort by afterwards.
#[derive(Debug, Clone)]
pub struct Reservoir<T> {
    capacity: usize,
    seen: u64,
    slots: Vec<T>,
}

impl<T> Reservoir<T> {
    /// Creates an empty reservoir that holds at most `capacity` items.
    pub fn new(capacity: usize) -> Self {
        Reservoir {
            capacity,
            seen: 0,
            slots: Vec::with_capacity(capacity),
        }
    }

    /// The reservoir's capacity, as given to [`new`](Reservoir::new).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many items have been passed to [`add`](Reservoir::add) so far.
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// Offers one more item from the stream.
    ///
    /// The first `capacity` items are kept unconditionally. Each item after
    /// that replaces a uniformly chosen slot with probability
    /// `capacity / seen`, which is what keeps every item seen so far equally
    /// likely to end up in the sample.
    pub fn add(&mut self, item: T, rng: &mut SplitMix64) {
        self.seen += 1;
        if self.slots.len() < self.capacity {
            self.slots.push(item);
            return;
        }
        if self.capacity == 0 {
            return;
        }
        let slot = rng.below(self.seen);
        if slot < self.capacity as u64 {
            self.slots[slot as usize] = item;
        }
    }

    /// Borrows the sample collected so far.
    pub fn as_slice(&self) -> &[T] {
        &self.slots
    }

    /// Consumes the reservoir, returning the sample collected so far.
    pub fn into_vec(self) -> Vec<T> {
        self.slots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_reproduces_the_same_sequence() {
        let mut a = SplitMix64::new(1234);
        let mut b = SplitMix64::new(1234);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = SplitMix64::new(1);
        let mut b = SplitMix64::new(2);
        let seq_a: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let seq_b: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_ne!(seq_a, seq_b);
    }

    #[test]
    fn below_never_reaches_bound() {
        let mut rng = SplitMix64::new(9001);
        for _ in 0..10_000 {
            assert!(rng.below(7) < 7);
        }
        // A power of two exercises the fast path (no rejection loop) too.
        for _ in 0..10_000 {
            assert!(rng.below(1) == 0);
        }
    }

    #[test]
    fn reservoir_keeps_every_item_when_capacity_covers_the_stream() {
        let mut rng = SplitMix64::new(1);
        let mut reservoir = Reservoir::new(10);
        for i in 0..5 {
            reservoir.add(i, &mut rng);
        }
        let mut got = reservoir.into_vec();
        got.sort_unstable();
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn reservoir_never_exceeds_capacity() {
        let mut rng = SplitMix64::new(7);
        let mut reservoir = Reservoir::new(3);
        for i in 0..1000 {
            reservoir.add(i, &mut rng);
        }
        assert_eq!(reservoir.as_slice().len(), 3);
        assert_eq!(reservoir.seen(), 1000);
    }

    #[test]
    fn zero_capacity_reservoir_stays_empty() {
        let mut rng = SplitMix64::new(3);
        let mut reservoir: Reservoir<u32> = Reservoir::new(0);
        for i in 0..50 {
            reservoir.add(i, &mut rng);
        }
        assert!(reservoir.as_slice().is_empty());
        assert_eq!(reservoir.seen(), 50);
    }

    #[test]
    fn same_seed_gives_the_same_sample() {
        let sample = |seed| {
            let mut rng = SplitMix64::new(seed);
            let mut reservoir = Reservoir::new(4);
            for i in 0..200 {
                reservoir.add(i, &mut rng);
            }
            reservoir.into_vec()
        };
        assert_eq!(sample(42), sample(42));
    }

    #[test]
    fn every_item_has_roughly_equal_odds_of_survival() {
        // Sample one item out of five, many times over, and check no item is
        // wildly over- or under-represented. This is Algorithm R's whole
        // reason to exist, so it is worth pinning down statistically rather
        // than just checking the capacity invariant.
        const ITEMS: u64 = 5;
        const TRIALS: u64 = 20_000;
        let mut counts = [0u64; ITEMS as usize];
        for seed in 0..TRIALS {
            let mut rng = SplitMix64::new(seed);
            let mut reservoir = Reservoir::new(1);
            for i in 0..ITEMS {
                reservoir.add(i, &mut rng);
            }
            counts[reservoir.as_slice()[0] as usize] += 1;
        }
        let expected = TRIALS / ITEMS;
        for count in counts {
            let low = expected * 8 / 10;
            let high = expected * 12 / 10;
            assert!(
                (low..=high).contains(&count),
                "count {count} outside [{low}, {high}] for expected {expected}"
            );
        }
    }
}
