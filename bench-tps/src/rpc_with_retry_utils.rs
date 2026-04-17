use {
    log::*,
    solana_clock::Slot,
    solana_commitment_config::CommitmentConfig,
    solana_tps_client::{TpsClient, TpsClientResult},
    std::{sync::Arc, thread::sleep, time::Duration},
};

const NUM_RETRY: u64 = 5;

fn call_rpc_with_retry<Func, Data>(
    f: Func,
    retry_warning: &str,
    retry_every: Duration,
) -> TpsClientResult<Data>
where
    Func: Fn() -> TpsClientResult<Data>,
{
    let mut iretry = 0;
    loop {
        match f() {
            Ok(slot) => {
                return Ok(slot);
            }
            Err(error) => {
                if iretry == NUM_RETRY {
                    return Err(error);
                }
                warn!("{retry_warning}: {error}, retry.");
                sleep(retry_every);
            }
        }
        iretry += 1;
    }
}

pub(crate) fn get_slot_with_retry<Client>(
    client: &Arc<Client>,
    commitment: CommitmentConfig,
) -> TpsClientResult<Slot>
where
    Client: 'static + TpsClient + Send + Sync + ?Sized,
{
    let retry_every = client
        .get_bank_timing_config()
        .unwrap_or_default()
        .duration_for_slots(4);
    call_rpc_with_retry(
        || client.get_slot_with_commitment(commitment),
        "Failed to get slot",
        retry_every,
    )
}

pub(crate) fn get_blocks_with_retry<Client>(
    client: &Arc<Client>,
    start_slot: Slot,
    end_slot: Option<Slot>,
    commitment: CommitmentConfig,
) -> TpsClientResult<Vec<Slot>>
where
    Client: 'static + TpsClient + Send + Sync + ?Sized,
{
    let retry_every = client
        .get_bank_timing_config()
        .unwrap_or_default()
        .duration_for_slots(4);
    call_rpc_with_retry(
        || client.get_blocks_with_commitment(start_slot, end_slot, commitment),
        "Failed to download blocks",
        retry_every,
    )
}
