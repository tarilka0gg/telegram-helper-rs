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
    if dim == 0 || matrix.len() % dim != 0 {
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
}
