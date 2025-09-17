use crate::client_error;
pub use solana_rpc_client_types::response::{
    OptionalContext, ProcessedSignatureResult, ReceivedSignatureResult, Response,
    RpcAccountBalance, RpcApiVersion, RpcBlockCommitment, RpcBlockProduction,
    RpcBlockProductionRange, RpcBlockUpdate, RpcBlockUpdateError, RpcBlockhash,
    RpcBlockhashFeeCalculator, RpcConfirmedTransactionStatusWithSignature, RpcContactInfo,
    RpcFeeCalculator, RpcFeeRateGovernor, RpcIdentity, RpcInflationGovernor, RpcInflationRate,
    RpcInflationReward, RpcKeyedAccount, RpcLeaderSchedule, RpcLogsResponse, RpcPerfSample,
    RpcPrioritizationFee, RpcResponseContext, RpcSignatureConfirmation, RpcSignatureResult,
    RpcSimulateTransactionResult, RpcSnapshotSlotInfo, RpcStorageTurn, RpcSupply,
    RpcTokenAccountBalance, RpcVersionInfo, RpcVote, RpcVoteAccountInfo, RpcVoteAccountStatus,
    SlotInfo, SlotTransactionStats, SlotUpdate, StakeActivationState,
};

pub type RpcResult<T> = client_error::Result<Response<T>>;
