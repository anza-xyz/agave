use {
    solana_commitment_config::CommitmentConfig,
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    std::{
        env, fs,
        io::{Read, Write},
        net::TcpStream,
        path::{Path, PathBuf},
        str::FromStr,
        time::{Duration, Instant},
    },
};

const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const RPC_URL_ENV: &str = "SOLANA_DEFI_INVENTORY_RPC_URL";
const DEFILLAMA_HOST: &str = "api.llama.fi";
const DEFILLAMA_PATH: &str = "/protocols";
const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnB6GZLzyA5W";

#[derive(Debug)]
struct Config {
    rpc_url: String,
    protocols_path: Option<PathBuf>,
    output_dir: PathBuf,
    top_limit: usize,
    max_program_accounts: usize,
    no_defillama: bool,
    no_rpc: bool,
    timeout_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rpc_url: env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
            protocols_path: None,
            output_dir: PathBuf::from("solana-defi-inventory-out"),
            top_limit: 25,
            max_program_accounts: 500,
            no_defillama: false,
            no_rpc: false,
            timeout_seconds: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtocolInput {
    protocol: String,
    program_id: Option<Pubkey>,
    pda_or_vault_authority: Option<Pubkey>,
    notes: String,
}

#[derive(Debug, Clone)]
struct TopProtocol {
    rank: usize,
    name: String,
    slug: String,
    category: String,
    tvl_usd: f64,
    url: String,
}

#[derive(Debug, Clone)]
struct InventoryRecord {
    protocol: String,
    record_type: String,
    address: String,
    owner_or_authority: String,
    lamports: u64,
    mint: String,
    amount: String,
    decimals: String,
    token_program: String,
    notes: String,
}

fn main() {
    let config = match Config::parse(env::args().skip(1)) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("error: {err}\n");
            print_help();
            std::process::exit(2);
        }
    };

    if let Err(err) = run(&config) {
        eprintln!("error: {}", sanitize_error(&config.rpc_url, &err));
        std::process::exit(1);
    }
}

