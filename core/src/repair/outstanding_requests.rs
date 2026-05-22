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

/// Outcome of [`OutstandingRequests::register_response`].
#[derive(Debug)]
pub enum RegisterResponseResult<R, U> {
    /// Nonce was valid and `verify_response` accepted the response.
    Success(R),
    /// Nonce was not in the cache (never existed, evicted, or already consumed).
    UnknownNonce,
    /// Nonce existed but the request had passed its expire_timestamp.
    Expired(Option<U>),
    /// Nonce existed and was within ttl, but `verify_response` rejected the
    /// payload. Metadata (if any) is returned so the caller can attribute the
    /// failure to the peer the nonce was sent to.
    InvalidResponse(Option<U>),
}

impl<R, U> RegisterResponseResult<R, U> {
    /// Convert to an `Option<R>`, discarding failure context. Useful for
    /// callers that only care whether the response was accepted.
    pub fn ok(self) -> Option<R> {
        match self {
            Self::Success(r) => Some(r),
            _ => None,
        }
    }
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
    ///
    /// Performs validation on the response, distinguishing four outcomes:
    /// - `Success`: nonce was valid and `verify_response` passed. Decrements the
    ///   # of expected responses; deletes the entry eagerly if exhausted and no
    ///   metadata was attached.
    /// - `UnknownNonce`: nonce never existed, was already evicted, or its
    ///   `num_expected_responses` is already zero.
    /// - `Expired`: nonce existed but its `expire_timestamp` has passed. The
    ///   entry is removed and any attached metadata is returned.
    /// - `InvalidResponse`: nonce was valid but `verify_response` rejected the
    ///   response (e.g. bad Merkle proof, mismatched block id). The entry is
    ///   removed and any attached metadata is returned, so callers can attribute
    ///   the failure back to the peer the nonce was sent to.
    pub fn register_response<R>(
        &mut self,
        nonce: u32,
        response: &S,
        now: u64,
        success_fn: impl Fn(&T) -> R,
    ) -> RegisterResponseResult<R, U> {
        let mut should_delete = false;
        let outcome = match self.requests.get_mut(&nonce) {
            None => RegisterResponseResult::UnknownNonce,
            Some(status) if status.num_expected_responses == 0 => {
                RegisterResponseResult::UnknownNonce
            }
            Some(status) if now >= status.expire_timestamp => {
                should_delete = true;
                RegisterResponseResult::Expired(status.metadata.take())
            }
            Some(status) if !status.request.verify_response(response) => {
                should_delete = true;
                RegisterResponseResult::InvalidResponse(status.metadata.take())
            }
            Some(status) => {
                status.num_expected_responses -= 1;
                let result = success_fn(&status.request);
                if status.num_expected_responses == 0 && status.metadata.is_none() {
                    // No metadata, and no more expected responses safe to delete eagerly.
                    should_delete = true;
                }
                RegisterResponseResult::Success(result)
            }
        };

        if should_delete {
            self.requests
                .pop(&nonce)
                .expect("request must exist when marked for deletion");
        }
        outcome
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

        assert!(matches!(
            outstanding_requests.register_response(
                nonce,
                shred.payload(),
                expire_timestamp + 1,
                |_| ()
            ),
            RegisterResponseResult::Expired(_)
        ));
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
        assert!(matches!(
            outstanding_requests.register_response(
                nonce,
                shred.payload(),
                expire_timestamp - 1,
                |_| ()
            ),
            RegisterResponseResult::Success(_)
        ));
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
        assert!(matches!(
            outstanding_requests.register_response(
                nonce + 1,
                shred.payload(),
                expire_timestamp - 1,
                |_| ()
            ),
            RegisterResponseResult::UnknownNonce
        ));
        assert!(matches!(
            outstanding_requests.register_response(
                nonce + 1,
                shred.payload(),
                expire_timestamp,
                |_| ()
            ),
            RegisterResponseResult::UnknownNonce
        ));
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
        assert!(matches!(
            outstanding_requests.register_response(
                nonce,
                shred.payload(),
                expire_timestamp,
                |_| ()
            ),
            RegisterResponseResult::Expired(_)
        ));
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
            assert!(matches!(
                outstanding_requests.register_response(
                    nonce,
                    shred.payload(),
                    expire_timestamp - 1,
                    |_| ()
                ),
                RegisterResponseResult::Success(_)
            ));
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

        assert!(matches!(
            outstanding_requests.register_response(nonce, &42, now, |_| ()),
            RegisterResponseResult::Success(_)
        ));

        // Duplicate responses should not remove metadata before consumption.
        assert!(matches!(
            outstanding_requests.register_response(nonce, &42, now, |_| ()),
            RegisterResponseResult::UnknownNonce
        ));

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
