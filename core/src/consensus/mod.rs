// Copyright (c) 2026 Richard Patterson
// GitHub: @De-ASI-INTERFACE | DeASI-INTERFACE · QuantumTradingInfinity · richy.ai
// SPDX-License-Identifier: Apache-2.0
//
// Consensus module root for Alpenglow upgrade scaffold.
// Votor and Rotor are independently activatable per SIMD-0326 governance.

pub mod alpenglow;
pub mod entrypoint;
pub mod rotor;
pub mod votor;

pub use alpenglow::{AlpenglowConfig, AlpenglowConsensus, FinalityOutcome};
pub use rotor::{RelayMessage, Rotor, RotorConfig};
pub use votor::{FinalityPath, VotePacket, Votor, VotorConfig, VotorDecision};
