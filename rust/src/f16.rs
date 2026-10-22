//! Vectors kept in memory as IEEE half floats: half the memory of f32. The model's vectors are
//! unit length, so each component is well inside f16's range, and its 11-bit precision moves a
//! similarity by about 1e-4 (the f32 vectors in the database stay exact). The dot product
//! widens to f32 as it reads: with F16C on x86-64 (every CPU since ~2013), else portably.

/// Round to the nearest half float (ties to even), subnormals included. Values here are within
/// [-1, 1]; anything past f16's range would saturate to infinity, which never happens for them.
pub fn encode(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let a = x.abs();
    if a.is_nan() {
        return sign | 0x7e00;
    }
    if a >= 65520.0 {
        return sign | 0x7c00;
    }
    if a < 1.0 / 16384.0 {
        // subnormal: a multiple of 2^-24 (1024 of them rounds up into the smallest normal)
        return sign | (a * 16_777_216.0).round_ties_even() as u16;
    }
    let ab = a.to_bits();
    let mant = ab & 0x7f_ffff;
    let exp = (ab >> 23) - 127 + 15;
    let mut h = (exp << 10) | (mant >> 13);
    let rest = mant & 0x1fff;
    if rest > 0x1000 || (rest == 0x1000 && h & 1 == 1) {
        h += 1; // a carry out of the mantissa steps the exponent, which is right
    }
    sign | h as u16
}

/// Exact for every finite half float: its bits moved into an f32's place, then rescaled by the
/// difference in exponent bias (2^112).
#[inline]
pub fn decode(h: u16) -> f32 {
    let h = h as u32;
    f32::from_bits(((h & 0x8000) << 16) | ((h & 0x7fff) << 13)) * f32::from_bits((127 + 112) << 23)
}

pub fn encode_into(dst: &mut Vec<u16>, v: &[f32]) {
    dst.extend(v.iter().map(|&x| encode(x)));
}

/// a · q, `a` in half floats.
#[inline]
pub fn dot(a: &[u16], q: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    if has_f16c() {
        // SAFETY: the CPU has AVX, F16C and FMA (checked at run time)
        return unsafe { dot_f16c(a, q) };
    }
    dot_portable(a, q)
}

fn dot_portable(a: &[u16], q: &[f32]) -> f32 {
    // 8 independent accumulators -> the compiler emits packed SIMD without fast-math
    let mut acc = [0f32; 8];
    let (ca, cq) = (a.chunks_exact(8), q.chunks_exact(8));
    let tail: f32 = ca.remainder().iter().zip(cq.remainder()).map(|(&x, y)| decode(x) * y).sum();
    for (x, y) in ca.zip(cq) {
        for i in 0..8 {
            acc[i] += decode(x[i]) * y[i];
        }
    }
    acc.iter().sum::<f32>() + tail
}

#[cfg(target_arch = "x86_64")]
fn has_f16c() -> bool {
    use std::sync::OnceLock;
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| is_x86_feature_detected!("avx") && is_x86_feature_detected!("f16c") && is_x86_feature_detected!("fma"))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx,f16c,fma")]
unsafe fn dot_f16c(a: &[u16], q: &[f32]) -> f32 {
    use std::arch::x86_64::*;
    let len = a.len().min(q.len());
    let n = len / 16 * 16;
    // SAFETY: every load reads 8 lanes at i or i + 8 with i + 16 <= n <= both lengths
    unsafe {
        let (mut s0, mut s1) = (_mm256_setzero_ps(), _mm256_setzero_ps());
        let mut i = 0;
        while i < n {
            let h0 = _mm_loadu_si128(a.as_ptr().add(i) as *const __m128i);
            let h1 = _mm_loadu_si128(a.as_ptr().add(i + 8) as *const __m128i);
            s0 = _mm256_fmadd_ps(_mm256_cvtph_ps(h0), _mm256_loadu_ps(q.as_ptr().add(i)), s0);
            s1 = _mm256_fmadd_ps(_mm256_cvtph_ps(h1), _mm256_loadu_ps(q.as_ptr().add(i + 8)), s1);
            i += 16;
        }
        let mut lanes = [0f32; 8];
        _mm256_storeu_ps(lanes.as_mut_ptr(), _mm256_add_ps(s0, s1));
        lanes.iter().sum::<f32>() + (n..len).map(|j| decode(a[j]) * q[j]).sum::<f32>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_half_float_survives_the_round_trip() {
        for h in 0..=u16::MAX {
            let x = decode(h);
            if (h & 0x7c00) == 0x7c00 {
                continue; // infinities and NaNs: never stored
            }
            assert_eq!(encode(x), h, "{h:#06x} -> {x}");
        }
    }

    #[test]
    fn encoding_rounds_to_the_nearest() {
        let mut x = -1.0f32;
        while x <= 1.0 {
            let y = decode(encode(x));
            // half a unit in the last place: 2^-11 relative, or 2^-25 for subnormals
            assert!((y - x).abs() <= (x.abs() * 4.9e-4).max(3e-8), "{x} -> {y}");
            x += 0.000_123_7;
        }
        assert_eq!(encode(0.0), 0);
        assert_eq!(encode(-0.0), 0x8000);
        assert_eq!(decode(encode(1.0)), 1.0);
        // a tie goes to the even neighbour: 1 + 2^-11 lies halfway between 1 and 1 + 2^-10
        assert_eq!(encode(1.0 + 1.0 / 2048.0), 0x3c00);
        assert_eq!(encode(1.0 + 3.0 / 2048.0), 0x3c02);
    }

    #[test]
    fn the_dot_product_matches_f32() {
        let a: Vec<f32> = (0..384).map(|i| ((i * 37 % 101) as f32 - 50.0) / 400.0).collect();
        let q: Vec<f32> = (0..384).map(|i| ((i * 53 % 97) as f32 - 48.0) / 390.0).collect();
        let mut h = Vec::new();
        encode_into(&mut h, &a);
        let exact: f32 = a.iter().zip(&q).map(|(x, y)| x * y).sum();
        assert!((dot(&h, &q) - exact).abs() < 1e-3, "{} vs {exact}", dot(&h, &q));
        assert!((dot(&h, &q) - dot_portable(&h, &q)).abs() < 1e-5);
        // a length that isn't a multiple of 16 (or 8)
        assert!((dot(&h[..37], &q[..37]) - dot_portable(&h[..37], &q[..37])).abs() < 1e-5);
    }
}
