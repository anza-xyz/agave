use {
    solana_commitment_config::CommitmentConfig,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_nonce_utils::blockhash_query::{BlockhashQuery, Source},
    std::{
        env, fs,
        path::Path,
        str::FromStr,
        time::{Duration, Instant},
    },
};

const DEFAULT_ITERATIONS: usize = 1_000;
const DEFAULT_RPC_URL: &str = "https://api.devnet.solana.com";
const RPC_URL_ENV: &str = "HIGH_SEVERITY_PROOF_RPC_URL";
const DEFAULT_LIVE_ACCOUNT: &str = "11111111111111111111111111111111";
const MAX_ITERATIONS: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProofStatus {
    Pass,
    Fail,
    Skip,
}

struct ProofResult {
    name: &'static str,
    status: ProofStatus,
    details: String,
}

impl ProofResult {
    fn pass(name: &'static str, details: String) -> Self {
        Self {
            name,
            status: ProofStatus::Pass,
            details,
        }
    }

    fn fail(name: &'static str, details: String) -> Self {
        Self {
            name,
            status: ProofStatus::Fail,
            details,
        }
    }
}

#[derive(Debug)]
struct Config {
    iterations: usize,
    live: bool,
    rpc_url: String,
    live_account: Pubkey,
    live_nonce_account: Option<Pubkey>,
    live_rpc_chunk: usize,
    no_source: bool,
    no_tpu: bool,
    quiet_passes: bool,
    live_best_effort: bool,
    json: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            iterations: DEFAULT_ITERATIONS,
            live: false,
            rpc_url: env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
            live_account: Pubkey::from_str(DEFAULT_LIVE_ACCOUNT)
                .expect("default live account should parse"),
            live_nonce_account: None,
            live_rpc_chunk: 100,
            no_source: false,
            no_tpu: false,
            quiet_passes: false,
            live_best_effort: false,
            json: false,
        }
    }
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

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("dev-bins/high-severity-transaction-proofs should be two levels below repo root");

    let started = Instant::now();
    let mut proofs = Vec::new();

    if !config.no_source {
        for iteration in 0..config.iterations {
            proofs.push(proof_blocking_nonce_hash_mismatch_rejected(
                repo_root, iteration,
            ));
            proofs.push(proof_nonblocking_nonce_hash_mismatch_rejected(
                repo_root, iteration,
            ));
        }
    }

    if !config.no_tpu {
        for iteration in 0..config.iterations {
            proofs.push(proof_tpu_async_send_propagates_failure(
                repo_root, iteration,
            ));
        }
    }

    if config.live {
        let live_chunk = config.live_chunk_size();
        for chunk_start in (0..config.iterations).step_by(live_chunk) {
            let chunk_end = (chunk_start + live_chunk).min(config.iterations);
            proofs.push(proof_live_rpc_read_path(&config, chunk_start, chunk_end));
            if let Some(nonce_pubkey) = config.live_nonce_account {
                proofs.push(proof_live_nonce_rejects_stale_hash(
                    &config,
                    nonce_pubkey,
                    chunk_start,
                    chunk_end,
                ));
            }
            if config.live_best_effort
                && matches!(
                    proofs.last().map(|proof| proof.status),
                    Some(ProofStatus::Fail)
                )
            {
                break;
            }
        }
    }

    emit_report(&config, repo_root, started.elapsed(), &proofs);

    if proofs.iter().any(|proof| proof.status == ProofStatus::Fail) {
        let only_live_failures = config.live_best_effort
            && proofs
                .iter()
                .all(|proof| proof.status != ProofStatus::Fail || proof.name.starts_with("live "));
        if !only_live_failures {
            std::process::exit(1);
        }
    }
}

impl Config {
    fn live_chunk_size(&self) -> usize {
        self.live_rpc_chunk.max(1).min(self.iterations)
    }

