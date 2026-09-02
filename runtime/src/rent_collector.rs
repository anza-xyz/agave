use {
    solana_clock::Epoch, solana_epoch_schedule::EpochSchedule,
    solana_genesis_config::GenesisConfig, solana_rent::Rent,
};

#[allow(deprecated)]
const SIMD0194_EXEMPTION_THRESHOLD: [u8; 8] = [0, 0, 0, 0, 0, 0, 240, 63];
#[allow(deprecated)]
const CURRENT_EXEMPTION_THRESHOLD: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 64];

#[cfg_attr(feature = "frozen-abi", derive(solana_frozen_abi_macro::AbiExample))]
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RentCollector {
    pub epoch: Epoch,
    pub epoch_schedule: EpochSchedule,
    pub slots_per_year: f64,
    pub rent: Rent,
}

impl Default for RentCollector {
    fn default() -> Self {
        Self {
            epoch: Epoch::default(),
            epoch_schedule: EpochSchedule::default(),
            // derive default value using GenesisConfig::default()
            slots_per_year: GenesisConfig::default().slots_per_year(),
            rent: Rent::default(),
        }
    }
}

impl RentCollector {
    pub(crate) fn new(
        epoch: Epoch,
        epoch_schedule: EpochSchedule,
        slots_per_year: f64,
        rent: Rent,
    ) -> Self {
        Self {
            epoch,
            epoch_schedule,
            slots_per_year,
            rent,
        }
    }

    pub(crate) fn clone_with_epoch(&self, epoch: Epoch) -> Self {
        Self {
            epoch,
            ..self.clone()
        }
    }

    #[allow(deprecated)]
    pub(crate) fn deprecate_rent_exemption_threshold(&mut self) {
        let lamports_per_byte = match self.rent.exemption_threshold {
            SIMD0194_EXEMPTION_THRESHOLD => self.rent.lamports_per_byte,
            CURRENT_EXEMPTION_THRESHOLD => self
                .rent
                .lamports_per_byte
                .checked_mul(2)
                .expect("SIMD-0194 rent threshold migration must not overflow"),
            unsupported_threshold => panic!(
                "unsupported rent exemption threshold for SIMD-0194 integer migration: \
                 {unsupported_threshold:?}"
            ),
        };
        self.rent = Rent {
            lamports_per_byte,
            exemption_threshold: SIMD0194_EXEMPTION_THRESHOLD,
            burn_percent: 50,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(deprecated)]
    #[test]
    fn test_deprecate_rent_exemption_threshold_integer_equivalent_cases() {
        let epoch_schedule = EpochSchedule::default();

        let mut rent_collector = RentCollector::new(
            0,
            epoch_schedule.clone(),
            GenesisConfig::default().slots_per_year(),
            Rent {
                lamports_per_byte: 10,
                exemption_threshold: SIMD0194_EXEMPTION_THRESHOLD,
                burn_percent: 25,
            },
        );
        rent_collector.deprecate_rent_exemption_threshold();
        assert_eq!(rent_collector.rent.lamports_per_byte, 10);
        assert_eq!(
            rent_collector.rent.exemption_threshold,
            SIMD0194_EXEMPTION_THRESHOLD
        );
        assert_eq!(rent_collector.rent.burn_percent, 50);

        let mut rent_collector = RentCollector::new(
            0,
            epoch_schedule,
            GenesisConfig::default().slots_per_year(),
            Rent {
                lamports_per_byte: 10,
                exemption_threshold: CURRENT_EXEMPTION_THRESHOLD,
                burn_percent: 25,
            },
        );
        rent_collector.deprecate_rent_exemption_threshold();
        assert_eq!(rent_collector.rent.lamports_per_byte, 20);
        assert_eq!(
            rent_collector.rent.exemption_threshold,
            SIMD0194_EXEMPTION_THRESHOLD
        );
        assert_eq!(rent_collector.rent.burn_percent, 50);
    }

    #[allow(deprecated)]
    #[test]
    #[should_panic(
        expected = "unsupported rent exemption threshold for SIMD-0194 integer migration"
    )]
    fn test_deprecate_rent_exemption_threshold_rejects_custom_threshold() {
        let mut rent_collector = RentCollector::new(
            0,
            EpochSchedule::default(),
            GenesisConfig::default().slots_per_year(),
            Rent {
                lamports_per_byte: 10,
                exemption_threshold: 1.2f64.to_le_bytes(),
                burn_percent: 25,
            },
        );
        rent_collector.deprecate_rent_exemption_threshold();
    }
}