fn run(config: &Config) -> Result<(), String> {
    validate_url(&config.rpc_url)?;
    fs::create_dir_all(&config.output_dir)
        .map_err(|err| format!("failed to create {}: {err}", config.output_dir.display()))?;

    println!("Solana DeFi read-only inventory/audit target finder");
    println!("Output dir: {}", config.output_dir.display());
    println!("RPC URL: {}", sanitize_url(&config.rpc_url));
    println!("Read-only safety: no keypairs, no signing, no transactions, no TPU sockets");

    let mut top_protocols = Vec::new();
    if !config.no_defillama {
        let started = Instant::now();
        top_protocols = fetch_top_solana_protocols(config.top_limit, config.timeout_seconds)?;
        println!(
            "Fetched {} top Solana protocols from DefiLlama in {:?}",
            top_protocols.len(),
            started.elapsed()
        );
        write_top_protocols_json(
            &config.output_dir.join("top_solana_protocols.json"),
            &top_protocols,
        )?;
        write_top_protocols_csv(
            &config.output_dir.join("top_solana_protocols.csv"),
            &top_protocols,
        )?;
    }

    let protocol_inputs = match &config.protocols_path {
        Some(path) => read_protocol_inputs(path)?,
        None => Vec::new(),
    };
    if let Some(path) = &config.protocols_path {
        println!(
            "Loaded {} protocol/program/PDA rows from {}",
            protocol_inputs.len(),
            path.display()
        );
    }

    let mut inventory = Vec::new();
    if !config.no_rpc && !protocol_inputs.is_empty() {
        let client =
            RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());
        for input in &protocol_inputs {
            if let Some(program_id) = input.program_id {
                match query_program_owned_accounts(
                    &client,
                    input,
                    program_id,
                    config.max_program_accounts,
                ) {
                    Ok(records) => inventory.extend(records),
                    Err(err) => inventory.push(rpc_error_record(
                        input,
                        "program_accounts_rpc_error",
                        &program_id.to_string(),
                        &config.rpc_url,
                        &err,
                    )),
                }
            }
            if let Some(authority) = input.pda_or_vault_authority {
                match query_token_accounts_for_authority(
                    &config.rpc_url,
                    input,
                    authority,
                    SPL_TOKEN_PROGRAM,
                    config.timeout_seconds,
                ) {
                    Ok(records) => inventory.extend(records),
                    Err(err) => inventory.push(rpc_error_record(
                        input,
                        "spl_token_accounts_rpc_error",
                        &authority.to_string(),
                        &config.rpc_url,
                        &err,
                    )),
                }
                match query_token_accounts_for_authority(
                    &config.rpc_url,
                    input,
                    authority,
                    TOKEN_2022_PROGRAM,
                    config.timeout_seconds,
                ) {
                    Ok(records) => inventory.extend(records),
                    Err(err) => inventory.push(rpc_error_record(
                        input,
                        "token_2022_accounts_rpc_error",
                        &authority.to_string(),
                        &config.rpc_url,
                        &err,
                    )),
                }
            }
        }
    } else if config.no_rpc {
        println!("RPC inventory disabled by --no-rpc");
    } else {
        println!("No protocol CSV supplied; skipping RPC inventory");
    }

    write_inventory_json(&config.output_dir.join("asset_inventory.json"), &inventory)?;
    write_inventory_csv(&config.output_dir.join("asset_inventory.csv"), &inventory)?;
    write_audit_checklists(
        &config.output_dir.join("audit_checklists.md"),
        &top_protocols,
        &protocol_inputs,
    )?;

    println!("Inventory records: {}", inventory.len());
    println!("Wrote outputs:");
    println!(
        "  {}",
        config
            .output_dir
            .join("top_solana_protocols.json")
            .display()
    );
    println!(
        "  {}",
        config.output_dir.join("top_solana_protocols.csv").display()
    );
    println!(
        "  {}",
        config.output_dir.join("asset_inventory.json").display()
    );
    println!(
        "  {}",
        config.output_dir.join("asset_inventory.csv").display()
    );
    println!(
        "  {}",
        config.output_dir.join("audit_checklists.md").display()
    );
    Ok(())
}

impl Config {
    fn parse<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut config = Config::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--rpc-url" => {
                    config.rpc_url = args
                        .next()
                        .ok_or_else(|| "--rpc-url requires a value".to_string())?;
                }
                "--protocols" => {
                    config.protocols_path = Some(PathBuf::from(
                        args.next()
                            .ok_or_else(|| "--protocols requires a path".to_string())?,
                    ));
                }
                "--output-dir" => {
                    config.output_dir = PathBuf::from(
                        args.next()
                            .ok_or_else(|| "--output-dir requires a path".to_string())?,
                    );
                }
                "--top-limit" => {
                    config.top_limit = parse_usize("--top-limit", args.next())?;
                }
                "--max-program-accounts" => {
                    config.max_program_accounts =
                        parse_usize("--max-program-accounts", args.next())?;
                }
                "--timeout-seconds" => {
                    config.timeout_seconds = parse_u64("--timeout-seconds", args.next())?;
                }
                "--no-defillama" => config.no_defillama = true,
                "--no-rpc" => config.no_rpc = true,
                unknown => return Err(format!("unknown argument `{unknown}`")),
            }
        }
        if config.top_limit == 0 {
            return Err("--top-limit must be greater than zero".to_string());
        }
        if config.max_program_accounts == 0 {
            return Err("--max-program-accounts must be greater than zero".to_string());
        }
        if config.timeout_seconds == 0 {
            return Err("--timeout-seconds must be greater than zero".to_string());
        }
        validate_url(&config.rpc_url)?;
        Ok(config)
    }
}