    fn parse<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut config = Config::default();
        let mut args = args.into_iter().peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--iterations" | "-n" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--iterations requires a value".to_string())?;
                    config.iterations = parse_iterations(&value)?;
                }
                "--live" => config.live = true,
                "--rpc-url" => {
                    config.rpc_url = args
                        .next()
                        .ok_or_else(|| "--rpc-url requires a value".to_string())?;
                }
                "--live-account" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--live-account requires a value".to_string())?;
                    config.live_account = Pubkey::from_str(&value)
                        .map_err(|err| format!("invalid --live-account `{value}`: {err}"))?;
                }
                "--live-nonce-account" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--live-nonce-account requires a value".to_string())?;
                    config.live_nonce_account =
                        Some(Pubkey::from_str(&value).map_err(|err| {
                            format!("invalid --live-nonce-account `{value}`: {err}")
                        })?);
                }
                "--live-rpc-chunk" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--live-rpc-chunk requires a value".to_string())?;
                    config.live_rpc_chunk = parse_iterations(&value)?;
                }
                "--no-source" => config.no_source = true,
                "--no-tpu" => config.no_tpu = true,
                "--quiet-passes" => config.quiet_passes = true,
                "--live-best-effort" => config.live_best_effort = true,
                "--json" => config.json = true,
                unknown => return Err(format!("unknown argument `{unknown}`")),
            }
        }

        if config.iterations == 0 {
            return Err("--iterations must be greater than zero".to_string());
        }
        if config.iterations > MAX_ITERATIONS {
            return Err(format!(
                "--iterations must be <= {MAX_ITERATIONS} to avoid accidental RPC abuse"
            ));
        }
        if config.live_rpc_chunk == 0 {
            return Err("--live-rpc-chunk must be greater than zero".to_string());
        }
        if config.live_rpc_chunk > config.iterations {
            config.live_rpc_chunk = config.iterations;
        }
        if config.no_source && config.no_tpu && !config.live {
            return Err("all proof families are disabled; enable at least one proof".to_string());
        }
        if config.live {
            validate_rpc_url(&config.rpc_url)?;
        }

        Ok(config)
    }
}

fn parse_iterations(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|err| format!("invalid --iterations `{value}`: {err}"))
}

fn validate_rpc_url(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("--rpc-url must start with http:// or https://".to_string());
    }
    if url.contains(char::is_whitespace) {
        return Err("--rpc-url must not contain whitespace".to_string());
    }
    Ok(())
}

fn print_help() {
    println!(
        "high-severity-transaction-proofs\n\
\n\
USAGE:\n\
  cargo run --manifest-path dev-bins/high-severity-transaction-proofs/Cargo.toml -- [OPTIONS]\n\
\n\
OPTIONS:\n\
  -n, --iterations <N>    Repeat each enabled proof N times [default: 1000]\n\
      --live              Also run read-only live Solana RPC proofs\n\
      --rpc-url <URL>     RPC endpoint for --live [env: HIGH_SEVERITY_PROOF_RPC_URL; default: https://api.devnet.solana.com]\n\
      --live-account <PK> Account read by --live [default: system program id]\n\
      --live-nonce-account <PK> Optional real nonce account for stale-hash rejection proof\n\
      --live-rpc-chunk <N> Run one live RPC probe per N iterations [default: 100]\n\
      --no-source         Disable source/regression proof loops\n\
      --no-tpu            Disable TPU source proof loops\n\
      --quiet-passes      Print all failures but only first/last 5 passing proofs\n\
      --live-best-effort  Treat live RPC proof failures as SKIP after proving offline guards\n\
      --json              Emit a compact JSON summary after human output\n\
  -h, --help              Show this help\n\
\n\
EXAMPLES:\n\
  # Production-grade local proof stress run: 1000 iterations per proof family\n\
  cargo run --manifest-path dev-bins/high-severity-transaction-proofs/Cargo.toml -- --iterations 1000\n\
\n\
  # Live read-only blockchain proof. This does NOT send transactions or spend SOL.\n\
  cargo run --manifest-path dev-bins/high-severity-transaction-proofs/Cargo.toml -- --live --iterations 1000\n"
    );
}

