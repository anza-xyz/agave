//! Error message constants for RPC client errors.
//!
//! This module contains predefined error messages that provide detailed information
//! about various error conditions that may occur during RPC operations. Each message
//! includes possible reasons for the error and suggested actions for resolution.

/// Error message for failed transaction confirmation attempts.
/// Includes common reasons for failure and suggested verification steps.
pub const TRANSACTION_CONFIRMATION_FAILED: &str = "\
Unable to confirm transaction. Possible reasons:
1. Transaction expired due to blockhash expiration
2. Insufficient fee-payer funds
3. Network congestion or node issues
Please check transaction status manually and verify account balances.";

/// Error message for cases when a requested block cannot be found.
/// Includes possible reasons why the block might not be accessible.
pub const BLOCK_NOT_FOUND: &str = "\
Block not found. Possible reasons:
1. Block has been pruned
2. Block is not yet finalized
3. Invalid slot number
Please verify the slot number and try again.";

/// Error message for cases when an account cannot be found.
/// Includes common reasons for missing accounts and configuration checks.
pub const ACCOUNT_NOT_FOUND: &str = "\
Account not found. Possible reasons:
1. Account does not exist
2. Account has zero lamports
3. Wrong network or RPC endpoint
Please verify the account address and network configuration.";

/// Error message for unhealthy node conditions.
/// Includes possible reasons for node health issues and suggested actions.
pub const NODE_UNHEALTHY: &str = "\
Node reported as unhealthy. Possible reasons:
1. Node is behind in processing blocks
2. Node is experiencing technical issues
3. High network load
Consider using a different RPC endpoint.";

/// Error message for rate limiting conditions.
/// Includes suggestions for handling rate limits and optimizing requests.
pub const TOO_MANY_REQUESTS: &str = "\
Too many requests received. Possible solutions:
1. Reduce request frequency
2. Use a different RPC endpoint
3. Implement request batching
Please adjust your request pattern and try again.";

#[cfg(test)]
mod tests {
    use super::*;
    use solana_rpc_client_api::{
        client_error::ErrorKind,
        request::RpcError,
    };

    #[test]
    fn test_error_messages_not_empty() {
        assert!(!TRANSACTION_CONFIRMATION_FAILED.is_empty());
        assert!(!BLOCK_NOT_FOUND.is_empty());
        assert!(!ACCOUNT_NOT_FOUND.is_empty());
        assert!(!NODE_UNHEALTHY.is_empty());
        assert!(!TOO_MANY_REQUESTS.is_empty());
    }

    #[test]
    fn test_error_messages_contain_reasons() {
        assert!(TRANSACTION_CONFIRMATION_FAILED.contains("Possible reasons"));
        assert!(BLOCK_NOT_FOUND.contains("Possible reasons"));
        assert!(ACCOUNT_NOT_FOUND.contains("Possible reasons"));
        assert!(NODE_UNHEALTHY.contains("Possible reasons"));
        assert!(TOO_MANY_REQUESTS.contains("Possible solutions"));
    }

    #[test]
    fn test_error_messages_contain_actions() {
        assert!(TRANSACTION_CONFIRMATION_FAILED.contains("Please check"));
        assert!(BLOCK_NOT_FOUND.contains("Please verify"));
        assert!(ACCOUNT_NOT_FOUND.contains("Please verify"));
        assert!(NODE_UNHEALTHY.contains("Consider using"));
        assert!(TOO_MANY_REQUESTS.contains("Please adjust"));
    }

    #[test]
    fn test_error_messages_integration() {
        // Test transaction confirmation error
        let err = RpcError::ForUser(TRANSACTION_CONFIRMATION_FAILED.to_string());
        assert!(err.to_string().contains("Transaction expired"));
        assert!(err.to_string().contains("Insufficient fee-payer funds"));
        
        // Test block not found error
        let err = RpcError::ForUser(BLOCK_NOT_FOUND.to_string());
        assert!(err.to_string().contains("Block has been pruned"));
        assert!(err.to_string().contains("Block is not yet finalized"));
        
        // Test account not found error
        let err = RpcError::ForUser(ACCOUNT_NOT_FOUND.to_string());
        assert!(err.to_string().contains("Account does not exist"));
        assert!(err.to_string().contains("Wrong network"));
    }

    #[test]
    fn test_error_messages_formatting() {
        for msg in [
            TRANSACTION_CONFIRMATION_FAILED,
            BLOCK_NOT_FOUND,
            ACCOUNT_NOT_FOUND,
            NODE_UNHEALTHY,
            TOO_MANY_REQUESTS,
        ] {
            // Check message format
            assert!(msg.contains("Possible reasons") || msg.contains("Possible solutions"));
            assert!(msg.split('\n').count() >= 4, "Message should have multiple lines");
            assert!(msg.chars().all(|c| c.is_ascii()), "Message should be ASCII only");
        }
    }
} 
