# Agave Blockchain Platform Knowledge Graph

This document provides a comprehensive knowledge graph of the Agave blockchain platform, showing the relationships between different components and their roles in the system.

## System Architecture

```mermaid
graph TD
    A[Client Applications] -->|Interact with| B[RPC API]
    A -->|Command Line| C[CLI]
    C -->|Uses| B
    
    B -->|Communicates with| D[Validator]
    
    D -->|Uses| E[Core]
    D -->|Uses| F[Runtime]
    D -->|Uses| G[SVM]
    D -->|Uses| H[Ledger]
    D -->|Uses| I[Programs]
    
    E -->|Banking Stage| F
    E -->|TPU| J[Transaction Processing]
    E -->|TVU| K[Block Validation]
    E -->|Gossip| L[Peer Discovery]
    
    F -->|Executes| G
    F -->|Updates| M[Accounts DB]
    
    G -->|Loads| I
    G -->|Modifies| M
    
    H -->|Stores| N[Blocks]
    H -->|Stores| O[Transactions]
    H -->|Creates| P[Snapshots]
    
    I -->|System Program| Q[Account Management]
    I -->|Stake Program| R[Staking]
    I -->|Vote Program| S[Consensus]
    I -->|BPF Loader| T[Program Deployment]
    
    M -->|Persisted in| H
```

## Component Relationships

### Client Interaction Layer
- **Client Applications**: External applications that interact with the blockchain
- **RPC API**: Provides methods for clients to interact with the blockchain
- **CLI**: Command-line tools for interacting with the blockchain

### Validator Node
- **Validator**: The main node implementation that participates in consensus
- **Core**: Central component handling consensus, networking, and transaction processing
- **Runtime**: Executes transactions and manages blockchain state
- **SVM**: Executes smart contracts (programs)
- **Ledger**: Stores and manages blockchain data
- **Programs**: Built-in smart contracts providing core functionality

### Core Components
- **Banking Stage**: Processes and validates transactions
- **TPU (Transaction Processing Unit)**: Receives and processes transactions
- **TVU (Transaction Validation Unit)**: Validates blocks
- **Gossip**: Handles peer discovery and communication

### State Management
- **Accounts DB**: Manages the state of all accounts
- **Runtime**: Updates account states based on transaction execution
- **SVM**: Executes program instructions that modify account states

### Data Storage
- **Ledger**: Stores blocks, transactions, and account states
- **Blocks**: Contains validated transactions
- **Transactions**: Contains instructions to be executed
- **Snapshots**: Point-in-time captures of the blockchain state

### Built-in Programs
- **System Program**: Manages accounts and transfers
- **Stake Program**: Manages staking and delegation
- **Vote Program**: Manages consensus voting
- **BPF Loader**: Deploys and executes user programs

## Data Flow

```mermaid
sequenceDiagram
    participant Client
    participant RPC
    participant Validator
    participant Core
    participant Runtime
    participant SVM
    participant Programs
    participant Ledger
    
    Client->>RPC: Submit Transaction
    RPC->>Validator: Forward Transaction
    Validator->>Core: Process Transaction
    Core->>Runtime: Execute Transaction
    Runtime->>SVM: Execute Program Instructions
    SVM->>Programs: Load and Execute Program
    Programs->>SVM: Return Execution Results
    SVM->>Runtime: Return State Changes
    Runtime->>Core: Return Execution Results
    Core->>Ledger: Store Transaction and State Changes
    Validator->>RPC: Return Transaction Status
    RPC->>Client: Return Result
```

## Consensus Flow

```mermaid
sequenceDiagram
    participant Leader
    participant Validator1
    participant Validator2
    participant Validator3
    
    Leader->>Leader: Create Block
    Leader->>Validator1: Broadcast Block
    Leader->>Validator2: Broadcast Block
    Leader->>Validator3: Broadcast Block
    
    Validator1->>Validator1: Validate Block
    Validator2->>Validator2: Validate Block
    Validator3->>Validator3: Validate Block
    
    Validator1->>Leader: Send Vote
    Validator2->>Leader: Send Vote
    Validator3->>Leader: Send Vote
    
    Leader->>Leader: Confirm Block
```

## Transaction Lifecycle

```mermaid
stateDiagram-v2
    [*] --> Submitted: Client submits transaction
    Submitted --> Received: Validator receives transaction
    Received --> Processed: Transaction processed by Banking Stage
    Processed --> Executed: Transaction executed by Runtime
    Executed --> Confirmed: Transaction included in block
    Confirmed --> Finalized: Block finalized by consensus
    Finalized --> [*]
```

## Account Structure

```mermaid
classDiagram
    class Account {
        +PublicKey address
        +u64 lamports
        +u64 data_len
        +PublicKey owner
        +bool executable
        +u64 rent_epoch
        +bytes data
    }
    
    class Transaction {
        +Signature[] signatures
        +Message message
    }
    
    class Message {
        +AccountKey[] account_keys
        +Hash recent_blockhash
        +Instruction[] instructions
    }
    
    class Instruction {
        +u8 program_id_index
        +u8[] accounts
        +bytes data
    }
    
    Transaction --> Message
    Message --> Instruction
    Instruction --> Account
```

## Further Reading

For more detailed information about the Agave blockchain platform, refer to the README.md files in each component directory:

- [Core README](core/README.md)
- [Validator README](validator/README.md)
- [SVM README](svm/README.md)
- [Runtime README](runtime/README.md)
- [Programs README](programs/README.md)
- [Ledger README](ledger/README.md)
- [RPC README](rpc/README.md)
- [CLI README](cli/README.md)