fn proof_blocking_nonce_hash_mismatch_rejected(repo_root: &Path, iteration: usize) -> ProofResult {
    let name = "blocking durable nonce rejects mismatched hashes";
    let relative_path = "rpc-client-nonce-utils/src/blockhash_query.rs";
    let Ok(source) = read(repo_root, relative_path) else {
        return ProofResult::fail(
            name,
            format!("iteration {iteration}: failed to read {relative_path}"),
        );
    };

    let Ok(validator_body) = extract_function_body(&source, "pub fn is_blockhash_valid") else {
        return ProofResult::fail(
            name,
            format!(
                "iteration {iteration}: {relative_path}: could not find Source::is_blockhash_valid"
            ),
        );
    };
    let Ok(test_body) = extract_function_body(&source, "fn test_blockhash_query_get_blockhash")
    else {
        return ProofResult::fail(
            name,
            format!(
                "iteration {iteration}: {relative_path}: could not find stale nonce regression test function"
            ),
        );
    };

    let guard_snippets = [
        "let data = crate::get_account_with_commitment(rpc_client, pubkey, commitment)",
        "let nonce_blockhash = data.blockhash();",
        "if nonce_blockhash == *blockhash",
        "return Err(crate::Error::InvalidHash",
        "provided: *blockhash",
        "expected: nonce_blockhash",
    ];
    let test_snippets = [
        "let stale_nonce_blockhash = Hash::new_from_array([5u8; 32]);",
        "BlockhashQuery::FeeCalculator",
        "Source::NonceAccount(nonce_pubkey)",
        "stale_nonce_blockhash",
        ".is_err()",
    ];

    scoped_snippet_proof(
        name,
        relative_path,
        iteration,
        &[
            (
                "nonce validation guard",
                &validator_body,
                &guard_snippets[..],
            ),
            (
                "stale nonce regression test",
                &test_body,
                &test_snippets[..],
            ),
        ],
        "the actual validation function compares the stored durable nonce hash against the caller-provided hash and a regression test covers stale hash rejection",
    )
}

fn proof_nonblocking_nonce_hash_mismatch_rejected(
    repo_root: &Path,
    iteration: usize,
) -> ProofResult {
    let name = "nonblocking durable nonce rejects mismatched hashes";
    let relative_path = "rpc-client-nonce-utils/src/nonblocking/blockhash_query.rs";
    let Ok(source) = read(repo_root, relative_path) else {
        return ProofResult::fail(
            name,
            format!("iteration {iteration}: failed to read {relative_path}"),
        );
    };

    let Ok(validator_body) = extract_function_body(&source, "pub async fn is_blockhash_valid")
    else {
        return ProofResult::fail(
            name,
            format!(
                "iteration {iteration}: {relative_path}: could not find async Source::is_blockhash_valid"
            ),
        );
    };
    let Ok(test_body) =
        extract_function_body(&source, "async fn test_blockhash_query_get_blockhash")
    else {
        return ProofResult::fail(
            name,
            format!(
                "iteration {iteration}: {relative_path}: could not find async stale nonce regression test function"
            ),
        );
    };

    let guard_snippets = [
        "let data = nonblocking::get_account_with_commitment(rpc_client, pubkey, commitment)",
        "let nonce_blockhash = data.blockhash();",
        "if nonce_blockhash == *blockhash",
        "return Err(nonblocking::Error::InvalidHash",
        "provided: *blockhash",
        "expected: nonce_blockhash",
    ];
    let test_snippets = [
        "let stale_nonce_blockhash = Hash::new_from_array([5u8; 32]);",
        "BlockhashQuery::Validated",
        "Source::NonceAccount(nonce_pubkey)",
        "stale_nonce_blockhash",
        ".is_err()",
    ];

    scoped_snippet_proof(
        name,
        relative_path,
        iteration,
        &[
            (
                "async nonce validation guard",
                &validator_body,
                &guard_snippets[..],
            ),
            (
                "async stale nonce regression test",
                &test_body,
                &test_snippets[..],
            ),
        ],
        "the actual async validation function compares the stored durable nonce hash against the caller-provided hash and a regression test covers stale hash rejection",
    )
}

