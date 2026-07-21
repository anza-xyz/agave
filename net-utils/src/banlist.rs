use std::{
    borrow::Borrow,
    collections::HashMap,
    hash::Hash,
    sync::RwLock,
    time::{Duration, Instant},
};

/// Tracks temporary bans based on e.g. address or pubkey
#[derive(Default)]
pub struct Banlist<T> {
    banned: RwLock<HashMap<T, Instant>>,
}

impl<T: Eq + Hash> Banlist<T> {
    /// Ban the `id` for the specified `timeout`
    ///
    /// Returns `true` if the `id` was already banned else `false`.
    pub fn ban(&self, id: T, timeout: Duration) -> bool {
        debug_assert!(!timeout.is_zero());
        debug_assert!(Instant::now().checked_add(timeout).is_some());
        let Some(expires_at) = Instant::now().checked_add(timeout) else {
            return false;
        };
        let prev = self.banned.write().unwrap().insert(id, expires_at);
        debug_assert!(prev < Some(expires_at));
        prev.is_some()
    }

    /// Check if `id` is banned using the current time.
    pub fn is_banned<Q>(&self, id: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let now = Instant::now();
        let Some(expires_at) = self.banned.read().unwrap().get(id).copied() else {
            return false;
        };

        if expires_at > now {
            return true;
        }

        let mut banned = self.banned.write().unwrap();
        // Check again in case another thread modified  the banlist
        let Some(expires_at) = banned.get(id).copied() else {
            return false;
        };

        if expires_at > now {
            return true;
        }

        banned.remove(id);
        false
    }

    /// Prune the banlist for any expired bans
    pub fn prune(&self) {
        let now = Instant::now();
        let mut banned = self.banned.write().unwrap();

        banned.retain(|_, expires_at| *expires_at > now)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::Banlist,
        std::time::{Duration, Instant},
    };

    #[test]
    fn test_is_banned() {
        let banlist = Banlist::<String>::default();
        banlist.ban("malicious-peer".to_string(), Duration::from_secs(60));

        assert!(banlist.is_banned("malicious-peer"));
        assert!(!banlist.is_banned("benign-peer"));
    }

    #[test]
    fn test_prune_removes_expired_entries() {
        let banlist = Banlist::<String>::default();
        let expired_id = "expired-peer".to_string();
        let active_id = "active-peer".to_string();

        {
            let mut banned = banlist.banned.write().unwrap();
            banned.insert(expired_id.clone(), Instant::now() - Duration::from_secs(1));
            banned.insert(active_id.clone(), Instant::now() + Duration::from_secs(60));
        }

        banlist.prune();

        let banned = banlist.banned.read().unwrap();
        assert!(!banned.contains_key("expired-peer"));
        assert!(banned.contains_key("active-peer"));
        assert_eq!(banned.len(), 1);
    }
}
