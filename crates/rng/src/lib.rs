#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn seed(seed: u64) -> Self {
        let mut sm = SplitMix64::new(seed);
        Self {
            s: [sm.next_u64(), sm.next_u64(), sm.next_u64(), sm.next_u64()],
        }
    }

    #[inline]
    fn rotl(x: u64, k: u32) -> u64 {
        x.rotate_left(k)
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let result = Self::rotl(self.s[0].wrapping_add(self.s[3]), 23).wrapping_add(self.s[0]);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = Self::rotl(self.s[3], 45);
        result
    }

    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    #[inline]
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        debug_assert!(lo < hi, "empty range [{lo}, {hi})");
        lo + self.below(hi - lo)
    }

    #[inline]
    pub fn one_in(&mut self, n: u64) -> bool {
        n != 0 && self.below(n) == 0
    }

    #[inline]
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next_u64() as f64 / u64::MAX as f64) < p
    }

    pub fn fill_bytes(&mut self, buf: &mut [u8]) {
        let mut chunks = buf.chunks_exact_mut(8);
        for c in &mut chunks {
            c.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        let rem = chunks.into_remainder();
        if !rem.is_empty() {
            let bytes = self.next_u64().to_le_bytes();
            rem.copy_from_slice(&bytes[..rem.len()]);
        }
    }

    pub fn choose_index(&mut self, len: usize) -> Option<usize> {
        if len == 0 {
            None
        } else {
            Some(self.below(len as u64) as usize)
        }
    }

    pub fn shuffle<T>(&mut self, xs: &mut [T]) {
        for i in (1..xs.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            xs.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::seed(0xC0FFEE);
        let mut b = Rng::seed(0xC0FFEE);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::seed(1);
        let mut b = Rng::seed(2);
        let mut same = 0;
        for _ in 0..1000 {
            if a.next_u64() == b.next_u64() {
                same += 1;
            }
        }
        assert!(
            same < 5,
            "streams should essentially never collide, got {same}"
        );
    }

    #[test]
    fn below_respects_bound() {
        let mut r = Rng::seed(42);
        for _ in 0..100_000 {
            assert!(r.below(7) < 7);
        }
        assert_eq!(r.below(0), 0);
        assert_eq!(r.below(1), 0);
    }

    #[test]
    fn below_is_roughly_uniform() {
        let mut r = Rng::seed(7);
        let mut counts = [0u64; 10];
        let n = 1_000_000u64;
        for _ in 0..n {
            counts[r.below(10) as usize] += 1;
        }
        let expected = n / 10;
        for c in counts {
            let dev = (c as i64 - expected as i64).unsigned_abs();
            assert!(dev < expected / 10, "bucket {c} too far from {expected}");
        }
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut r = Rng::seed(99);
        let mut xs: Vec<u32> = (0..1000).collect();
        r.shuffle(&mut xs);
        xs.sort_unstable();
        assert!(xs.iter().copied().eq(0..1000));
    }

    #[test]
    fn fill_bytes_deterministic() {
        let mut a = Rng::seed(5);
        let mut b = Rng::seed(5);
        let mut ba = [0u8; 37];
        let mut bb = [0u8; 37];
        a.fill_bytes(&mut ba);
        b.fill_bytes(&mut bb);
        assert_eq!(ba, bb);
    }
}