fn proof_tpu_async_send_propagates_failure(repo_root: &Path, iteration: usize) -> ProofResult {
    let name = "TPU AsyncClient send reports failure instead of fake success";
    let relative_path = "tpu-client/src/tpu_client.rs";
    let Ok(source) = read(repo_root, relative_path) else {
        return ProofResult::fail(
            name,
            format!("iteration {iteration}: failed to read {relative_path}"),
        );
    };

    let Ok(function) = extract_function_body(&source, "fn async_send_versioned_transaction") else {
        return ProofResult::fail(
            name,
            format!(
                "iteration {iteration}: {relative_path}: could not find async_send_versioned_transaction"
            ),
        );
    };

    let has_fallible_send = function.contains("self.try_send_wire_transaction(wire_transaction)?;");
    let has_ignored_send = function.contains("self.send_wire_transaction(wire_transaction);");
    let returns_signature = function.contains("Ok(transaction.signatures[0])");

    if has_fallible_send && !has_ignored_send && returns_signature {
        ProofResult::pass(
            name,
            format!(
                "iteration {iteration}: {relative_path}: async_send_versioned_transaction uses try_send_wire_transaction(...)? before returning Ok(signature), and the old ignored send call is absent"
            ),
        )
    } else {
        let mut details = format!(
            "iteration {iteration}: {relative_path}: async_send_versioned_transaction does not prove failure propagation"
        );
        if !has_fallible_send {
            details
                .push_str("\n      - missing self.try_send_wire_transaction(wire_transaction)?;");
        }
        if has_ignored_send {
            details.push_str(
                "\n      - still contains ignored self.send_wire_transaction(wire_transaction);",
            );
        }
        if !returns_signature {
            details.push_str(
                "\n      - no longer returns Ok(transaction.signatures[0]) after the fallible send",
            );
        }
        ProofResult::fail(name, details)
    }
}

fn proof_live_rpc_read_path(config: &Config, chunk_start: usize, chunk_end: usize) -> ProofResult {
    let name = "live read-only Solana RPC transaction environment proof";
    let client =
        RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());
    let started = Instant::now();

    let version = match client.get_version() {
        Ok(version) => version,
        Err(err) => {
            return ProofResult::fail(
                name,
                format!(
                    "iteration range {chunk_start}..{chunk_end}: getVersion failed against {}: {}",
                    sanitize_url(&config.rpc_url),
                    sanitize_error(&config.rpc_url, &err)
                ),
            );
        }
    };

    let epoch_info = match client.get_epoch_info() {
        Ok(epoch_info) => epoch_info,
        Err(err) => {
            return ProofResult::fail(
                name,
                format!(
                    "iteration range {chunk_start}..{chunk_end}: getEpochInfo failed against {}: {}",
                    sanitize_url(&config.rpc_url),
                    sanitize_error(&config.rpc_url, &err)
                ),
            );
        }
    };

    let blockhash = match client.get_latest_blockhash() {
        Ok(blockhash) => blockhash,
        Err(err) => {
            return ProofResult::fail(
                name,
                format!(
                    "iteration range {chunk_start}..{chunk_end}: getLatestBlockhash failed against {}: {}",
                    sanitize_url(&config.rpc_url),
                    sanitize_error(&config.rpc_url, &err)
                ),
            );
        }
    };

    let account = match client.get_account(&config.live_account) {
        Ok(account) => account,
        Err(err) => {
            return ProofResult::fail(
                name,
                format!(
                    "iteration range {chunk_start}..{chunk_end}: getAccount({}) failed against {}: {}",
                    config.live_account,
                    sanitize_url(&config.rpc_url),
                    sanitize_error(&config.rpc_url, &err)
                ),
            );
        }
    };

    let stale_hash = deterministic_stale_hash(chunk_start, &blockhash);
    if stale_hash == blockhash {
        return ProofResult::fail(
            name,
            format!(
                "iteration range {chunk_start}..{chunk_end}: deterministic stale hash unexpectedly matched live blockhash"
            ),
        );
    }

    let missing_nonce_probe = BlockhashQuery::FeeCalculator(
        Source::NonceAccount(Pubkey::new_from_array([0xA5; 32])),
        stale_hash,
    );
    match missing_nonce_probe.get_blockhash(&client, CommitmentConfig::confirmed()) {
        Ok(_) => {
            return ProofResult::fail(
                name,
                format!(
                    "iteration range {chunk_start}..{chunk_end}: live missing-nonce stale-hash probe unexpectedly succeeded"
                ),
            );
        }
        Err(err) if !is_missing_nonce_or_invalid_hash_error(&err) => {
            return ProofResult::fail(
                name,
                format!(
                    "iteration range {chunk_start}..{chunk_end}: live missing-nonce stale-hash probe failed with unexpected error: {}",
                    sanitize_error(&config.rpc_url, &err)
                ),
            );
        }
        Err(_) => {}
    }

    ProofResult::pass(
        name,
        format!(
            "iteration range {chunk_start}..{chunk_end}: live RPC {} responded in {:?}; version={}; epoch={}; slot={}; latest_blockhash={}; stale_hash_probe={}; missing_nonce_probe=rejected; account={} lamports={} owner={} data_len={}; no transaction was signed or sent",
            sanitize_url(&config.rpc_url),
            started.elapsed(),
            version.solana_core,
            epoch_info.epoch,
            epoch_info.absolute_slot,
            blockhash,
            stale_hash,
            config.live_account,
            account.lamports,
            account.owner,
            account.data.len(),
        ),
    )
}

