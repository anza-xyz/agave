# Agave Blockchain Platform Usage Examples

This document provides comprehensive examples of how to use the Agave blockchain platform for various use cases. These examples cover common scenarios and demonstrate the platform's capabilities.

## Basic Usage

### Setting Up a Development Environment

1. Install the Agave CLI:
   ```bash
   curl -sSf https://raw.githubusercontent.com/anza-xyz/agave/master/install/agave-install-init.sh | sh
   ```

2. Start a local test validator:
   ```bash
   agave-test-validator
   ```

3. Create a wallet for development:
   ```bash
   agave-keygen new -o wallet.json
   ```

4. Request airdrop for testing:
   ```bash
   agave airdrop 1 --keypair wallet.json
   ```

### Basic Transactions

#### Transfer SOL

```bash
# Transfer 0.1 SOL to another account
agave transfer recipient_address 0.1 --keypair wallet.json
```

#### Check Account Balance

```bash
# Check the balance of your account
agave balance --keypair wallet.json

# Check the balance of another account
agave balance recipient_address
```

## Smart Contract Development

### Deploying a Program

1. Write a simple program in Rust:
   ```rust
   use solana_program::{
       account_info::AccountInfo,
       entrypoint,
       entrypoint::ProgramResult,
       msg,
       pubkey::Pubkey,
   };

   entrypoint!(process_instruction);

   fn process_instruction(
       program_id: &Pubkey,
       accounts: &[AccountInfo],
       instruction_data: &[u8],
   ) -> ProgramResult {
       msg!("Hello, Agave!");
       Ok(())
   }
   ```

2. Build the program:
   ```bash
   cargo build-sbf
   ```

3. Deploy the program:
   ```bash
   agave program deploy target/deploy/myprogram.so --keypair wallet.json
   ```

### Interacting with a Program

```bash
# Create an account owned by the program
agave create-account program_id 1000 --keypair wallet.json -o account.json

# Invoke the program
agave program invoke program_id \
  --keypair wallet.json \
  --account account.json \
  --data "68656c6c6f" # "hello" in hex
```

## Validator Operation

### Setting Up a Validator

1. Generate validator identity:
   ```bash
   agave-keygen new -o validator-keypair.json
   ```

2. Generate vote account:
   ```bash
   agave-keygen new -o vote-account-keypair.json
   ```

3. Create vote account:
   ```bash
   agave create-vote-account \
     vote-account-keypair.json \
     validator-keypair.json \
     --commission 10
   ```

4. Start the validator:
   ```bash
   agave-validator \
     --identity validator-keypair.json \
     --vote-account vote-account-keypair.json \
     --ledger /path/to/ledger \
     --rpc-port 8899 \
     --entrypoint entrypoint.mainnet-beta.solana.com:8001 \
     --expected-genesis-hash <GENESIS_HASH> \
     --limit-ledger-size
   ```

### Monitoring a Validator

```bash
# Check validator status
agave validators

# Check vote account
agave vote-account vote-account-keypair.json

# Monitor gossip network
agave gossip
```

## Token Management

### Creating and Managing Tokens

1. Create a token:
   ```bash
   agave spl-token create-token --keypair wallet.json
   ```

2. Create a token account:
   ```bash
   agave spl-token create-account token_address --keypair wallet.json
   ```

3. Mint tokens:
   ```bash
   agave spl-token mint token_address 1000 token_account --keypair wallet.json
   ```

4. Transfer tokens:
   ```bash
   agave spl-token transfer token_address 100 recipient_token_account \
     --keypair wallet.json \
     --from token_account
   ```

## Staking and Delegation

### Staking SOL

1. Create a stake account:
   ```bash
   agave create-stake-account \
     --from wallet.json \
     stake-account.json \
     1 # Amount in SOL
   ```

2. Delegate stake:
   ```bash
   agave delegate-stake \
     stake-account.json \
     vote_account_address \
     --keypair wallet.json
   ```

3. Check stake status:
   ```bash
   agave stake-account stake-account.json
   ```

4. Deactivate stake:
   ```bash
   agave deactivate-stake \
     stake-account.json \
     --keypair wallet.json
   ```

5. Withdraw stake:
   ```bash
   agave withdraw-stake \
     stake-account.json \
     wallet.json \
     1 \
     --keypair wallet.json
   ```

## Advanced Examples

### Cross-Program Invocation

