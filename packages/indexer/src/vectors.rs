//! 向量存储原语：f32 ⇆ 小端 BLOB 序列化 + cosine 相似度。
//!
//! 向量来自 `model-runtime::embed()`（已 L2 归一化），故 cosine = 点积；
//! 但本函数不假设归一化，按定义算（除以双方模长），对未归一化输入也正确。

/// f32 向量序列化为小端字节（dim × 4 字节）。
#[must_use]
pub fn vector_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// 小端字节反序列化为 f32 向量。长度非 4 倍数 → `None`。
/// 空切片返回 `Some(vec![])`（合法零长，下游 cosine 维度不符会自然记 0 分）。
#[must_use]
pub fn blob_to_vector(b: &[u8]) -> Option<Vec<f32>> {
    if b.len() % 4 != 0 {
        return None;
    }
    Some(
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// cosine 相似度。维度不等或任一为零向量 → `0.0`。
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if !denom.is_finite() || denom == 0.0 {
        return 0.0;
    }
    let sim = dot / denom;
    if sim.is_finite() {
        sim
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp, clippy::approx_constant)]
    use super::*;

    #[test]
    fn blob_round_trip_preserves_values() {
        let v = vec![0.0f32, 1.0, -2.5, 3.14159, f32::MIN, f32::MAX];
        let blob = vector_to_blob(&v);
        assert_eq!(blob.len(), v.len() * 4);
        let back = blob_to_vector(&blob).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn blob_to_vector_rejects_misaligned_len() {
        assert!(blob_to_vector(&[0u8, 1, 2]).is_none());
        assert!(blob_to_vector(&[]).unwrap().is_empty());
    }

    #[test]
    fn cosine_identical_is_one() {
        let v = vec![1.0f32, 2.0, 3.0];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn cosine_dim_mismatch_or_zero_is_zero() {
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn cosine_nan_or_inf_input_is_zero() {
        assert_eq!(cosine(&[f32::NAN, 1.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[f32::INFINITY, 1.0], &[1.0, 1.0]), 0.0);
    }
}
