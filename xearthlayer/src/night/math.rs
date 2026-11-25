//! Small numeric helpers shared by the lit art reader and the lit
//! generator, so neither depends on the other for them.

/// `x.floor() as i64` without the call to `floor` (not an instruction on
/// the baseline x86-64 target), for every `f64`: a cast truncates towards
/// zero, one less below zero unless already whole. Past the ends of `i64`
/// the cast saturates, and the step down saturates with it.
///
/// Where the floor is also needed as an `f64`, `floor_i64(x) as f64` is
/// `x.floor()` for every `x` below 2^53 in magnitude, except that `-0.0`
/// gives `0.0`: a site that only subtracts it from `x` or casts it gets the
/// same result either way.
pub(crate) fn floor_i64(x: f64) -> i64 {
    let t = x as i64;
    if (t as f64) > x {
        t.saturating_sub(1)
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_i64_is_floor() {
        let mut xs = vec![0.0, -0.0, 1.0, -1.0, 1e15, -1e15, 0.5, -0.5];
        xs.extend((-200_000..200_000).map(|k| f64::from(k) * 0.000_9));
        for k in -400..400 {
            let x = f64::from(k);
            xs.extend([x, x.next_up(), x.next_down()]);
        }
        // Past the ends of i64 the cast saturates, and so must this.
        for x in [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            1e300,
            -1e300,
            f64::MAX,
            f64::MIN,
            i64::MIN as f64,
            i64::MAX as f64,
        ] {
            xs.extend([x, x.next_up(), x.next_down()]);
        }
        for x in xs {
            assert_eq!(floor_i64(x), x.floor() as i64, "{x}");
        }
    }
}
