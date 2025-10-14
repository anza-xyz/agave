use crate::client_error;
pub use solana_rpc_client_types::response::{
    transaction, EncodedTransaction, EncodedTransactionWithStatusMeta, FeeCalculator,
    FeeRateGovernor, OptionSerializer, OptionalContext, ParsedAccount, ParsedInstruction,
    ProcessedSignatureResult, ReceivedSignatureResult, Response, Reward, RewardType, Rewards,
    RpcAccountBalance, RpcApiVersion, RpcBlockCommitment, RpcBlockProduction,
    RpcBlockProductionRange, RpcBlockUpdate, RpcBlockUpdateError, RpcBlockhash,
    RpcBlockhashFeeCalculator, RpcConfirmedTransactionStatusWithSignature, RpcContactInfo,
    RpcFeeCalculator, RpcFeeRateGovernor, RpcIdentity, RpcInflationGovernor, RpcInflationRate,
    RpcInflationReward, RpcKeyedAccount, RpcLeaderSchedule, RpcLogsResponse, RpcPerfSample,
    RpcPrioritizationFee, RpcResponseContext, RpcSignatureConfirmation, RpcSignatureResult,
    RpcSimulateTransactionResult, RpcSnapshotSlotInfo, RpcStorageTurn, RpcSupply,
    RpcTokenAccountBalance, RpcVersionInfo, RpcVote, RpcVoteAccountInfo, RpcVoteAccountStatus,
    SlotInfo, SlotTransactionStats, SlotUpdate, StakeActivationState, TransactionBinaryEncoding,
    TransactionConfirmationStatus, TransactionError, TransactionParsedAccount, TransactionResult,
    UiAccount, UiAccountData, UiAccountEncoding, UiAccountsList, UiCompiledInstruction,
    UiConfirmedBlock, UiInnerInstructions, UiInstruction, UiLoadedAddresses, UiParsedInstruction,
    UiPartiallyDecodedInstruction, UiReturnDataEncoding, UiTokenAmount, UiTransactionError,
    UiTransactionReturnData, UiTransactionStatusMeta, UiTransactionTokenBalance, Value,
};

pub type RpcResult<T> = client_error::Result<Response<T>>;