fn print_help() {
    println!(
        "solana-defi-inventory\n\
\n\
USAGE:\n\
  cargo run --release --manifest-path dev-bins/solana-defi-inventory/Cargo.toml -- [OPTIONS]\n\
\n\
OPTIONS:\n\
      --rpc-url <URL>              Solana RPC URL [env: SOLANA_DEFI_INVENTORY_RPC_URL; default: mainnet-beta]\n\
      --protocols <CSV>            CSV with protocol,program_id,pda_or_vault_authority,notes\n\
      --output-dir <DIR>           Output directory [default: solana-defi-inventory-out]\n\
      --top-limit <N>              Number of DefiLlama Solana protocols to save [default: 25]\n\
      --max-program-accounts <N>   Cap saved getProgramAccounts rows per program [default: 500]\n\
      --timeout-seconds <N>        HTTP timeout for DefiLlama/token-account RPC calls [default: 30]\n\
      --no-defillama               Skip DefiLlama fetch\n\
      --no-rpc                     Skip all Solana RPC inventory calls\n\
  -h, --help                       Show this help\n\
\n\
SAFETY:\n\
  Read-only only: no keypairs, no signing, no transactions, no TPU sockets.\n"
    );
}

fn parse_usize(flag: &str, value: Option<String>) -> Result<usize, String> {
    value
        .ok_or_else(|| format!("{flag} requires a value"))?
        .parse::<usize>()
        .map_err(|err| format!("invalid {flag}: {err}"))
}

fn parse_u64(flag: &str, value: Option<String>) -> Result<u64, String> {
    value
        .ok_or_else(|| format!("{flag} requires a value"))?
        .parse::<u64>()
        .map_err(|err| format!("invalid {flag}: {err}"))
}

fn validate_url(url: &str) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("RPC URL must start with http:// or https://".to_string());
    }
    if url.contains(char::is_whitespace) {
        return Err("RPC URL must not contain whitespace".to_string());
    }
    Ok(())
}

fn fetch_top_solana_protocols(
    limit: usize,
    timeout_seconds: u64,
) -> Result<Vec<TopProtocol>, String> {
    let response = https_get(DEFILLAMA_HOST, DEFILLAMA_PATH, timeout_seconds)?;
    let body = http_body(&response)?;
    let objects = split_json_objects_from_array(body)?;
    let mut protocols = Vec::new();
    for object in objects {
        if !json_array_field_contains(object, "chains", "Solana") {
            continue;
        }
        let category = json_string_field(object, "category").unwrap_or_default();
        if category == "CEX" {
            continue;
        }
        protocols.push(TopProtocol {
            rank: 0,
            name: json_string_field(object, "name").unwrap_or_else(|| "unknown".to_string()),
            slug: json_string_field(object, "slug").unwrap_or_default(),
            category,
            tvl_usd: json_number_field(object, "tvl").unwrap_or(0.0),
            url: json_string_field(object, "url").unwrap_or_default(),
        });
    }
    protocols.sort_by(|a, b| b.tvl_usd.total_cmp(&a.tvl_usd));
    protocols.truncate(limit);
    for (index, protocol) in protocols.iter_mut().enumerate() {
        protocol.rank = index + 1;
    }
    Ok(protocols)
}

