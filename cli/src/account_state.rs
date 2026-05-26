#[cfg(test)]
use solana_account::{AccountSharedData, WritableAccount};
use {
    bincode::Error,
    serde::de::DeserializeOwned,
    solana_account::{Account, ReadableAccount},
};

pub(crate) trait StateMut {
    fn state<T: DeserializeOwned>(&self) -> Result<T, Error>;
    #[cfg(test)]
    fn set_state<T: serde::Serialize>(&mut self, state: &T) -> Result<(), Error>;
}

impl StateMut for Account {
    fn state<T: DeserializeOwned>(&self) -> Result<T, Error> {
        bincode::deserialize(self.data())
    }

    #[cfg(test)]
    fn set_state<T: serde::Serialize>(&mut self, state: &T) -> Result<(), Error> {
        serialize_state(self, state)
    }
}

#[cfg(test)]
impl StateMut for AccountSharedData {
    fn state<T: DeserializeOwned>(&self) -> Result<T, Error> {
        bincode::deserialize(self.data())
    }

    fn set_state<T: serde::Serialize>(&mut self, state: &T) -> Result<(), Error> {
        serialize_state(self, state)
    }
}

#[cfg(test)]
fn serialize_state<T: serde::Serialize>(
    account: &mut impl WritableAccount,
    state: &T,
) -> Result<(), Error> {
    if bincode::serialized_size(state)? > account.data().len() as u64 {
        return Err(Box::new(bincode::ErrorKind::SizeLimit));
    }
    bincode::serialize_into(account.data_as_mut_slice(), state)
}