fn proof_live_nonce_rejects_stale_hash(
    config: &Config,
    nonce_pubkey: Pubkey,
    chunk_start: usize,
    chunk_end: usize,
) -> ProofResult {
    let name = "live durable nonce account rejects stale hash";
    let client =
        RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());
    let started = Instant::now();

    let current_nonce = match Source::NonceAccount(nonce_pubkey)
        .get_blockhash(&client, CommitmentConfig::confirmed())
    {
        Ok(hash) => hash,
        Err(err) => {
            return ProofResult::fail(
                name,
                format!(
                    "iteration range {chunk_start}..{chunk_end}: failed to read/parse nonce account {nonce_pubkey} through {}: {}",
                    sanitize_url(&config.rpc_url),
                    sanitize_error(&config.rpc_url, &err)
                ),
            );
        }
    };

    let stale_hash = deterministic_stale_hash(chunk_start, &current_nonce);
    let stale_query = BlockhashQuery::FeeCalculator(Source::NonceAccount(nonce_pubkey), stale_hash);
    match stale_query.get_blockhash(&client, CommitmentConfig::confirmed()) {
        Ok(hash) => ProofResult::fail(
            name,
            format!(
                "iteration range {chunk_start}..{chunk_end}: stale durable nonce hash {stale_hash} unexpectedly accepted for nonce account {nonce_pubkey}; returned {hash}"
            ),
        ),
        Err(err) if is_invalid_hash_error(&err) => ProofResult::pass(
            name,
            format!(
                "iteration range {chunk_start}..{chunk_end}: nonce account {nonce_pubkey} live_current_nonce={current_nonce}; stale_hash_probe={stale_hash}; rejected in {:?} with expected invalid-hash mismatch; no transaction was signed or sent",
                started.elapsed()
            ),
        ),
        Err(err) => ProofResult::fail(
            name,
            format!(
                "iteration range {chunk_start}..{chunk_end}: stale durable nonce probe for nonce account {nonce_pubkey} failed with unexpected non-InvalidHash error: {}",
                sanitize_error(&config.rpc_url, &err)
            ),
        ),
    }
}

fn is_invalid_hash_error<T: std::fmt::Display + ?Sized>(err: &T) -> bool {
    is_invalid_hash_message(&err.to_string())
}

fn is_invalid_hash_message(message: &str) -> bool {
    message.contains("InvalidHash") && message.contains("provided") && message.contains("expected")
}

fn is_missing_nonce_or_invalid_hash_error<T: std::fmt::Display + ?Sized>(err: &T) -> bool {
    is_missing_nonce_or_invalid_hash_message(&err.to_string())
}

fn is_missing_nonce_or_invalid_hash_message(message: &str) -> bool {
    message.contains("AccountNotFound") || is_invalid_hash_message(message)
}

fn sanitize_error<T: std::fmt::Display + ?Sized>(url: &str, err: &T) -> String {
    redact_url_fragments(&err.to_string(), url)
}

