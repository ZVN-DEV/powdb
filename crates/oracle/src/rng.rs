//! A tiny deterministic PRNG.
//!
//! The oracle must produce byte-identical case lists on every machine and
//! every run, so it cannot use `std`'s hash-seeded randomness or a third-party
//! generator whose stream could change across versions. SplitMix64 is ~10
//! lines, has a fixed published constant set, and is stable forever.

/// SplitMix64. Deterministic for a given seed, forever.
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-ish index in `0..n`. Panics on `n == 0` because every call site
    /// picks from a non-empty pool; a zero-length pool is a harness bug that
    /// must be loud, not silently rounded to index 0.
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "Rng::below called with an empty range");
        (self.next_u64() % (n as u64)) as usize
    }

    /// Pick a reference from a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// True with probability `numerator/denominator`.
    pub fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        assert!(denominator > 0, "Rng::chance called with zero denominator");
        self.next_u64() % denominator < numerator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stream is pinned. If this changes, every generated case changes and
    /// the recorded divergence set silently shifts underneath CI.
    #[test]
    fn stream_is_pinned() {
        let mut r = Rng::new(0x5057_4442_5157_4C00);
        let got: Vec<u64> = (0..4).map(|_| r.next_u64()).collect();
        assert_eq!(
            got,
            vec![
                14_099_672_909_886_588_497,
                6_675_331_277_712_830_072,
                9_715_574_713_118_705_858,
                5_380_694_554_064_597_549
            ]
        );
    }

    #[test]
    fn below_stays_in_range() {
        let mut r = Rng::new(7);
        for _ in 0..1000 {
            assert!(r.below(5) < 5);
        }
    }
}
