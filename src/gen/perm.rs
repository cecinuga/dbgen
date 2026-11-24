//! A keyed bijection of `[0, n)`: a balanced Feistel network over the smallest even
//! number of bits that covers `n`, with cycle walking for values that fall outside.
//!
//! Used wherever rows must map to distinct values without remembering which values were
//! used: one-to-one relations, enumerating the combinations of a composite key, and
//! scattering "hot" parents of a skewed distribution.

use crate::rng::mix;

#[derive(Debug, Clone)]
pub struct Perm {
    n: u64,
    half: u32,
    mask: u64,
    keys: [u64; 4],
}

impl Perm {
    pub fn new(n: u64, key: u64) -> Perm {
        let bits = (64 - n.saturating_sub(1).leading_zeros()).max(2);
        let half = bits.div_ceil(2);
        let mask = if half >= 64 { u64::MAX } else { (1u64 << half) - 1 };
        let keys = [mix(key ^ 1), mix(key ^ 2), mix(key ^ 3), mix(key ^ 4)];
        Perm { n, half, mask, keys }
    }

    pub fn len(&self) -> u64 {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn round_trip(&self, x: u64) -> u64 {
        let mut l = (x >> self.half) & self.mask;
        let mut r = x & self.mask;
        for k in self.keys {
            let f = mix(r ^ k) & self.mask;
            (l, r) = (r, l ^ f);
        }
        (l << self.half) | r
    }

    /// The image of `i`, which must be below `n`.
    pub fn apply(&self, i: u64) -> u64 {
        debug_assert!(i < self.n);
        if self.n <= 1 {
            return 0;
        }
        let mut x = i;
        loop {
            x = self.round_trip(x);
            if x < self.n {
                return x;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_a_bijection() {
        for n in [1u64, 2, 3, 7, 64, 1000, 4097] {
            let p = Perm::new(n, 42);
            let mut seen = vec![false; n as usize];
            for i in 0..n {
                let x = p.apply(i) as usize;
                assert!(!seen[x], "n={n}: {x} twice");
                seen[x] = true;
            }
        }
    }

    #[test]
    fn large_domains() {
        let p = Perm::new(1 << 62, 7);
        let a = p.apply(0);
        let b = p.apply(1);
        assert_ne!(a, b);
        assert!(a < 1 << 62 && b < 1 << 62);
    }
}
