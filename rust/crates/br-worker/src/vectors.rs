// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

/// Vector truncation, normalization, and cosine similarity helpers.
pub struct VectorMath;

impl VectorMath {
    /// Matryoshka: keep the first `dim` values, then L2-renormalise.
    pub fn matryoshka(full: &[f32], dim: usize) -> Result<Vec<f32>, String> {
        if dim == 0 || dim > full.len() {
            return Err(format!("dim {dim} not in 1..{}", full.len()));
        }
        let mut v = full[..dim].to_vec();
        Self::l2_normalize_in_place(&mut v);
        Ok(v)
    }

    /// L2-normalizes `v` in place if its squared norm is positive and finite.
    pub fn l2_normalize_in_place(v: &mut [f32]) {
        let sum: f64 = v.iter().map(|&x| (x as f64) * (x as f64)).sum();
        if sum <= 0.0 || !sum.is_finite() {
            return;
        }
        let inv = (1.0 / sum.sqrt()) as f32;
        for x in v.iter_mut() {
            *x *= inv;
        }
    }

    /// Returns `true` if any element is `NaN` or infinite.
    pub fn has_non_finite(v: &[f32]) -> bool {
        v.iter().any(|&x| !x.is_finite())
    }

    /// Cosine similarity in double precision. `NaN` when either side is zero or not finite.
    pub fn cosine(a: &[f32], b: &[f32]) -> Result<f64, &'static str> {
        if a.len() != b.len() {
            return Err("Length mismatch.");
        }
        if Self::has_non_finite(a) || Self::has_non_finite(b) {
            return Ok(f64::NAN);
        }
        let mut dot = 0.0f64;
        let mut na = 0.0f64;
        let mut nb = 0.0f64;
        for (&x, &y) in a.iter().zip(b.iter()) {
            let xf = x as f64;
            let yf = y as f64;
            dot += xf * yf;
            na += xf * xf;
            nb += yf * yf;
        }
        if na == 0.0 || nb == 0.0 {
            return Ok(f64::NAN);
        }
        Ok(dot / (na * nb).sqrt())
    }
}
