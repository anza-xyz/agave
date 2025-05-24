use {
    log::*,
    quinn::Connection,
    solana_streamer::tpu_feedback::{
        TpuFeedback, TYPE_PRIORITY_FEES, TYPE_TIMESTAMP, TYPE_TRANSACTION_STATE, TYPE_VERSION,
    },
    tokio::sync::broadcast,
    tokio_util::sync::CancellationToken,
};

fn parse_feedback(data: &[u8]) -> Option<TpuFeedback> {
    let mut offset = 0;
    let mut version: Option<u8> = None;
    let mut timestamp: Option<u64> = None;
    let mut transactions: Vec<(u64, u32)> = vec![];
    let mut priority_fees: Option<(u64, u64, u64)> = None;

    while offset + 3 <= data.len() {
        let t = data[offset];
        let l = u16::from_le_bytes([data[offset + 1], data[offset + 2]]) as usize;
        offset += 3;

        if offset + l > data.len() {
            break;
        }

        match t {
            TYPE_VERSION if l == 1 => {
                version = Some(data[offset]);
            }
            TYPE_TIMESTAMP if l == 8 => {
                timestamp = Some(u64::from_le_bytes(
                    data[offset..offset + 8].try_into().ok()?,
                ));
            }
            TYPE_TRANSACTION_STATE => {
                let mut sub_offset = 0;
                while sub_offset + 12 <= l {
                    let tx_sig = u64::from_le_bytes(
                        data[offset + sub_offset..offset + sub_offset + 8]
                            .try_into()
                            .ok()?,
                    );
                    let state = u32::from_le_bytes(
                        data[offset + sub_offset + 8..offset + sub_offset + 12]
                            .try_into()
                            .ok()?,
                    );
                    transactions.push((tx_sig, state));
                    sub_offset += 12;
                }
            }
            TYPE_PRIORITY_FEES if l == 24 => {
                let min = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
                let median = u64::from_le_bytes(data[offset + 8..offset + 16].try_into().ok()?);
                let max = u64::from_le_bytes(data[offset + 16..offset + 24].try_into().ok()?);
                priority_fees = Some((min, median, max));
            }
            _ => {
                debug!("Unknown or malformed TLV type: {} with length {}", t, l);
            }
        }

        offset += l;
    }

    Some(TpuFeedback {
        version: version?,
        timestamp: timestamp?,
        transactions,
        priority_fees: priority_fees?,
    })
}

pub(crate) async fn recv_tpu_feedback(
    connection: Connection,
    cancel: CancellationToken,
    sender: broadcast::Sender<TpuFeedback>,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!("Recv datagrams task cancelled.");
                break;
            }
            result = connection.read_datagram() => {
                match result {
                    Ok(data) => {
                        debug!("Received datagram of size: {}", data.len());
                        let feedback = parse_feedback(&data);
                        if let Some(feedback) = feedback {
                            trace!("Received feedback: {:?}", feedback);
                            match sender.send(feedback) {
                                    Ok(_) => {
                                        debug!("Feedback sent to sender.");
                                    }
                                    Err(e) => {
                                        debug!("Failed to send feedback: {:?}", e);
                                    }
                                }
                        } else {
                            debug!("Failed to parse feedback.");
                        }
                    }
                    Err(e) => {
                        debug!("Error receiving datagram: {e}");
                        break;
                    }
                }
            }
        }
    }
}