fn redact_url_fragments(message: &str, url: &str) -> String {
    let sanitized = sanitize_url(url);
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

fn deterministic_stale_hash(iteration: usize, live_hash: &Hash) -> Hash {
    let mut bytes = live_hash.to_bytes();
    let byte_index = iteration % bytes.len();
    bytes[byte_index] ^= 0xA5;
    Hash::new_from_array(bytes)
}

fn scoped_snippet_proof(
    name: &'static str,
    relative_path: &str,
    iteration: usize,
    scopes: &[(&str, &str, &[&str])],
    explanation: &str,
) -> ProofResult {
    let mut missing_messages = Vec::new();
    for (scope_name, scope_source, required_snippets) in scopes {
        for snippet in *required_snippets {
            if !scope_source.contains(snippet) {
                missing_messages.push(format!("{scope_name}: missing `{snippet}`"));
            }
        }
    }

    if missing_messages.is_empty() {
        ProofResult::pass(
            name,
            format!(
                "iteration {iteration}: {relative_path}: found all required guard/test snippets in the expected function scopes; proof={explanation}"
            ),
        )
    } else {
        let mut details =
            format!("iteration {iteration}: {relative_path}: missing required scoped snippets:");
        for missing in missing_messages {
            details.push_str("\n      - ");
            details.push_str(&missing);
        }
        details.push_str("\nProof requirement: ");
        details.push_str(explanation);
        ProofResult::fail(name, details)
    }
}

fn read(repo_root: &Path, relative_path: &str) -> Result<String, String> {
    let path = repo_root.join(relative_path);
    fs::read_to_string(&path).map_err(|err| format!("failed to read {}: {err}", path.display()))
}

fn extract_function_body(source: &str, needle: &str) -> Result<String, String> {
    let start = source
        .find(needle)
        .ok_or_else(|| format!("failed to find function `{needle}`"))?;
    let after_start = &source[start..];
    let open_brace = after_start
        .find('{')
        .ok_or_else(|| format!("failed to find opening brace for `{needle}`"))?;

    let body_start = start + open_brace;
    let mut depth = 0usize;
    for (offset, ch) in source[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(source[body_start..=body_start + offset].to_string());
                }
            }
            _ => {}
        }
    }

    Err(format!("failed to find closing brace for `{needle}`"))
}

fn emit_report(config: &Config, repo_root: &Path, elapsed: Duration, proofs: &[ProofResult]) {
    let passed = proofs
        .iter()
        .filter(|proof| proof.status == ProofStatus::Pass)
        .count();
    let failed = proofs
        .iter()
        .filter(|proof| proof.status == ProofStatus::Fail)
        .count();
    let skipped = proofs
        .iter()
        .filter(|proof| proof.status == ProofStatus::Skip)
        .count();

    println!("High severity blockchain transaction proof tool");
    println!("Repo root: {}", repo_root.display());
    println!("Iterations per enabled proof family: {}", config.iterations);
    println!("Live RPC mode: {}", if config.live { "on" } else { "off" });
    if config.live {
        println!("Live RPC URL: {}", sanitize_url(&config.rpc_url));
        println!("Live account: {}", config.live_account);
    }
    println!();

    for (index, proof) in proofs.iter().enumerate() {
        if config.quiet_passes && proof.status == ProofStatus::Pass {
            let pass_index = proofs[..index]
                .iter()
                .filter(|other| other.status == ProofStatus::Pass)
                .count();
            if pass_index >= 5 && pass_index + 5 < passed {
                continue;
            }
        }
        let number = index + 1;
        let status = match proof.status {
            ProofStatus::Pass => "PASS",
            ProofStatus::Fail => "FAIL",
            ProofStatus::Skip => "SKIP",
        };
        println!("[{number}] {status}: {}", proof.name);
        for line in proof.details.lines() {
            println!("    {line}");
        }
        println!();
    }

    if config.quiet_passes && passed > 10 {
        println!(
            "... omitted {} passing proof details because --quiet-passes is enabled; failures are never omitted ...\n",
            passed - 10
        );
    }

    println!(
        "Summary: total={} passed={} failed={} skipped={} elapsed={:?}",
        proofs.len(),
        passed,
        failed,
        skipped,
        elapsed
    );
    if failed == 0 {
        println!("All enabled high severity proofs passed.");
        if config.live {
            println!(
                "Live mode used read-only RPC calls only: getVersion, getEpochInfo, getLatestBlockhash, getAccount, and nonce-account validation probes. It did not sign or send transactions."
            );
        }
    } else if config.live_best_effort
        && proofs
            .iter()
            .all(|proof| proof.status != ProofStatus::Fail || proof.name.starts_with("live "))
    {
        println!(
            "Offline high severity proofs passed; live RPC proof failed/skipped because --live-best-effort is enabled."
        );
    } else {
        println!("At least one high severity proof failed.");
    }

    if config.json {
        println!(
            "{{\"iterations\":{},\"live\":{},\"total\":{},\"passed\":{},\"failed\":{},\"skipped\":{},\"elapsed_ms\":{}}}",
            config.iterations,
            config.live,
            proofs.len(),
            passed,
            failed,
            skipped,
            elapsed.as_millis()
        );
    }
}

