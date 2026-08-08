//! GF(2^8) arithmetic for Q-parity reconstruction.
//!
//! Field: GF(2^8) with reduction byte 0x1d (x^8 + x^4 + x^3 + x^2 + 1, the
//! Linux raid6 field), generator g = 2. Q at each offset is XOR over slots
//! j of g^(j-1) * D_j.

/// 256x256 multiplication table: `GFMUL[a][b] = a * b` in the field.
pub static GFMUL: [[u8; 256]; 256] = build_gfmul();

/// Powers of the generator: `GFEXP[e] = g^e` for e in 0..255. Entry 255
/// is a 0 sentinel (g^255 == 1 would alias entry 0).
pub static GFEXP: [u8; 256] = build_gfexp();

/// Multiplicative inverses: `GFINV[x] = x^-1 = x^254`; `GFINV[0] = 0`
/// (0 has no inverse).
pub static GFINV: [u8; 256] = build_gfinv();

/// `GFEXI[e] = (g^e XOR 1)^-1` — the `pbmul` table for the two-disk solve.
pub static GFEXI: [u8; 256] = build_gfexi();

const fn gf_mul_scalar(mut a: u8, mut b: u8) -> u8 {
    let mut v: u8 = 0;
    while b != 0 {
        if b & 1 != 0 {
            v ^= a;
        }
        // Multiply by x: shift left, reduce by the field polynomial when
        // the high bit escapes.
        let hi = a & 0x80;
        a = (a << 1) ^ (if hi != 0 { 0x1d } else { 0 });
        b >>= 1;
    }
    v
}

const fn build_gfmul() -> [[u8; 256]; 256] {
    let mut t = [[0u8; 256]; 256];
    let mut a = 0;
    while a < 256 {
        let mut b = 0;
        while b < 256 {
            t[a][b] = gf_mul_scalar(a as u8, b as u8);
            b += 1;
        }
        a += 1;
    }
    t
}

const fn build_gfexp() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut v: u8 = 1;
    let mut i = 0;
    while i < 255 {
        t[i] = v;
        // v <- v * 2 in the field.
        let hi = v & 0x80;
        v = (v << 1) ^ (if hi != 0 { 0x1d } else { 0 });
        i += 1;
    }
    // g^255 == 1 would alias entry 0; leave 255 as a 0 sentinel.
    t[255] = 0;
    t
}

// x^p by repeated squaring; const-eval helper for the table builders.
const fn gf_pow_const(x: u8, mut p: u32) -> u8 {
    let mut v: u8 = 1;
    let mut a = x;
    while p > 0 {
        if p & 1 != 0 {
            v = gf_mul_scalar(v, a);
        }
        a = gf_mul_scalar(a, a);
        p >>= 1;
    }
    v
}

const fn build_gfinv() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = if i == 0 {
            0
        } else {
            gf_pow_const(i as u8, 254)
        };
        i += 1;
    }
    t
}

const fn build_gfexi() -> [u8; 256] {
    // GFINV and GFEXP are already const-built; compose at compile time.
    let gfinv = build_gfinv();
    let gfexp = build_gfexp();
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = gfinv[(gfexp[i] ^ 1) as usize];
        i += 1;
    }
    t
}

/// `g^e`, with the exponent taken mod 255 (g^255 == g^0).
pub fn gf_exp(e: i32) -> u8 {
    let e = e.rem_euclid(255);
    GFEXP[e as usize]
}

/// `x^-1 = x^254`; `gf_inv(0)` is undefined and returns 0.
#[allow(dead_code)]
pub fn gf_inv(x: u8) -> u8 {
    if x == 0 {
        return 0;
    }
    gf_pow(x, 254)
}

/// `x^p` by repeated squaring, exponent normalized mod 255 (negative
/// allowed).
#[allow(dead_code)]
pub fn gf_pow(x: u8, mut p: i32) -> u8 {
    p %= 255;
    if p < 0 {
        p += 255;
    }
    let mut v: u8 = 1;
    let mut a = x;
    while p > 0 {
        if p & 1 != 0 {
            v = GFMUL[v as usize][a as usize];
        }
        a = GFMUL[a as usize][a as usize];
        p >>= 1;
    }
    v
}

/// Field multiplication via the precomputed table.
#[allow(dead_code)]
pub fn gf_mul(a: u8, b: u8) -> u8 {
    GFMUL[a as usize][b as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gfmul_table_is_consistent() {
        // g * 1 == g and g * 0 == 0 for every g.
        for g in 0..=255u8 {
            assert_eq!(gf_mul(g, 0), 0);
            assert_eq!(gf_mul(g, 1), g);
        }
    }

    #[test]
    fn gfexp_matches_double_and_reduce() {
        // GFEXP[e] must equal 2 multiplied by itself e times.
        let mut v: u8 = 1;
        for (e, gfexp) in GFEXP.iter().enumerate().take(255) {
            assert_eq!(gfexp, &v, "GFEXP[{e}]");
            let hi = v & 0x80;
            v = (v << 1) ^ (if hi != 0 { 0x1d } else { 0 });
        }
    }

    #[test]
    fn gf_inv_round_trip() {
        for x in 1..=255u8 {
            let inv = gf_inv(x);
            assert_ne!(inv, 0, "gf_inv({x}) = 0");
            assert_eq!(gf_mul(x, inv), 1, "gf_inv({x}) = {inv}");
        }
    }

    #[test]
    fn gf_exp_period_255() {
        // g^255 == g^0 == 1.
        assert_eq!(gf_exp(0), 1);
        assert_eq!(gf_exp(255), 1);
        assert_eq!(gf_exp(256), 2);
        assert_eq!(gf_exp(-1), gf_exp(254));
    }

    #[test]
    fn q_syndrome_matches_gen_syndrome_pattern() {
        // Cross-check the kernel's gen_syndrome walk against the closed
        // form Q = XOR g^(j-1) * D_j for a 3-disk stripe.
        let d: [u8; 3] = [0x11, 0x22, 0x80];
        // 3 data disks, slots 1..=3.
        let n = d.len();

        // gen_syndrome walk: wq = d[z0]; stepping down: wq = (wq*2) ^ d[z].
        let z0 = n - 1;
        let mut wq: u8 = d[z0];
        for z in (0..z0).rev() {
            wq = gf_mul(wq, 2) ^ d[z];
        }

        // Closed form: Q = XOR over slots j of g^(j-1) * D_j.
        let mut q: u8 = 0;
        for j in 1..=n {
            q ^= gf_mul(gf_exp((j - 1) as i32), d[j - 1]);
        }
        assert_eq!(wq, q, "gen_syndrome walk vs closed-form Q");
    }

    #[test]
    fn recover_one_disk_via_q() {
        // Reconstruct slot k (1-based) from Q + the other slots:
        // D_k = g^-(k-1) * (Q XOR XOR_{j!=k} g^(j-1) * D_j).
        let d: [u8; 4] = [0x01, 0x02, 0x04, 0x08];
        let n = d.len();
        let mut q: u8 = 0;
        for j in 1..=n {
            q ^= gf_mul(gf_exp((j - 1) as i32), d[j - 1]);
        }
        let k = 2;
        let mut acc: u8 = q;
        for j in 1..=n {
            if j == k {
                continue;
            }
            acc ^= gf_mul(gf_exp((j - 1) as i32), d[j - 1]);
        }
        let recovered = gf_mul(gf_exp(-((k - 1) as i32)), acc);
        assert_eq!(recovered, d[k - 1]);
    }
}