fn https_get(host: &str, path: &str, timeout_seconds: u64) -> Result<String, String> {
    let mut stream = TcpStream::connect((host, 80))
        .map_err(|err| format!("failed to connect to {host}:80: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(timeout_seconds)))
        .map_err(|err| format!("failed to set read timeout: {err}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(timeout_seconds)))
        .map_err(|err| format!("failed to set write timeout: {err}"))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: solana-defi-inventory/0.1\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("failed to write HTTP request: {err}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|err| format!("failed to read HTTP response: {err}"))?;
    if response.starts_with("HTTP/1.1 301") || response.starts_with("HTTP/1.1 302") {
        return https_get_via_python(host, path, timeout_seconds);
    }
    Ok(response)
}

fn https_get_via_python(host: &str, path: &str, timeout_seconds: u64) -> Result<String, String> {
    let url = format!("https://{host}{path}");
    let script = format!(
        "import urllib.request; print(urllib.request.urlopen({url:?}, timeout={timeout_seconds}).read().decode())"
    );
    let output = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .output()
        .map_err(|err| format!("failed to run python3 fallback for DefiLlama fetch: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "python3 DefiLlama fetch failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let body = String::from_utf8(output.stdout)
        .map_err(|err| format!("python3 DefiLlama response was not UTF-8: {err}"))?;
    Ok(format!("HTTP/1.1 200 OK\r\n\r\n{body}"))
}

fn http_body(response: &str) -> Result<&str, String> {
    if !(response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200")) {
        let first_line = response.lines().next().unwrap_or("<empty response>");
        return Err(format!("HTTP request failed: {first_line}"));
    }
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or_else(|| "HTTP response missing header/body separator".to_string())
}

fn read_protocol_inputs(path: &Path) -> Result<Vec<ProtocolInput>, String> {
    let content = fs::read_to_string(path)
        .map_err(|err| format!("failed to read protocol CSV {}: {err}", path.display()))?;
    let mut rows = Vec::new();
    for (line_no, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line_no == 0 && line.to_ascii_lowercase().starts_with("protocol,") {
            continue;
        }
        let fields = parse_csv_line(line);
        if fields.len() < 4 {
            return Err(format!(
                "{}:{} expected 4 columns: protocol,program_id,pda_or_vault_authority,notes",
                path.display(),
                line_no + 1
            ));
        }
        let protocol = fields[0].trim().to_string();
        if protocol.is_empty() {
            return Err(format!(
                "{}:{} protocol is required",
                path.display(),
                line_no + 1
            ));
        }
        let program_id = parse_optional_pubkey(&fields[1], "program_id", path, line_no + 1)?;
        let pda_or_vault_authority =
            parse_optional_pubkey(&fields[2], "pda_or_vault_authority", path, line_no + 1)?;
        rows.push(ProtocolInput {
            protocol,
            program_id,
            pda_or_vault_authority,
            notes: fields[3].trim().to_string(),
        });
    }
    Ok(rows)
}

fn parse_optional_pubkey(
    value: &str,
    field_name: &str,
    path: &Path,
    line_no: usize,
) -> Result<Option<Pubkey>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Pubkey::from_str(trimmed).map(Some).map_err(|err| {
        format!(
            "{}:{line_no} invalid {field_name} `{trimmed}`: {err}",
            path.display()
        )
    })
}

fn rpc_error_record(
    input: &ProtocolInput,
    record_type: &str,
    address: &str,
    rpc_url: &str,
    err: &str,
) -> InventoryRecord {
    InventoryRecord {
        protocol: input.protocol.clone(),
        record_type: record_type.to_string(),
        address: address.to_string(),
        owner_or_authority: String::new(),
        lamports: 0,
        mint: String::new(),
        amount: String::new(),
        decimals: String::new(),
        token_program: String::new(),
        notes: sanitize_error(rpc_url, err),
    }
}

fn query_program_owned_accounts(
    client: &RpcClient,
    input: &ProtocolInput,
    program_id: Pubkey,
    max_program_accounts: usize,
) -> Result<Vec<InventoryRecord>, String> {
    let accounts = client
        .get_program_accounts(&program_id)
        .map_err(|err| format!("getProgramAccounts({program_id}) failed: {err}"))?;
    let mut records = Vec::new();
    for (index, (pubkey, account)) in accounts.into_iter().enumerate() {
        if index >= max_program_accounts {
            records.push(InventoryRecord {
                protocol: input.protocol.clone(),
                record_type: "program_account_truncated".to_string(),
                address: program_id.to_string(),
                owner_or_authority: program_id.to_string(),
                lamports: 0,
                mint: String::new(),
                amount: String::new(),
                decimals: String::new(),
                token_program: String::new(),
                notes: format!(
                    "more than {max_program_accounts} accounts; increase --max-program-accounts"
                ),
            });
            break;
        }
        records.push(InventoryRecord {
            protocol: input.protocol.clone(),
            record_type: "program_owned_account".to_string(),
            address: pubkey.to_string(),
            owner_or_authority: account.owner.to_string(),
            lamports: account.lamports,
            mint: String::new(),
            amount: String::new(),
            decimals: String::new(),
            token_program: String::new(),
            notes: format!(
                "data_len={}; executable={}",
                account.data.len(),
                account.executable
            ),
        });
    }
    Ok(records)
}

fn query_token_accounts_for_authority(
    rpc_url: &str,
    input: &ProtocolInput,
    authority: Pubkey,
    token_program: &str,
    timeout_seconds: u64,
) -> Result<Vec<InventoryRecord>, String> {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"getTokenAccountsByOwner","params":["{authority}",{{"programId":"{token_program}"}},{{"encoding":"jsonParsed"}}]}}"#
    );
    let response = post_json(rpc_url, &body, timeout_seconds)?;
    if response.contains("\"error\"") {
        return Err(format!(
            "getTokenAccountsByOwner({authority}, {token_program}) returned error: {}",
            compact_json_for_error(&response)
        ));
    }
    let mut records = Vec::new();
    for object in objects_with_field(&response, "account") {
        let token_account = json_string_field(object, "pubkey").unwrap_or_default();
        let mint = json_string_field(object, "mint").unwrap_or_default();
        let owner = json_string_field(object, "owner").unwrap_or_else(|| authority.to_string());
        let amount = json_string_field(object, "uiAmountString")
            .or_else(|| json_number_field(object, "amount").map(|v| v.to_string()))
            .unwrap_or_default();
        let decimals = json_number_field(object, "decimals")
            .map(|v| (v as u64).to_string())
            .unwrap_or_default();
        if token_account.is_empty() || mint.is_empty() {
            continue;
        }
        records.push(InventoryRecord {
            protocol: input.protocol.clone(),
            record_type: "token_account".to_string(),
            address: token_account,
            owner_or_authority: owner,
            lamports: 0,
            mint,
            amount,
            decimals,
            token_program: token_program.to_string(),
            notes: input.notes.clone(),
        });
    }
    Ok(records)
}

fn post_json(url: &str, body: &str, timeout_seconds: u64) -> Result<String, String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("RPC URL must start with http:// or https://".to_string());
    }
    let script = format!(
        "import urllib.request; req=urllib.request.Request({url:?}, data={body:?}.encode(), headers={{'Content-Type':'application/json'}}); print(urllib.request.urlopen(req, timeout={timeout_seconds}).read().decode())"
    );
    let output = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .output()
        .map_err(|err| format!("failed to run python3 RPC helper: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "RPC HTTP request failed against {}: {}",
            sanitize_url(url),
            sanitize_error(url, &String::from_utf8_lossy(&output.stderr))
        ));
    }
    String::from_utf8(output.stdout).map_err(|err| format!("RPC response was not UTF-8: {err}"))
}

