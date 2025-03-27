use {
    super::EpochAccountsHash,
    solana_clock::Slot,
    std::sync::{
        atomic::{AtomicBool, Ordering},
        Condvar, Mutex,
    },
};

/// Manage the epoch accounts hash
///
/// Handles setting when an EAH calculation is requested and when it completes.  Also handles
/// waiting for in-flight calculations to complete when the "stop" Bank must include it.
#[derive(Debug)]
pub struct Manager {
    /// Current state of the epoch accounts hash
    state: Mutex<State>,
    /// This condition variable is used to wait for an in-flight EAH calculation to complete
    cvar: Condvar,
    /// Raise this flag when the EAH calculation is in-flight.
    /// Others can query this flag instead of having to lock `state`, which will be much cheaper.
    in_flight_flag: AtomicBool,
}

impl Manager {
    #[must_use]
    fn _new(state: State) -> Self {
        let is_in_flight = matches!(&state, State::InFlight(_));
        Self {
            state: Mutex::new(state),
            cvar: Condvar::new(),
            in_flight_flag: AtomicBool::new(is_in_flight),
        }
    }

    /// Create a new epoch accounts hash manager, with the initial state set to Invalid
    #[must_use]
    pub fn new_invalid() -> Self {
        Self::_new(State::Invalid)
    }

    /// Create a new epoch accounts hash manager, with the initial state set to Valid
    #[must_use]
    pub fn new_valid(epoch_accounts_hash: EpochAccountsHash, slot: Slot) -> Self {
        Self::_new(State::Valid(epoch_accounts_hash, slot))
    }

    /// An epoch accounts hash calculation has been requested; update our state
    pub fn set_in_flight(&self, slot: Slot) {
        let mut state = self.state.lock().unwrap();
        if let State::InFlight(old_slot) = &*state {
            panic!("An epoch accounts hash calculation is already in-flight from slot {old_slot}!");
        }
        *state = State::InFlight(slot);
        self.in_flight_flag.store(true, Ordering::Release);
    }

    /// An epoch accounts hash calculation has completed; update our state
    pub fn set_valid(&self, epoch_accounts_hash: EpochAccountsHash, slot: Slot) {
        let mut state = self.state.lock().unwrap();
        if let State::Valid(old_epoch_accounts_hash, old_slot) = &*state {
            panic!(
                "The epoch accounts hash is already valid! \
                \nold slot: {old_slot}, epoch accounts hash: {old_epoch_accounts_hash:?} \
                \nnew slot: {slot}, epoch accounts hash: {epoch_accounts_hash:?}"
            );
        }
        *state = State::Valid(epoch_accounts_hash, slot);
        self.in_flight_flag.store(false, Ordering::Release);
        self.cvar.notify_all();
    }

    /// Get the epoch accounts hash
    ///
    /// If an EAH calculation is in-flight, then this call will block until it completes.
    pub fn wait_get_epoch_accounts_hash(&self) -> EpochAccountsHash {
        let mut state = self.state.lock().unwrap();
        loop {
            match &*state {
                State::Valid(epoch_accounts_hash, _slot) => break *epoch_accounts_hash,
                State::InFlight(_slot) => state = self.cvar.wait(state).unwrap(),
                State::Invalid => panic!("The epoch accounts hash cannot be awaited when Invalid!"),
            }
        }
    }

    /// Get the epoch accounts hash
    ///
    /// This fn does not block, and will only yield an EAH if the state is `Valid`
    pub fn try_get_epoch_accounts_hash(&self) -> Option<EpochAccountsHash> {
        let state = self.state.lock().unwrap();
        match &*state {
            State::Valid(epoch_accounts_hash, _slot) => Some(*epoch_accounts_hash),
            _ => None,
        }
    }

    /// Is an epoch accounts hash calculation in flight?
    pub fn is_in_flight(&self) -> bool {
        self.in_flight_flag.load(Ordering::Acquire)
    }
}

/// The EpochAccountsHash is calculated in the background via AccountsBackgroundService.  This enum
/// is used to track the state of that calculation, and queried when saving the EAH into a Bank or
/// Snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// The initial state of the EAH.  This can occur when loading from a snapshot that does not
    /// include an EAH, or when starting from genesis (before an EAH calculation is requested).
    Invalid,
    /// An EAH calculation has been requested (for `Slot`) and is in flight.  The Bank that should
    /// save the EAH must wait until the calculation has completed.
    InFlight(Slot),
    /// The EAH calculation is complete (for `Slot`) and the EAH value is valid to read/use.
    Valid(EpochAccountsHash, Slot),
}

#[cfg(test)]
mod tests {
    use {super::*, solana_hash::Hash, std::time::Duration};

    #[test]
    fn test_new_valid() {
        let epoch_accounts_hash = EpochAccountsHash::new(Hash::new_unique());
        let manager = Manager::new_valid(epoch_accounts_hash, 5678);
        assert_eq!(
            manager.try_get_epoch_accounts_hash(),
            Some(epoch_accounts_hash),
        );
        assert_eq!(manager.wait_get_epoch_accounts_hash(), epoch_accounts_hash);
    }

    #[test]
    fn test_new_invalid() {
        let manager = Manager::new_invalid();
        assert!(manager.try_get_epoch_accounts_hash().is_none());
    }

    #[test]
    fn test_try_get_epoch_accounts_hash() {
        let epoch_accounts_hash = EpochAccountsHash::new(Hash::new_unique());
        for (state, expected) in [
            (State::Invalid, None),
            (State::InFlight(123), None),
            (
                State::Valid(epoch_accounts_hash, 5678),
                Some(epoch_accounts_hash),
            ),
        ] {
            let manager = Manager::_new(state);
            let actual = manager.try_get_epoch_accounts_hash();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn test_wait_epoch_accounts_hash() {
        // Test: State is Valid, no need to wait
        {
            let epoch_accounts_hash = EpochAccountsHash::new(Hash::new_unique());
            let manager = Manager::new_valid(epoch_accounts_hash, 5678);
            assert_eq!(manager.wait_get_epoch_accounts_hash(), epoch_accounts_hash);
        }

        // Test: State is InFlight, must wait
        {
            let epoch_accounts_hash = EpochAccountsHash::new(Hash::new_unique());
            let manager = Manager::new_invalid();
            manager.set_in_flight(123);

            std::thread::scope(|s| {
                s.spawn(|| {
                    std::thread::sleep(Duration::from_secs(1));
                    manager.set_valid(epoch_accounts_hash, 5678)
                });
                assert!(manager.try_get_epoch_accounts_hash().is_none());
                assert_eq!(manager.wait_get_epoch_accounts_hash(), epoch_accounts_hash);
                assert!(manager.try_get_epoch_accounts_hash().is_some());
            });
        }
    }

    #[test]
    #[should_panic]
    fn test_wait_epoch_accounts_hash_invalid() {
        // Test: State is Invalid, should panic
        let manager = Manager::new_invalid();
        let _epoch_accounts_hash = manager.wait_get_epoch_accounts_hash();
    }

    #[test]
    fn test_is_in_flight() {
        let manager = Manager::new_invalid();
        assert!(!manager.is_in_flight());

        let in_flight_slot = 123;
        manager.set_in_flight(in_flight_slot);
        assert!(manager.is_in_flight());

        manager.set_valid(EpochAccountsHash::new(Hash::new_unique()), 5678);
        assert!(!manager.is_in_flight());
    }
}
