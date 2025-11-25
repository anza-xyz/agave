# Solana Remote Wallet

A Rust library for interacting with hardware wallets in the Solana ecosystem. This library provides a unified interface for discovering, connecting to, and performing operations with various hardware wallet devices.

## Features

- **Multi-Wallet Support**: Currently supports Ledger hardware wallets, with extensible architecture for additional wallet types
- **USB HID Communication**: Secure communication with hardware wallets via USB HID protocol
- **Device Discovery**: Automatic detection and connection to connected hardware wallets
- **Transaction Signing**: Sign Solana transactions and messages using hardware wallet private keys
- **Public Key Derivation**: Derive Solana public keys from hardware wallet derivation paths
- **Cross-Platform**: Works on Linux, macOS, and Windows
- **Thread-Safe**: Concurrent access to wallet manager with proper synchronization

## Supported Hardware Wallets

### Ledger
- **Protocol**: USB HID with APDU commands
- **Features**: Transaction signing, message signing, public key derivation
- **Ledger udev-rules**: In order to use a Ledger device on Linux machines, users must apply certain udev rules. These are available at the [udev-rules repository](https://github.com/LedgerHQ/udev-rules) maintained by the Ledger team.

### Keystone (Planned/In Development)
- Support for Keystone hardware wallets is being added

## Architecture

The library is organized into several key modules:

```
remote-wallet/
├── src/
│   ├── lib.rs                 # Main library entry point
│   ├── errors.rs              # Error types and conversions
│   ├── remote_wallet.rs       # Core wallet manager and traits
│   ├── remote_keypair.rs      # Remote keypair implementation
│   ├── locator.rs             # Device locator and URI parsing
│   ├── wallet/                # Wallet implementations
│   │   ├── mod.rs             # Wallet module definitions
│   │   ├── types.rs           # Common wallet types
│   │   ├── errors.rs          # Wallet-specific errors
│   │   └── ledger/            # Ledger wallet implementation
│   │       ├── mod.rs         # Ledger module
│   │       ├── ledger.rs      # Ledger wallet logic
│   │       └── error.rs       # Ledger-specific errors
│   └── transport/             # Communication layer
│       ├── mod.rs             # Transport module
│       ├── transport_trait.rs # Transport trait definition
│       ├── hid_transport.rs   # HID transport implementation
│       └── common.rs          # Common transport utilities
```

### Core Components

#### RemoteWalletManager
The main entry point for hardware wallet operations. It manages:
- Device discovery and connection
- Device lifecycle management
- Concurrent access to multiple devices

#### RemoteWallet Trait
Defines the interface that all hardware wallet implementations must provide:
- Device initialization and information reading
- Public key derivation
- Transaction and message signing

#### WalletProbe Trait
Handles device discovery and connection for specific wallet types:
- Device identification
- Connection establishment
- Device validation

#### Transport Layer
Abstracts the communication protocol:
- USB HID transport for Ledger devices
- Extensible for other communication methods

### Device Locators

Hardware wallets are identified using URI-style locators:
```
usb://ledger
usb://ledger?key=0
```

### Security Considerations

- **Private Keys**: Private keys never leave the hardware wallet
- **User Confirmation**: Critical operations require physical confirmation on the device
- **Secure Communication**: All communication uses encrypted USB HID protocol
- **Input Validation**: All inputs are validated before being sent to the device

## Development

### Building

```bash
# Build the library
cargo build

# Build with specific features
cargo build --features "hidapi,linux-static-hidraw"

# Run tests
cargo test

# Build documentation
cargo doc --open
```

### Adding New Wallet Types

To add support for a new hardware wallet:

1. Create a new module in `src/wallet/`
2. Implement the `RemoteWallet` trait
3. Implement the `WalletProbe` trait
4. Add the wallet type to `RemoteWalletType` enum
5. Register the probe in `create_wallet_probes()`
6. Update error handling and keypair generation

Example structure for a new wallet:

```rust
// src/wallet/new_wallet/mod.rs
pub mod wallet;
pub mod probe;
pub mod error;

// src/wallet/new_wallet/wallet.rs
pub struct NewWallet {
    // Implementation
}

impl RemoteWallet<hidapi::DeviceInfo> for NewWallet {
    // Implement required methods
}

// src/wallet/new_wallet/probe.rs
pub struct NewWalletProbe;

impl WalletProbe for NewWalletProbe {
    // Implement device discovery
}
```
