use {
    solana_clock::Epoch, solana_epoch_schedule::EpochSchedule,
    solana_genesis_config::GenesisConfig, solana_rent::Rent,
};

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

    pub(crate) fn deprecate_rent_exemption_threshold(&mut self) {
        // If not free rent (tests)
        if self.rent.lamports_per_byte != 0 {
            // also resets exemption_threshold to 1.0 and lamports_per_byte
            self.rent = Rent::with_lamports_per_byte(solana_rent::DEFAULT_LAMPORTS_PER_BYTE);
        }
    }
}
