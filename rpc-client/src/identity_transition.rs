//! Types returned by the observational `identityTransitionStatus` RPC.
//!
//! Reuse the server's wire contract; the runtime tracker is not part of this API.
pub use agave_votor_messages::identity_transition::{
    IdentityTransitionConsensus, IdentityTransitionState, IdentityTransitionStatus,
};

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            nonblocking,
            rpc_client::{RpcClient, RpcClientConfig},
            rpc_sender::{RpcSender, RpcTransportStats},
        },
        serde_json::{Value, json},
        solana_rpc_client_api::{
            client_error::{Error, ErrorKind, Result},
            request::{RpcError, RpcRequest, RpcResponseErrorData},
        },
    };

    struct Sender(Option<Value>);

    #[async_trait::async_trait]
    impl RpcSender for Sender {
        async fn send(&self, request: RpcRequest, params: Value) -> Result<Value> {
            assert_eq!(request, RpcRequest::IdentityTransitionStatus);
            assert!(params.is_null());
            self.0.clone().ok_or_else(|| {
                Error::from(RpcError::RpcResponseError {
                    code: -32601,
                    message: "Method not found".to_string(),
                    data: RpcResponseErrorData::Empty,
                })
            })
        }
        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }
        fn url(&self) -> String {
            "mock://identity-transition".to_string()
        }
    }

    fn response(state: &str, consensus: &str, slot: Option<u64>) -> Value {
        json!({
            "version": 1, "processInstanceId": "instance", "sequence": u64::MAX,
            "state": state, "consensus": consensus, "currentIdentity": "to",
            "fromIdentity": "from", "toIdentity": "to", "voteAccount": "",
            "fromIdentityLastSubmittedVoteSlot": slot, "towerRootSlot": slot,
            "error": if state == "failed" { Some("diagnostic") } else { None }
        })
    }

    #[tokio::test]
    async fn test_identity_transition_status_async() {
        for state in ["idle", "transitioning", "complete", "failed"] {
            for consensus in ["unknown", "tower", "alpenglow"] {
                for slot in [None, Some(0), Some(u64::MAX)] {
                    let raw = response(state, consensus, slot);
                    let expected: IdentityTransitionStatus =
                        serde_json::from_value(raw.clone()).unwrap();
                    let client = nonblocking::rpc_client::RpcClient::new_sender(
                        Sender(Some(raw)),
                        RpcClientConfig::default(),
                    );
                    let actual = client.get_identity_transition_status().await.unwrap();
                    assert_eq!(actual, expected);
                    assert_eq!(actual.from_identity_last_submitted_vote_slot, slot);
                }
            }
        }
    }

    #[test]
    fn test_identity_transition_status_blocking() {
        for slot in [None, Some(0), Some(u64::MAX)] {
            let raw = response("complete", "tower", slot);
            let expected: IdentityTransitionStatus = serde_json::from_value(raw.clone()).unwrap();
            let client = RpcClient::new_sender(Sender(Some(raw)), RpcClientConfig::default());
            assert_eq!(client.get_identity_transition_status().unwrap(), expected);
        }
        let client = RpcClient::new_mock("succeeds".to_string());
        assert_eq!(
            client.get_identity_transition_status().unwrap().state,
            IdentityTransitionState::Idle
        );
    }

    fn assert_method_not_found(error: Error) {
        assert_eq!(error.request(), Some(&RpcRequest::IdentityTransitionStatus));
        assert!(
            matches!(error.kind(), ErrorKind::RpcError(RpcError::RpcResponseError { code: -32601, message, .. }) if message == "Method not found")
        );
    }

    #[tokio::test]
    async fn test_identity_transition_status_async_errors() {
        let client = nonblocking::rpc_client::RpcClient::new_sender(
            Sender(None),
            RpcClientConfig::default(),
        );
        assert_method_not_found(client.get_identity_transition_status().await.unwrap_err());
        let client = nonblocking::rpc_client::RpcClient::new_sender(
            Sender(Some(json!({"state":"complete"}))),
            RpcClientConfig::default(),
        );
        let error = client.get_identity_transition_status().await.unwrap_err();
        assert_eq!(error.request(), Some(&RpcRequest::IdentityTransitionStatus));
        assert!(matches!(error.kind(), ErrorKind::SerdeJson(_)));
    }

    #[test]
    fn test_identity_transition_status_blocking_errors() {
        let client = RpcClient::new_sender(Sender(None), RpcClientConfig::default());
        assert_method_not_found(client.get_identity_transition_status().unwrap_err());
        let client = RpcClient::new_sender(
            Sender(Some(json!({"state":"complete"}))),
            RpcClientConfig::default(),
        );
        assert!(matches!(
            client.get_identity_transition_status().unwrap_err().kind(),
            ErrorKind::SerdeJson(_)
        ));
    }
}
