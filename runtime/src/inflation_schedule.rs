use solana_inflation::Inflation;

const INFLATION_REDUCTION_FACTOR: f64 = 10.0;

fn scaled(mut inflation: Inflation) -> Inflation {
    inflation.initial /= INFLATION_REDUCTION_FACTOR;
    inflation.terminal /= INFLATION_REDUCTION_FACTOR;
    inflation
}

pub fn default_inflation() -> Inflation {
    scaled(Inflation::default())
}

pub fn full_inflation() -> Inflation {
    scaled(Inflation::full())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_inflation_is_scaled_by_ten() {
        let base = Inflation::default();
        let reduced = default_inflation();

        assert_eq!(reduced.initial, base.initial / INFLATION_REDUCTION_FACTOR);
        assert_eq!(reduced.terminal, base.terminal / INFLATION_REDUCTION_FACTOR);
        assert_eq!(reduced.taper, base.taper);
        assert_eq!(reduced.foundation, base.foundation);
        assert_eq!(reduced.foundation_term, base.foundation_term);
    }

    #[test]
    fn test_full_inflation_is_scaled_by_ten() {
        let base = Inflation::full();
        let reduced = full_inflation();

        assert_eq!(reduced.initial, base.initial / INFLATION_REDUCTION_FACTOR);
        assert_eq!(reduced.terminal, base.terminal / INFLATION_REDUCTION_FACTOR);
        assert_eq!(reduced.taper, base.taper);
        assert_eq!(reduced.foundation, 0.0);
        assert_eq!(reduced.foundation_term, 0.0);
    }
}
