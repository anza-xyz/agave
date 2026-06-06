use {
    crate::repair::request_response::RequestResponse,
    lazy_lru::LruCache,
    rand::{Rng, rng},
    solana_ledger::shred::Nonce,
};

pub const DEFAULT_REQUEST_EXPIRATION_MS: u64 = 60_000;

pub struct OutstandingRequests<T, U = ()> {
    requests: LruCache<Nonce, RequestStatus<T, U>>,
}

impl<T, S: ?Sized, U> OutstandingRequests<T, U>
where
    T: RequestResponse<Response = S>,
{
    /// Add a request to the cache, returns the nonce to be sent with the repair request
    /// and expected on the response.
    pub fn add_request(&mut self, request: T, now: u64) -> Nonce {
        self.add_request_with_metadata(request, now, None)
    }

    /// Similar to `add_request` but additionally specifies an associated metadata
    /// for the nonce that can be fetched with `fetch_metadata_for_nonce`.
    pub fn add_request_with_metadata(
        &mut self,
        request: T,
        now: u64,
        metadata: Option<U>,
    ) -> Nonce {
        let num_expected_responses = request.num_expected_responses();
        let nonce = rng().random_range(0..Nonce::MAX);
        self.requests.put(
            nonce,
            RequestStatus {
                expire_timestamp: now + DEFAULT_REQUEST_EXPIRATION_MS,
                num_expected_responses,
                request,
                metadata,
            },
        );
        nonce
    }

    /// Register a response to the request associated with `nonce`.
    /// See [`Self::register_response_with_failure_handler`] for the full
    /// behavior; this is a thin wrapper that ignores verify failures.
    pub fn register_response<R>(
        &mut self,
        nonce: u32,
        response: &S,
        now: u64,
        success_fn: impl FnOnce(&T) -> R,
    ) -> Option<R> {
        self.register_response_with_failure_handler(nonce, response, now, success_fn, |_| {})
    }

    /// Like [`Self::register_response`] but additionally invokes `failure_fn`
    /// when there was a matching pending request and `verify_response`
    /// returned false. Lets the caller re-queue the request immediately
    /// against a different peer instead of waiting for the timeout retry.
    ///
    /// `failure_fn` is NOT invoked for unknown / already-consumed nonces or
    /// for requests that have hit their `expire_timestamp` — those are
    /// dropped silently as before (the timeout retry path handles
    /// genuinely-stale requests).
    pub fn register_response_with_failure_handler<R>(
        &mut self,
        nonce: u32,
        response: &S,
        now: u64,
        success_fn: impl FnOnce(&T) -> R,
        failure_fn: impl FnOnce(&T),
    ) -> Option<R> {
        let mut should_delete = false;
        let response = self.requests.get_mut(&nonce).and_then(|status| {
            if status.num_expected_responses == 0 {
                // No more expected responses
                return None;
            }

            if now >= status.expire_timestamp {
                // Stale: leave to the caller's timeout retry path, just GC here.
                should_delete = true;
                return None;
            }

            if !status.request.verify_response(response) {
                // Peer responded fast with garbage. Hand the request back so
                // the caller can re-queue against a different peer.
                should_delete = true;
                failure_fn(&status.request);
                return None;
            }

            status.num_expected_responses -= 1;
            if status.num_expected_responses == 0 && status.metadata.is_none() {
                // No metadata, and no more expected responses safe to delete eagerly.
                should_delete = true;
            }
            Some(success_fn(&status.request))
        });

        if should_delete {
            self.requests
                .pop(&nonce)
                .expect("request must exist when marked for deletion");
        }
        response
    }

    /// Fetches metadata associated with the nonce
    pub fn fetch_metadata_for_nonce(&self, nonce: u32) -> Option<U>
    where
        U: Copy,
    {
        let status = self.requests.get(&nonce)?;
        status.metadata
    }
}

impl<T, U> Default for OutstandingRequests<T, U> {
    fn default() -> Self {
        Self {
            requests: LruCache::new(16 * 1024),
        }
    }
}

