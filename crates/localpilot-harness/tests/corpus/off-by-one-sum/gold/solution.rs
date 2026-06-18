//! Sum the integers 1..=n. Original fixture for this repository.

/// The sum of every integer from 1 through `n` inclusive.
pub fn sum_to(n: u32) -> u32 {
    (1..=n).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_to_five() {
        assert_eq!(sum_to(5), 15);
    }

    #[test]
    fn sums_to_one() {
        assert_eq!(sum_to(1), 1);
    }
}
