# Agave Clap Utils

The clap-utils module provides utilities for command-line argument parsing in the Agave blockchain platform. It extends the functionality of the Clap (Command Line Argument Parser) library with Agave-specific types and helpers, making it easier to create consistent and user-friendly command-line interfaces across the platform.

## Architecture Overview

```mermaid
graph TD
    A[CLI Applications] -->|Use| B[Clap Utils]
    B -->|Extends| C[Clap Library]
    
    subgraph "Clap Utils Components"
    D[Input Validators]
    E[Type Converters]
    F[Arg Constructors]
    G[Common Arguments]
    end
    
    B --- D
    B --- E
    B --- F
    B --- G
    
    D -->|Validates| H[User Input]
    E -->|Converts| I[String to Types]
    F -->|Creates| J[Argument Definitions]
    G -->|Provides| K[Reusable Arguments]
    
    L[CLI Modules] -->|Depend on| B
```

## Key Components

### Input Validators
The Input Validators component provides functions to validate command-line input:
- Keypair file validation
- Public key format validation
- Numeric range validation
- URL format validation
- File path validation
- Hash format validation

### Type Converters
The Type Converters component handles conversion between string inputs and Agave types:
- Public key conversion
- Hash conversion
- Signature conversion
- Lamport amount conversion
- Commitment level conversion
- Keypair loading and parsing

### Arg Constructors
The Arg Constructors component provides helper functions for creating common argument types:
- Keypair arguments
- Public key arguments
- Amount arguments
- Hash arguments
- URL arguments
- Fee arguments
- Commitment arguments

### Common Arguments
The Common Arguments component defines reusable argument definitions for:
- RPC URL configuration
- Keypair specification
- Fee parameters
- Transaction confirmation
- Output formatting
- Verbose logging
- Commitment level

## Usage Examples

### Creating a Command with Common Arguments

```rust
use clap::{App, Arg, SubCommand};
use solana_clap_utils::{
    input_validators::{is_url, is_keypair_or_ask_keyword},
    keypair::DefaultSigner,
    fee_payer::fee_payer_arg,
    commitment::commitment_arg,
};

// Create a command with common arguments
let app = App::new("my-command")
    .arg(
        Arg::with_name("config_file")
            .long("config")
            .value_name("PATH")
            .help("Configuration file path")
    )
    .arg(
        Arg::with_name("url")
            .long("url")
            .value_name("URL")
            .validator(is_url)
            .help("RPC URL to the cluster")
    )
    .arg(
        Arg::with_name("keypair")
            .long("keypair")
            .value_name("KEYPAIR")
            .validator(is_keypair_or_ask_keyword)
            .help("Keypair path or ASK keyword")
    )
    .arg(fee_payer_arg())
    .arg(commitment_arg());
```

### Using Input Validators

```rust
use clap::{App, Arg};
use solana_clap_utils::input_validators::{
    is_amount, is_parsable, is_url, is_valid_pubkey, normalize_to_url_if_moniker
};

// Create arguments with input validation
let app = App::new("validator")
    .arg(
        Arg::with_name("identity")
            .long("identity")
            .value_name("KEYPAIR")
            .validator(is_keypair_or_ask_keyword)
            .help("Validator identity keypair")
    )
    .arg(
        Arg::with_name("vote_account")
            .long("vote-account")
            .value_name("PUBKEY")
            .validator(is_valid_pubkey)
            .help("Vote account public key")
    )
    .arg(
        Arg::with_name("rpc_url")
            .long("rpc-url")
            .value_name("URL")
            .validator(is_url)
            .default_value("https://api.mainnet-beta.solana.com")
            .help("RPC URL for the cluster")
    )
    .arg(
        Arg::with_name("stake_amount")
            .long("stake-amount")
            .value_name("AMOUNT")
            .validator(is_amount)
            .help("Amount to stake in SOL")
    );
```

### Loading Keypairs

```rust
use clap::{App, Arg};
use solana_clap_utils::{
    keypair::{keypair_arg, DefaultSigner},
    input_validators::is_keypair_or_ask_keyword,
};
use solana_sdk::signature::Signer;

// Define a command with keypair argument
let app = App::new("sign-message")
    .arg(
        keypair_arg()
            .value_name("KEYPAIR")
            .validator(is_keypair_or_ask_keyword)
            .help("Keypair to use for signing")
    );

// Parse arguments
let matches = app.get_matches();

// Load the keypair
let default_signer = DefaultSigner::new("keypair", &matches);
let keypair = default_signer.signer_from_path(&matches)?;

// Use the keypair
let signature = keypair.sign_message(&message);
println!("Signature: {}", signature);
```

### Working with Fee Parameters

```rust
use clap::{App, Arg};
use solana_clap_utils::{
    fee_payer::fee_payer_arg,
    input_parsers::lamports_of_sol,
};
use solana_sdk::commitment_config::CommitmentConfig;

// Create a command with fee parameters
let app = App::new("transfer")
    .arg(fee_payer_arg())
    .arg(
        Arg::with_name("fee")
            .long("fee")
            .value_name("SOL")
            .help("Transaction fee in SOL")
    );

// Parse arguments
let matches = app.get_matches();

// Get fee payer
let fee_payer = get_fee_payer(matches, "fee_payer", &mut wallet_manager)?;

// Parse fee amount if provided
let fee = matches.value_of("fee")
    .map(|f| lamports_of_sol(f))
    .transpose()?;
```

## Integration with CLI Applications

The clap-utils module is used throughout the Agave CLI applications:

- **solana-cli**: Main command-line interface for interacting with the blockchain
- **solana-validator**: Command-line interface for running a validator
- **solana-keygen**: Command-line interface for generating keypairs
- **solana-bench-tps**: Command-line interface for benchmarking transactions per second
- **solana-test-validator**: Command-line interface for running a local test validator

## Development

### Building

To build the clap-utils module:

```bash
cd clap-utils
cargo build
```

### Testing

To run the tests for the clap-utils module:

```bash
cd clap-utils
cargo test
```

## Further Reading

For more detailed information about command-line interfaces in Agave, refer to the following resources:

- [CLI Documentation](../cli/README.md)
- [Clap Library Documentation](https://docs.rs/clap/)
- [Keypair Management](https://docs.anza.xyz/wallet-guide/key-management)
- [CLI Configuration](https://docs.anza.xyz/cli/configure)