pub struct RequestStatus<T, U> {
    expire_timestamp: u64,
    num_expected_responses: u32,
    request: T,
    metadata: Option<U>,
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::repair::{request_response::RequestResponse, serve_repair::ShredRepairType},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{blockstore_meta::BlockLocation, shred::Shredder},
        solana_time_utils::timestamp,
    };

    #[derive(Clone, Copy)]
    struct TestRequest {
        expected_response: u8,
        num_expected_responses: u32,
    }

    impl RequestResponse for TestRequest {
        type Response = u8;

        fn num_expected_responses(&self) -> u32 {
            self.num_expected_responses
        }

        fn verify_response(&self, response: &Self::Response) -> bool {
            self.expected_response == *response
        }
    }

    #[test]
    fn test_add_request() {
        let repair_type = ShredRepairType::Orphan(9);
        let mut outstanding_requests = OutstandingRequests::<ShredRepairType>::default();
        let nonce = outstanding_requests.add_request(repair_type, timestamp());
        let request_status = outstanding_requests.requests.get(&nonce).unwrap();
        assert_eq!(request_status.request, repair_type);
        assert_eq!(
            request_status.num_expected_responses,
            repair_type.num_expected_responses()
        );
        assert!(request_status.metadata.is_none());
    }

    #[test]
    fn test_timeout_expired_remove() {
        let repair_type = ShredRepairType::Orphan(9);
        let mut outstanding_requests = OutstandingRequests::<ShredRepairType>::default();
        let nonce = outstanding_requests.add_request(repair_type, timestamp());
        let keypair = Keypair::new();
        let shred = Shredder::single_shred_for_tests(0, &keypair);

        let expire_timestamp = outstanding_requests
            .requests
            .get(&nonce)
            .map(|status| status.expire_timestamp)
            .unwrap();

        assert!(
            outstanding_requests
                .register_response(nonce, shred.payload(), expire_timestamp + 1, |_| ())
                .is_none()
        );
        assert!(outstanding_requests.requests.get(&nonce).is_none());
    }

    #[test]
    fn test_register_response() {
        let repair_type = ShredRepairType::Orphan(9);
        let mut outstanding_requests = OutstandingRequests::<ShredRepairType>::default();
        let nonce = outstanding_requests.add_request(repair_type, timestamp());
        let keypair = Keypair::new();
        let shred = Shredder::single_shred_for_tests(0, &keypair);
        let mut expire_timestamp = outstanding_requests
            .requests
            .get(&nonce)
            .map(|status| status.expire_timestamp)
            .unwrap();
        let mut num_expected_responses = outstanding_requests
            .requests
            .get(&nonce)
            .map(|status| status.num_expected_responses)
            .unwrap();
        assert!(num_expected_responses > 1);

        // Response that passes all checks should decrease num_expected_responses.
        assert!(
            outstanding_requests
                .register_response(nonce, shred.payload(), expire_timestamp - 1, |_| ())
                .is_some()
        );
        num_expected_responses -= 1;
        assert_eq!(
            outstanding_requests
                .requests
                .get(&nonce)
                .unwrap()
                .num_expected_responses,
            num_expected_responses
        );

        // Response with incorrect nonce is ignored.
        assert!(
            outstanding_requests
                .register_response(nonce + 1, shred.payload(), expire_timestamp - 1, |_| ())
                .is_none()
        );
        assert!(
            outstanding_requests
                .register_response(nonce + 1, shred.payload(), expire_timestamp, |_| ())
                .is_none()
        );
        assert_eq!(
            outstanding_requests
                .requests
                .get(&nonce)
                .unwrap()
                .num_expected_responses,
            num_expected_responses
        );

        // Response with timestamp over limit should remove status, preventing late
        // responses from being accepted.
        assert!(
            outstanding_requests
                .register_response(nonce, shred.payload(), expire_timestamp, |_| ())
                .is_none()
        );
        assert!(outstanding_requests.requests.get(&nonce).is_none());

        // If number of outstanding requests hits zero and there is no completion
        // data, remove the entry.
        let nonce = outstanding_requests.add_request(repair_type, timestamp());
        expire_timestamp = outstanding_requests
            .requests
            .get(&nonce)
            .map(|status| status.expire_timestamp)
            .unwrap();
        num_expected_responses = outstanding_requests
            .requests
            .get(&nonce)
            .map(|status| status.num_expected_responses)
            .unwrap();
        assert!(num_expected_responses > 1);
        for _ in 0..num_expected_responses {
            assert!(outstanding_requests.requests.get(&nonce).is_some());
            assert!(
                outstanding_requests
                    .register_response(nonce, shred.payload(), expire_timestamp - 1, |_| ())
                    .is_some()
            );
        }
        assert!(outstanding_requests.requests.get(&nonce).is_none());
    }

    #[test]
    fn test_fetch_metadata_for_registered_response_single_response_request() {
        let mut outstanding_requests = OutstandingRequests::<TestRequest, BlockLocation>::default();
        let now = timestamp();
        let request = TestRequest {
            expected_response: 42,
            num_expected_responses: 1,
        };
        let block_id = Hash::new_unique();
        let nonce = outstanding_requests.add_request_with_metadata(
            request,
            now,
            Some(BlockLocation::Alternate { block_id }),
        );

        assert!(
            outstanding_requests
                .register_response(nonce, &42, now, |_| ())
                .is_some()
        );

        // Duplicate responses should not remove metadata before consumption.
        assert!(
            outstanding_requests
                .register_response(nonce, &42, now, |_| ())
                .is_none()
        );

        assert_eq!(
            outstanding_requests.fetch_metadata_for_nonce(nonce),
            Some(BlockLocation::Alternate { block_id })
        );
        // Entry is lazily cleaned up by LRU, metadata still exists
        assert!(outstanding_requests.requests.get(&nonce).is_some());
        assert_eq!(
            outstanding_requests.fetch_metadata_for_nonce(nonce),
            Some(BlockLocation::Alternate { block_id })
        );
    }
}
