#![allow(dead_code)]

use reqwest::Client;
use serde::Deserialize;
use std::error::Error;

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<AccountInfoResponse>,
    id: u64,
}

#[derive(Deserialize)]
struct AccountInfoResponse {
    context: Context,
    value: Option<AccountValue>,
}

#[derive(Deserialize)]
struct Context {
    slot: u64,
}

#[derive(Deserialize)]
struct AccountValue {
    data: AccountData,
    executable: bool,
    lamports: u64,
    owner: String,
    #[serde(rename = "rentEpoch")]
    rent_epoch: u64,
    space: u64,
}

#[derive(Deserialize)]
struct AccountData {
    parsed: ParsedData,
    program: String,
    space: u64,
}

#[derive(Deserialize)]
struct ParsedData {
    info: ProgramInfo,
    #[serde(rename = "type")]
    data_type: String,
}

#[derive(Deserialize)]
struct ProgramInfo {
    #[serde(rename = "programData")]
    program_data: Option<String>,
    data: Option<Vec<String>>,
    slot: Option<u64>,
}

async fn get_account_info(client: &Client, rpc_url: &str, address: &str) -> anyhow::Result<AccountValue> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [
            address,
            {
                "encoding": "jsonParsed"
            }
        ]
    });


    let response = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await?;

    let response: RpcResponse = response.json().await?;
    if let Some(value) = response.result {
        return value.value.ok_or_else(|| anyhow::anyhow!("No value found in account info response"));
    }
    anyhow::bail!("No result found in account info response");
}

pub async fn get_last_deployed_slot(rpc_url: &str, program_address: &str) -> Result<u64, Box<dyn Error>> {
    let client = Client::new();

    // Step 1: Get account info for the program address
    let account_info = get_account_info(&client, rpc_url, program_address).await?;
    let program_data_address = account_info
        .data
        .parsed
        .info
        .program_data
        .ok_or("No programData found in program account response")?;

    // Step 2: Get account info for the program data address
    let program_data_info = get_account_info(&client, rpc_url, &program_data_address).await?;
    let last_deployed_slot = program_data_info
        .data
        .parsed
        .info
        .slot
        .ok_or("No slot found in program data account response")?;

    Ok(last_deployed_slot)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_last_deployed_slot() {
        let rpc_url = "https://docs-demo.solana-mainnet.quiknode.pro";
        let program_address = "verifycLy8mB96wd9wqq3WDXQwM4oU6r42Th37Db9fC";
        let last_deployed_slot = get_last_deployed_slot(rpc_url, program_address).await;
        assert!(last_deployed_slot.is_ok());
    }
}