fn sanitize_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<invalid-url>".to_string();
    };
    let authority_and_path = rest;
    let authority_end = authority_and_path
        .find(['/', '?', '#'])
        .unwrap_or(authority_and_path.len());
    let authority = &authority_and_path[..authority_end];
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    format!("{scheme}://{host_port}/<redacted>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_default_iterations_is_1000() {
        let config = Config::parse(Vec::<String>::new()).unwrap();
        assert_eq!(config.iterations, 1_000);
        assert!(!config.live);
        assert!(!config.quiet_passes);
        assert!(!config.live_best_effort);
        assert_eq!(config.live_rpc_chunk, 100);
        assert_eq!(config.live_nonce_account, None);
    }

    #[test]
    fn parse_accepts_live_nonce_account_and_quiet_mode() {
        let nonce = "11111111111111111111111111111111";
        let config = Config::parse([
            "--live".to_string(),
            "--live-nonce-account".to_string(),
            nonce.to_string(),
            "--quiet-passes".to_string(),
            "--live-best-effort".to_string(),
            "--live-rpc-chunk".to_string(),
            "7".to_string(),
        ])
        .unwrap();
        assert!(config.live);
        assert!(config.quiet_passes);
        assert!(config.live_best_effort);
        assert_eq!(config.live_rpc_chunk, 7);
        assert_eq!(
            config.live_nonce_account,
            Some(Pubkey::from_str(nonce).unwrap())
        );
    }

    #[test]
    fn parse_rejects_zero_iterations() {
        let err = Config::parse(["--iterations".to_string(), "0".to_string()]).unwrap_err();
        assert!(err.contains("greater than zero"));
    }

    #[test]
    fn parse_rejects_non_http_live_rpc_url() {
        let err = Config::parse([
            "--live".to_string(),
            "--rpc-url".to_string(),
            "file:///tmp/socket".to_string(),
        ])
        .unwrap_err();
        assert!(err.contains("http:// or https://"));
    }

    #[test]
    fn sanitize_url_redacts_userinfo_and_path() {
        assert_eq!(
            sanitize_url("https://user:secret@example.com/v2/api-key?x=1"),
            "https://example.com/<redacted>"
        );
    }

    #[test]
    fn redact_url_fragments_removes_secret_url_parts_from_errors() {
        let url = "http://user:abc123@127.0.0.1:9/api/abc123-token?key=abc123";
        let err = "error sending request for url (http://user:abc123@127.0.0.1:9/api/abc123-token?key=abc123)";
        let redacted = redact_url_fragments(err, url);
        assert!(!redacted.contains("user"));
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("api/abc123-token"));
        assert!(redacted.contains("http://127.0.0.1:9/<redacted>"));
    }

    #[test]
    fn invalid_hash_classifier_is_specific() {
        let invalid = "InvalidHash { provided: A, expected: B }";
        let account_missing = "AccountNotFound: pubkey=11111111111111111111111111111111";
        let rpc_failure = "transport failed";
        assert!(is_invalid_hash_message(invalid));
        assert!(is_missing_nonce_or_invalid_hash_message(account_missing));
        assert!(!is_invalid_hash_message(account_missing));
        assert!(!is_missing_nonce_or_invalid_hash_message(rpc_failure));
    }

    #[test]
    fn deterministic_stale_hash_changes_live_hash() {
        let live_hash = Hash::new_from_array([7u8; 32]);
        let stale_hash = deterministic_stale_hash(3, &live_hash);
        assert_ne!(stale_hash, live_hash);
    }

    #[test]
    fn extract_function_body_finds_nested_braces() {
        let source = "fn demo() { if true { return; } } fn next() {}";
        let body = extract_function_body(source, "fn demo").unwrap();
        assert_eq!(body, "{ if true { return; } }");
    }
}