```rust
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    program::invoke,
    pubkey::Pubkey,
    system_instruction,
};

entrypoint!(process_instruction);

fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let accounts_iter = &mut accounts.iter();
    let payer = next_account_info(accounts_iter)?;
    let recipient = next_account_info(accounts_iter)?;
    let system_program = next_account_info(accounts_iter)?;
    
    // Create a transfer instruction
    let transfer_instruction = system_instruction::transfer(
        payer.key,
        recipient.key,
        10000000, // 0.01 SOL in lamports
    );
    
    // Execute the transfer instruction via CPI
    invoke(
        &transfer_instruction,
        &[payer.clone(), recipient.clone(), system_program.clone()],
    )?;
    
    Ok(())
}
```

### Program Derived Addresses (PDAs)

```rust
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    program::{invoke_signed},
    pubkey::Pubkey,
    system_instruction,
};

entrypoint!(process_instruction);

fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let accounts_iter = &mut accounts.iter();
    let payer = next_account_info(accounts_iter)?;
    let pda_account = next_account_info(accounts_iter)?;
    let system_program = next_account_info(accounts_iter)?;
    
    // Find PDA
    let (pda, bump_seed) = Pubkey::find_program_address(
        &[b"seed"],
        program_id,
    );
    
    // Create PDA account
    let create_account_instruction = system_instruction::create_account(
        payer.key,
        &pda,
        10000000, // lamports
        100, // space
        program_id,
    );
    
    // Execute with signer seeds
    invoke_signed(
        &create_account_instruction,
        &[payer.clone(), pda_account.clone(), system_program.clone()],
        &[&[b"seed", &[bump_seed]]],
    )?;
    
    Ok(())
}
```

### Subscribing to Account Changes

```javascript
const web3 = require('@solana/web3.js');
const connection = new web3.Connection('http://localhost:8899');

// Subscribe to account changes
const subscriptionId = connection.onAccountChange(
  new web3.PublicKey('account_address'),
  (accountInfo, context) => {
    console.log('Account updated:', accountInfo);
    console.log('Slot:', context.slot);
  },
  'confirmed'
);

// Later, unsubscribe
connection.removeAccountChangeListener(subscriptionId);
```

### Transaction with Multiple Instructions

```javascript
const web3 = require('@solana/web3.js');
const splToken = require('@solana/spl-token');

async function executeMultipleInstructions() {
  const connection = new web3.Connection('http://localhost:8899');
  const payer = web3.Keypair.fromSecretKey(/* your secret key */);
  
  // Create a new token
  const mint = await splToken.createMint(
    connection,
    payer,
    payer.publicKey,
    null,
    9
  );
  
  // Create a token account
  const tokenAccount = await splToken.getOrCreateAssociatedTokenAccount(
    connection,
    payer,
    mint,
    payer.publicKey
  );
  
  // Mint some tokens
  const mintInstruction = splToken.createMintToInstruction(
    mint,
    tokenAccount.address,
    payer.publicKey,
    1000000000 // 1 token with 9 decimals
  );
  
  // Transfer SOL in the same transaction
  const transferInstruction = web3.SystemProgram.transfer({
    fromPubkey: payer.publicKey,
    toPubkey: new web3.PublicKey('recipient_address'),
    lamports: 10000000 // 0.01 SOL
  });
  
  // Create and send transaction with both instructions
  const transaction = new web3.Transaction().add(mintInstruction, transferInstruction);
  const signature = await web3.sendAndConfirmTransaction(connection, transaction, [payer]);
  console.log('Transaction signature:', signature);
}
```

## Troubleshooting

### Common Issues and Solutions

#### Transaction Simulation Failed

```
Error: Transaction simulation failed: Error processing Instruction 0: custom program error: 0x1
```

**Solution**: This error indicates a program-specific error. Check the program's error codes and ensure you're providing the correct accounts and data.

#### Insufficient Funds

```
Error: Attempt to debit an account but found no record of a prior credit.
```

**Solution**: Ensure your account has sufficient SOL. You can request an airdrop in development:

```bash
agave airdrop 1 --keypair wallet.json
```

#### Account Already Exists

```
Error: Account already exists
```

**Solution**: When creating an account, ensure you're using a new keypair or a PDA that hasn't been created yet.

#### RPC Connection Issues

```
Error: Failed to fetch RPC response
```

**Solution**: Check your network connection and ensure the RPC endpoint is correct and accessible:

```bash
# Test RPC connection
curl -X POST -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' http://localhost:8899
```

## Further Resources

- [Official Documentation](https://docs.anza.xyz)
- [API Reference](https://docs.anza.xyz/api)
- [GitHub Repository](https://github.com/anza-xyz/agave)
- [Developer Discord](https://discord.gg/anza)