fn write_top_protocols_json(path: &Path, protocols: &[TopProtocol]) -> Result<(), String> {
    let mut out = String::from("[\n");
    for (index, protocol) in protocols.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!(
            "  {{\"rank\":{},\"name\":\"{}\",\"slug\":\"{}\",\"category\":\"{}\",\"tvl_usd\":{},\"url\":\"{}\"}}",
            protocol.rank,
            json_escape(&protocol.name),
            json_escape(&protocol.slug),
            json_escape(&protocol.category),
            protocol.tvl_usd,
            json_escape(&protocol.url)
        ));
    }
    out.push_str("\n]\n");
    fs::write(path, out).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn write_top_protocols_csv(path: &Path, protocols: &[TopProtocol]) -> Result<(), String> {
    let mut out = String::from("rank,name,slug,category,tvl_usd,url\n");
    for protocol in protocols {
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            protocol.rank,
            csv_escape(&protocol.name),
            csv_escape(&protocol.slug),
            csv_escape(&protocol.category),
            protocol.tvl_usd,
            csv_escape(&protocol.url)
        ));
    }
    fs::write(path, out).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn write_inventory_json(path: &Path, inventory: &[InventoryRecord]) -> Result<(), String> {
    let mut out = String::from("[\n");
    for (index, record) in inventory.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!(
            "  {{\"protocol\":\"{}\",\"record_type\":\"{}\",\"address\":\"{}\",\"owner_or_authority\":\"{}\",\"lamports\":{},\"mint\":\"{}\",\"amount\":\"{}\",\"decimals\":\"{}\",\"token_program\":\"{}\",\"notes\":\"{}\"}}",
            json_escape(&record.protocol),
            json_escape(&record.record_type),
            json_escape(&record.address),
            json_escape(&record.owner_or_authority),
            record.lamports,
            json_escape(&record.mint),
            json_escape(&record.amount),
            json_escape(&record.decimals),
            json_escape(&record.token_program),
            json_escape(&record.notes)
        ));
    }
    out.push_str("\n]\n");
    fs::write(path, out).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn write_inventory_csv(path: &Path, inventory: &[InventoryRecord]) -> Result<(), String> {
    let mut out = String::from(
        "protocol,record_type,address,owner_or_authority,lamports,mint,amount,decimals,token_program,notes\n",
    );
    for record in inventory {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{}\n",
            csv_escape(&record.protocol),
            csv_escape(&record.record_type),
            csv_escape(&record.address),
            csv_escape(&record.owner_or_authority),
            record.lamports,
            csv_escape(&record.mint),
            csv_escape(&record.amount),
            csv_escape(&record.decimals),
            csv_escape(&record.token_program),
            csv_escape(&record.notes)
        ));
    }
    fs::write(path, out).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn write_audit_checklists(
    path: &Path,
    top_protocols: &[TopProtocol],
    inputs: &[ProtocolInput],
) -> Result<(), String> {
    let mut names = Vec::new();
    for protocol in top_protocols {
        if !names.contains(&protocol.name) {
            names.push(protocol.name.clone());
        }
    }
    for input in inputs {
        if !names.contains(&input.protocol) {
            names.push(input.protocol.clone());
        }
    }
    let mut out = String::from("# Solana DeFi audit target checklist\n\n");
    out.push_str("Safety: read-only reconnaissance only. Do not send exploit transactions or move assets.\n\n");
    for name in names {
        out.push_str(&format!("## {}\n\n", name));
        out.push_str("- [ ] Collect official program IDs, IDLs, source repositories, docs, and bug bounty scope.\n");
        out.push_str("- [ ] Map PDA seeds, vault authorities, pool accounts, reserve accounts, oracle accounts, and admin/config accounts.\n");
        out.push_str("- [ ] Inventory SOL lamports in program-owned accounts and SPL/Token-2022 vault balances.\n");
        out.push_str("- [ ] Verify account validation: owner, signer, writable, mint, token program, PDA seeds, and bump checks.\n");
        out.push_str("- [ ] Review oracle freshness, confidence, exponent/decimal handling, TWAP bounds, and user-supplied oracle rejection.\n");
        out.push_str("- [ ] Review asset movement paths: deposits, withdrawals, swaps, borrows, repays, liquidations, settlement, close-account flows.\n");
        out.push_str("- [ ] Review math safety: rounding direction, overflow/underflow, precision loss, fee growth, interest accrual, collateral/health calculations.\n");
        out.push_str("- [ ] Review CPI/remaining_accounts usage for arbitrary program/account substitution and confused-deputy flows.\n");
        out.push_str("- [ ] Review Token-2022 extension handling: transfer fees, hooks, freeze/close authority, non-transferable tokens.\n");
        out.push_str("- [ ] Review admin/upgrade authority, pause controls, timelocks, emergency withdraws, and config mutation bounds.\n");
        out.push_str("- [ ] Build a local/devnet proof only; never transfer mainnet assets.\n\n");
    }
    fs::write(path, out).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn split_json_objects_from_array(input: &str) -> Result<Vec<&str>, String> {
    let mut objects = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = None;
    for (index, ch) in input.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    return Err("JSON object parser saw unmatched closing brace".to_string());
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(start_index) = start.take() {
                        objects.push(&input[start_index..=index]);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(objects)
}

fn objects_with_field<'a>(input: &'a str, field: &str) -> Vec<&'a str> {
    split_json_objects_from_array(input)
        .unwrap_or_default()
        .into_iter()
        .filter(|object| object.contains(&format!("\"{field}\"")))
        .collect()
}

