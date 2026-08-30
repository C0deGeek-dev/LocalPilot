//! Clamp a value into an inclusive range. Original fixture for this repository.

/// Return `v` confined to `lo..=hi`.
pub fn clamp_value(v: i32, lo: i32, hi: i32) -> i32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_below_range() {
        assert_eq!(clamp_value(-5, 0, 10), 0);
    }

    #[test]
    fn clamps_above_range() {
        assert_eq!(clamp_value(42, 0, 10), 10);
    }

    #[test]
    fn keeps_in_range() {
        assert_eq!(clamp_value(7, 0, 10), 7);
    }
}
