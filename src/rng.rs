//! Counter-based deterministic randomness.
//!
//! Every generated value is a pure function of `(seed, stream tags, row index, ...)`, never
//! of the order in which batches are produced. That is what makes the output independent of
//! `batch_size`, `workers` and resume points, and lets a child table compute the key of the
//! parent row it references without the parent being in memory or in the database.
//!
//! The algorithms are implemented here rather than taken from `rand` on purpose: a crate
//! upgrade that changed a generator would silently change every generated dataset.

const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// FNV-1a, usable in `const` context to derive stable stream tags from names.
pub const fn tag(s: &str) -> u64 {
    let bytes = s.as_bytes();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    h
}

/// SplitMix64 finalizer: a bijective, well-avalanching 64-bit mix.
#[inline]
pub const fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Combines any number of parts into one key. Order-sensitive.
#[inline]
pub fn hash(parts: &[u64]) -> u64 {
    let mut h = 0x243F_6A88_85A3_08D3u64;
    for &p in parts {
        h = mix(h ^ p.wrapping_mul(GOLDEN));
    }
    h
}

/// A uniform draw in `[0, 1)` from a key, without building an [`Rng`].
#[inline]
pub fn unit(key: u64) -> f64 {
    (mix(key) >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

/// SplitMix64 generator. Cheap enough to create one per value.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// The stream of `key`.
    pub fn new(key: u64) -> Self {
        Rng(mix(key ^ GOLDEN))
    }

    /// The stream of a combination of parts (seed, tags, row index...).
    pub fn at(parts: &[u64]) -> Self {
        Rng::new(hash(parts))
    }

    /// The next 64 random bits.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GOLDEN);
        mix(self.0)
    }

    /// Uniform in `[0, 1)`.
    #[inline]
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform in `[0, n)`; `n` must be positive.
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// Uniform in `[0, n)` for 128-bit spans.
    pub fn below_u128(&mut self, n: u128) -> u128 {
        debug_assert!(n > 0);
        if n <= u64::MAX as u128 {
            return self.below(n as u64) as u128;
        }
        let x = ((self.next_u64() as u128) << 64) | self.next_u64() as u128;
        x % n
    }

    /// Uniform in `[lo, hi]`.
    pub fn between_i128(&mut self, lo: i128, hi: i128) -> i128 {
        debug_assert!(hi >= lo);
        let span = (hi - lo) as u128;
        if span == u128::MAX {
            return (((self.next_u64() as u128) << 64) | self.next_u64() as u128) as i128;
        }
        lo + self.below_u128(span + 1) as i128
    }

    /// True with probability `p`.
    #[inline]
    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }

    /// Standard normal (Box-Muller, one value per call).
    pub fn normal(&mut self) -> f64 {
        let u1 = 1.0 - self.f64();
        let u2 = self.f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    /// Index drawn with probability proportional to `cumulative` weights (running sums).
    pub fn weighted_cumulative(&mut self, cumulative: &[u64]) -> usize {
        let total = *cumulative.last().expect("non-empty weights");
        let x = self.below(total.max(1));
        cumulative.partition_point(|&c| c <= x).min(cumulative.len() - 1)
    }

    /// A uniformly chosen item; `items` must not be empty.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    /// A version-4 formatted UUID as a `u128` (RFC 9562 bit layout).
    pub fn uuid_v4(&mut self) -> u128 {
        let hi = (self.next_u64() & !0xF000) | 0x4000;
        let lo = (self.next_u64() & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
        ((hi as u128) << 64) | lo as u128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_key_same_stream() {
        let mut a = Rng::at(&[1, 2, 3]);
        let mut b = Rng::at(&[1, 2, 3]);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::at(&[1, 2, 3]).next_u64(), Rng::at(&[1, 2, 4]).next_u64());
    }

    #[test]
    fn values_are_pinned_across_builds() {
        // Changing these numbers changes every dataset ever generated with a given seed:
        // bump MODEL_VERSION (src/plan.rs) if that is ever intended.
        assert_eq!(tag(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(tag("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(mix(0), 0);
        assert_eq!(mix(1), 0x5692_161d_100b_05e5);
    }

    #[test]
    fn ranges_hold() {
        let mut r = Rng::new(7);
        for _ in 0..10_000 {
            assert!(r.below(13) < 13);
            let x = r.between_i128(-5, 5);
            assert!((-5..=5).contains(&x));
            let f = r.f64();
            assert!((0.0..1.0).contains(&f));
        }
        assert_eq!(r.between_i128(3, 3), 3);
    }

    #[test]
    fn weighted_respects_zero_weights() {
        let mut r = Rng::new(9);
        // weights 3, 0, 5
        let cumulative = [3, 3, 8];
        for _ in 0..1_000 {
            assert_ne!(r.weighted_cumulative(&cumulative), 1);
        }
    }

    #[test]
    fn uuid_has_version_and_variant() {
        let mut r = Rng::new(1);
        for _ in 0..100 {
            let u = r.uuid_v4();
            assert_eq!((u >> 76) & 0xF, 4);
            assert_eq!((u >> 62) & 0x3, 0b10);
        }
    }
}