fn json_string_field(object: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\":");
    let start = object.find(&key)? + key.len();
    let rest = object[start..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    parse_json_string(rest)
}

fn parse_json_string(input: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = input.chars();
    if chars.next()? != '"' {
        return None;
    }
    let mut escaped = false;
    for ch in chars {
        if escaped {
            match ch {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000C}'),
                _ => out.push(ch),
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn json_number_field(object: &str, field: &str) -> Option<f64> {
    let key = format!("\"{field}\":");
    let start = object.find(&key)? + key.len();
    let rest = object[start..].trim_start();
    let end = rest
        .find(|ch: char| !(ch.is_ascii_digit() || matches!(ch, '.' | '-' | '+' | 'e' | 'E')))
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

fn json_array_field_contains(object: &str, field: &str, needle: &str) -> bool {
    let key = format!("\"{field}\":");
    let Some(start) = object.find(&key).map(|index| index + key.len()) else {
        return false;
    };
    let rest = object[start..].trim_start();
    if !rest.starts_with('[') {
        return false;
    }
    let end = rest.find(']').unwrap_or(rest.len());
    rest[..end].contains(&format!("\"{needle}\""))
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(current.clone());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    fields.push(current);
    fields
}

fn json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn compact_json_for_error(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn sanitize_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<invalid-url>".to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    format!("{scheme}://{host_port}/<redacted>")
}

fn sanitize_error<T: std::fmt::Display + ?Sized>(url: &str, err: &T) -> String {
    let sanitized = sanitize_url(url);
    let message = err.to_string();
    let mut redacted = message.replace(url, &sanitized);
    if let Some((scheme, rest)) = url.split_once("://") {
        redacted = redacted.replace(rest, "<redacted>");
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        redacted = redacted.replace(authority, "<redacted>");
        if let Some(path_and_query) = rest.get(authority_end..) {
            if !path_and_query.is_empty() {
                redacted = redacted.replace(path_and_query, "<redacted>");
            }
        }
        redacted = redacted.replace(&format!("{scheme}://<redacted>"), &sanitized);
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_csv_handles_quotes() {
        let fields = parse_csv_line("Proto,111,222,\"hello, world\"");
        assert_eq!(fields, vec!["Proto", "111", "222", "hello, world"]);
    }

    #[test]
    fn json_string_and_number_fields_work() {
        let object = r#"{"name":"Kamino","tvl":123.5,"chains":["Solana","Ethereum"]}"#;
        assert_eq!(
            json_string_field(object, "name"),
            Some("Kamino".to_string())
        );
        assert_eq!(json_number_field(object, "tvl"), Some(123.5));
        assert!(json_array_field_contains(object, "chains", "Solana"));
    }

    #[test]
    fn split_json_objects_from_array_finds_nested_objects() {
        let input = r#"[{"a":1,"nested":{"b":2}},{"a":3}]"#;
        let objects = split_json_objects_from_array(input).unwrap();
        assert_eq!(objects.len(), 2);
        assert!(objects[0].contains("nested"));
    }

    #[test]
    fn sanitizer_removes_url_secrets() {
        let url = "https://user:secret@example.com/v2/api-key?token=secret";
        let err = "failed https://user:secret@example.com/v2/api-key?token=secret";
        let redacted = sanitize_error(url, &err);
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("api-key"));
        assert!(redacted.contains("https://example.com/<redacted>"));
    }

    #[test]
    fn config_rejects_bad_rpc_url() {
        let err =
            Config::parse(["--rpc-url".to_string(), "file:///tmp/socket".to_string()]).unwrap_err();
        assert!(err.contains("http:// or https://"));
    }
}
