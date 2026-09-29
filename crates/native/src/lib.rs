//! Safe wrappers over the Zig C-ABI library in `zig/src/native.zig`.

extern "C" {
    fn tgh_version() -> u32;
    fn tgh_cosine_topk(
        query: *const f32,
        dim: usize,
        matrix: *const f32,
        rows: usize,
        k: usize,
        out_idx: *mut u32,
        out_score: *mut f32,
    ) -> usize;
    fn tgh_fuzzy_score(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> u32;
}

pub fn version() -> u32 {
    unsafe { tgh_version() }
}

/// Top-`k` rows of a row-major `rows x dim` matrix by cosine similarity, best first. `k` is capped at 64.
pub fn cosine_topk(query: &[f32], matrix: &[f32], k: usize) -> Vec<(u32, f32)> {
    let dim = query.len();
    if dim == 0 || !matrix.len().is_multiple_of(dim) {
        return Vec::new();
    }
    let rows = matrix.len() / dim;
    let k = k.min(64).min(rows);
    let (mut idx, mut score) = (vec![0u32; k], vec![0f32; k]);
    let n = unsafe { tgh_cosine_topk(query.as_ptr(), dim, matrix.as_ptr(), rows, k, idx.as_mut_ptr(), score.as_mut_ptr()) };
    idx.into_iter().zip(score).take(n).collect()
}

/// 0..=100 similarity of two names (case-insensitive, Latin + Cyrillic).
pub fn fuzzy_score(a: &str, b: &str) -> u32 {
    unsafe { tgh_fuzzy_score(a.as_ptr(), a.len(), b.as_ptr(), b.len()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_roundtrip() {
        assert_eq!(version(), 1);
        assert_eq!(fuzzy_score("Олександр", "олександр"), 100);
        let m = [1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
        let r = cosine_topk(&[0.0, 1.0], &m, 2);
        assert_eq!(r[0].0, 1);
        assert!((r[0].1 - 1.0).abs() < 1e-6);
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn f32(&mut self) -> f32 {
            (self.below(2001) as f32 - 1000.0) / 100.0
        }
    }

    fn naive_cosine(q: &[f32], row: &[f32]) -> f32 {
        let dot: f64 = q.iter().zip(row).map(|(a, b)| f64::from(*a) * f64::from(*b)).sum();
        let (nq, nr): (f64, f64) = (q.iter().map(|a| f64::from(*a).powi(2)).sum::<f64>().sqrt(), row.iter().map(|a| f64::from(*a).powi(2)).sum::<f64>().sqrt());
        if nq * nr > 0.0 { (dot / (nq * nr)) as f32 } else { 0.0 }
    }

    /// Zig SIMD top-k must agree with a straightforward f64 implementation for every dimension
    /// (below, at and above the 8-lane width, with tails), row count and k.
    #[test]
    fn cosine_topk_matches_naive_reference() {
        let mut r = Rng(0xDEADBEEFCAFEF00D);
        for _ in 0..400 {
            let dim = 1 + r.below(70) as usize;
            let rows = 1 + r.below(120) as usize;
            let k = 1 + r.below(70) as usize;
            let q: Vec<f32> = (0..dim).map(|_| r.f32()).collect();
            let mut m: Vec<f32> = (0..dim * rows).map(|_| r.f32()).collect();
            if rows > 2 && r.below(3) == 0 {
                m[dim..2 * dim].fill(0.0); // a zero row must score 0, not NaN
            }
            let got = cosine_topk(&q, &m, k);
            let mut want: Vec<(usize, f32)> = (0..rows).map(|i| (i, naive_cosine(&q, &m[i * dim..(i + 1) * dim]))).collect();
            want.sort_by(|a, b| b.1.total_cmp(&a.1));
            assert_eq!(got.len(), k.min(64).min(rows), "dim={dim} rows={rows} k={k}");
            for (rank, (idx, score)) in got.iter().enumerate() {
                assert!(score.is_finite() && (-1.001..=1.001).contains(score), "score {score}");
                // scores must match the reference at the same rank (ties may pick different indices)
                assert!((score - want[rank].1).abs() < 1e-3, "rank {rank}: {score} vs {} (dim={dim})", want[rank].1);
                assert!((naive_cosine(&q, &m[*idx as usize * dim..(*idx as usize + 1) * dim]) - score).abs() < 1e-3);
            }
            assert!(got.windows(2).all(|w| w[0].1 >= w[1].1), "not sorted best-first");
            let mut idxs: Vec<u32> = got.iter().map(|g| g.0).collect();
            idxs.sort_unstable();
            idxs.dedup();
            assert_eq!(idxs.len(), got.len(), "duplicate row in result");
        }
    }

    #[test]
    fn cosine_topk_degenerate_inputs() {
        assert!(cosine_topk(&[], &[1.0], 3).is_empty()); // empty query
        assert!(cosine_topk(&[1.0, 2.0], &[1.0, 2.0, 3.0], 3).is_empty()); // ragged matrix
        assert!(cosine_topk(&[1.0], &[], 3).is_empty()); // no rows
        assert!(cosine_topk(&[1.0], &[1.0], 0).is_empty()); // k = 0
        let z = cosine_topk(&[0.0, 0.0], &[1.0, 2.0, 3.0, 4.0], 2); // zero query: all scores 0, no NaN
        assert!(z.iter().all(|x| x.1 == 0.0));
        assert_eq!(cosine_topk(&[1.0], &[1.0; 300], 500).len(), 64); // k capped at 64
    }

    #[test]
    fn fuzzy_properties_on_random_unicode() {
        let alphabet: Vec<char> = "abcXYZ áéíöüñçÅØ Олександр Ярослав ЇЄІҐ їєіґ ёЁ 字漢 😀🎉  -_.@".chars().collect();
        let mut r = Rng(0x1234_5678_9ABC_DEF1);
        let mut gen = |max: u64| -> String { (0..r.below(max)).map(|_| alphabet[r.below(alphabet.len() as u64) as usize]).collect() };
        for _ in 0..4000 {
            let (a, b) = (gen(40), gen(40));
            let (ab, ba) = (fuzzy_score(&a, &b), fuzzy_score(&b, &a));
            assert!(ab <= 100 && ba <= 100, "{a:?} {b:?}");
            assert_eq!(ab, ba, "not symmetric for {a:?} / {b:?}");
            assert_eq!(fuzzy_score(&a, &a), 100, "identity for {a:?}");
        }
        // very long inputs are truncated (256 code points), never a crash
        let long = "я".repeat(5000);
        assert_eq!(fuzzy_score(&long, &long), 100);
        assert!(fuzzy_score(&long, "x") <= 100);
        assert_eq!(fuzzy_score("", ""), 100);
        assert_eq!(fuzzy_score("", "abc"), 0);
    }

    #[test]
    fn fuzzy_realistic_contact_lookups() {
        assert!(fuzzy_score("оля", "Оля Іванова") >= 90);
        assert!(fuzzy_score("Олександр", "Александр") >= 75);
        assert!(fuzzy_score("Ivanova Olga", "Olga Ivanova") >= 90);
        assert!(fuzzy_score("Starfield", "Starfield | Школярі") >= 90);
        assert!(fuzzy_score("mama", "Максим") < 60);
        assert!(fuzzy_score("Андрій", "Максим") < 60);
    }
}
