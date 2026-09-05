/// Module responsible for notifying plugins of transactions
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::{
        geyser_plugin_interface::{ReplicaTransactionInfoV4, ReplicaTransactionInfoVersions},
        transaction_status_meta as mirror,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_clock::{BankId, Slot},
    solana_hash::Hash,
    solana_rpc::transaction_notifier_interface::TransactionNotifier,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_status::{
        InnerInstruction, Reward, TransactionStatusMeta, TransactionTokenBalance,
    },
    std::sync::Arc,
};

/// This implementation of TransactionNotifier is passed to the rpc's TransactionStatusService
/// at the validator startup. TransactionStatusService invokes the notify_transaction method
/// for new transactions. The implementation in turn invokes the notify_transaction of each
/// plugin enabled with transaction notification managed by the GeyserPluginManager.
pub(crate) struct TransactionNotifierImpl {
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
}

impl TransactionNotifier for TransactionNotifierImpl {
    fn notify_transaction(
        &self,
        slot: Slot,
        bank_id: BankId,
        index: usize,
        signature: &Signature,
        message_hash: &Hash,
        is_vote: bool,
        transaction_status_meta: &TransactionStatusMeta,
        transaction: &VersionedTransaction,
    ) {
        let plugin_manager = self.plugin_manager.load();

        if plugin_manager.plugins.is_empty() {
            return;
        }

        // Destructured exhaustively: adding a field to the internal type must
        // break this conversion at compile time, not silently drop data on
        // the plugin boundary.
        let TransactionStatusMeta {
            status,
            fee,
            pre_balances,
            post_balances,
            inner_instructions,
            log_messages,
            pre_token_balances,
            post_token_balances,
            rewards,
            loaded_addresses,
            return_data,
            compute_units_consumed,
            cost_units,
        } = transaction_status_meta;

        // Scratch buffers owned by this call; only slices into them cross
        // the plugin boundary.
        let inner_instruction_rows: Option<Vec<Vec<mirror::InnerInstruction>>> =
            inner_instructions.as_ref().map(|list| {
                list.iter()
                    .map(|inner| {
                        inner
                            .instructions
                            .iter()
                            .map(convert_inner_instruction)
                            .collect()
                    })
                    .collect()
            });
        let inner_instructions_scratch: Option<Vec<mirror::InnerInstructions>> =
            inner_instruction_rows.as_ref().map(|rows| {
                inner_instructions
                    .as_ref()
                    .expect("rows are built from inner_instructions")
                    .iter()
                    .zip(rows)
                    .map(|(inner, instructions)| mirror::InnerInstructions {
                        index: inner.index,
                        instructions,
                    })
                    .collect()
            });
        let log_messages_scratch: Option<Vec<&str>> = log_messages
            .as_ref()
            .map(|logs| logs.iter().map(String::as_str).collect());
        let pre_token_balances_scratch: Option<Vec<mirror::TransactionTokenBalance>> =
            pre_token_balances
                .as_ref()
                .map(|balances| balances.iter().map(convert_token_balance).collect());
        let post_token_balances_scratch: Option<Vec<mirror::TransactionTokenBalance>> =
            post_token_balances
                .as_ref()
                .map(|balances| balances.iter().map(convert_token_balance).collect());
        let rewards_scratch: Option<Vec<mirror::Reward>> = rewards
            .as_ref()
            .map(|rewards| rewards.iter().map(convert_reward).collect());

        let transaction_status_meta = mirror::TransactionStatusMeta {
            status: match status {
                Ok(()) => Ok(()),
                Err(err) => Err(err),
            },
            fee: *fee,
            pre_balances,
            post_balances,
            inner_instructions: inner_instructions_scratch.as_deref(),
            log_messages: log_messages_scratch.as_deref(),
            pre_token_balances: pre_token_balances_scratch.as_deref(),
            post_token_balances: post_token_balances_scratch.as_deref(),
            rewards: rewards_scratch.as_deref(),
            loaded_addresses,
            return_data: return_data
                .as_ref()
                .map(|data| mirror::TransactionReturnData {
                    program_id: data.program_id,
                    data: &data.data,
                }),
            compute_units_consumed: *compute_units_consumed,
            cost_units: *cost_units,
        };

        let transaction_log_info = ReplicaTransactionInfoV4 {
            index,
            message_hash,
            signature,
            is_vote,
            transaction,
            transaction_status_meta: &transaction_status_meta,
        };

        for plugin in plugin_manager.plugins.iter() {
            if !plugin.transaction_notifications_enabled() {
                continue;
            }
            match plugin.notify_transaction_for_bank(
                ReplicaTransactionInfoVersions::V0_0_4(&transaction_log_info),
                slot,
                bank_id,
            ) {
                Err(err) => {
                    error!(
                        "Failed to notify transaction, error: ({}) to plugin {}",
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully notified transaction to plugin {}",
                        plugin.name()
                    );
                }
            }
        }
    }
}

impl TransactionNotifierImpl {
    pub fn new(plugin_manager: Arc<ArcSwap<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }
}

fn convert_inner_instruction(inner: &InnerInstruction) -> mirror::InnerInstruction<'_> {
    let InnerInstruction {
        instruction,
        stack_height,
    } = inner;
    mirror::InnerInstruction {
        instruction,
        stack_height: *stack_height,
    }
}

fn convert_token_balance(balance: &TransactionTokenBalance) -> mirror::TransactionTokenBalance<'_> {
    let TransactionTokenBalance {
        account_index,
        mint,
        ui_token_amount,
        owner,
        program_id,
    } = balance;
    mirror::TransactionTokenBalance {
        account_index: *account_index,
        mint,
        ui_token_amount: mirror::UiTokenAmount {
            ui_amount: ui_token_amount.ui_amount,
            decimals: ui_token_amount.decimals,
            amount: &ui_token_amount.amount,
            ui_amount_string: &ui_token_amount.ui_amount_string,
        },
        owner,
        program_id,
    }
}

fn convert_reward(reward: &Reward) -> mirror::Reward<'_> {
    let Reward {
        pubkey,
        lamports,
        post_balance,
        reward_type,
        commission,
        commission_bps,
    } = reward;
    mirror::Reward {
        pubkey,
        lamports: *lamports,
        post_balance: *post_balance,
        reward_type: *reward_type,
        commission: *commission,
        commission_bps: *commission_bps,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::geyser_plugin_manager::{GeyserPluginManager, LoadedGeyserPlugin},
        agave_geyser_plugin_interface::geyser_plugin_interface::{GeyserPlugin, Result},
        libloading::Library,
        solana_account_decoder_client_types::token::UiTokenAmount,
        solana_message::{compiled_instruction::CompiledInstruction, v0::LoadedAddresses},
        solana_pubkey::Pubkey,
        solana_reward_info::RewardType,
        solana_transaction_context::transaction::TransactionReturnData,
        solana_transaction_error::TransactionError,
        solana_transaction_status::InnerInstructions,
        std::sync::Mutex,
    };

    #[test]
    fn test_convert_inner_instruction() {
        let instruction = CompiledInstruction {
            program_id_index: 2,
            accounts: vec![0, 1],
            data: vec![9, 9],
        };
        let source = InnerInstruction {
            instruction: instruction.clone(),
            stack_height: Some(2),
        };
        let converted = convert_inner_instruction(&source);
        assert_eq!(*converted.instruction, instruction);
        assert_eq!(converted.stack_height, Some(2));
    }

    #[test]
    fn test_convert_token_balance() {
        let source = TransactionTokenBalance {
            account_index: 1,
            mint: "mint".to_string(),
            ui_token_amount: UiTokenAmount {
                ui_amount: Some(1.5),
                decimals: 2,
                amount: "150".to_string(),
                ui_amount_string: "1.5".to_string(),
            },
            owner: "owner".to_string(),
            program_id: "program".to_string(),
        };
        let converted = convert_token_balance(&source);
        assert_eq!(converted.account_index, 1);
        assert_eq!(converted.mint, "mint");
        assert_eq!(converted.ui_token_amount.ui_amount, Some(1.5));
        assert_eq!(converted.ui_token_amount.decimals, 2);
        assert_eq!(converted.ui_token_amount.amount, "150");
        assert_eq!(converted.ui_token_amount.ui_amount_string, "1.5");
        assert_eq!(converted.owner, "owner");
        assert_eq!(converted.program_id, "program");
    }

    #[test]
    fn test_convert_reward() {
        let source = Reward {
            pubkey: "rewardee".to_string(),
            lamports: -1,
            post_balance: 7,
            reward_type: Some(RewardType::Rent),
            commission: None,
            commission_bps: Some(300),
        };
        let converted = convert_reward(&source);
        assert_eq!(converted.pubkey, "rewardee");
        assert_eq!(converted.lamports, -1);
        assert_eq!(converted.post_balance, 7);
        assert_eq!(converted.reward_type, Some(RewardType::Rent));
        assert_eq!(converted.commission, None);
        assert_eq!(converted.commission_bps, Some(300));
    }

    #[derive(Debug)]
    struct TxCapturePlugin {
        enabled: bool,
        captured: Arc<Mutex<Vec<String>>>,
    }

    impl GeyserPlugin for TxCapturePlugin {
        fn name(&self) -> &'static str {
            "tx-capture-plugin"
        }

        fn transaction_notifications_enabled(&self) -> bool {
            self.enabled
        }

        fn notify_transaction_for_bank(
            &self,
            transaction_info: ReplicaTransactionInfoVersions,
            slot: Slot,
            bank_id: BankId,
        ) -> Result<()> {
            let ReplicaTransactionInfoVersions::V0_0_4(info) = transaction_info;
            self.captured.lock().unwrap().push(format!(
                "{:?}",
                (
                    slot,
                    bank_id,
                    info.index,
                    info.is_vote,
                    info.transaction_status_meta
                )
            ));
            Ok(())
        }
    }

    fn loaded_tx_plugin(plugin: TxCapturePlugin) -> Arc<LoadedGeyserPlugin> {
        #[cfg(unix)]
        let library = libloading::os::unix::Library::this();
        #[cfg(windows)]
        let library = libloading::os::windows::Library::this().unwrap();
        Arc::new(LoadedGeyserPlugin::new(
            Library::from(library),
            Box::new(plugin),
            None,
        ))
    }

    #[test]
    fn test_notify_transaction_end_to_end() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let ignored = Arc::new(Mutex::new(Vec::new()));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![
                loaded_tx_plugin(TxCapturePlugin {
                    enabled: true,
                    captured: captured.clone(),
                }),
                loaded_tx_plugin(TxCapturePlugin {
                    enabled: false,
                    captured: ignored.clone(),
                }),
            ],
        })));
        let notifier = TransactionNotifierImpl::new(plugin_manager);

        let instruction = CompiledInstruction {
            program_id_index: 2,
            accounts: vec![0, 1],
            data: vec![9, 9],
        };
        let return_program_id = Pubkey::new_unique();
        let meta = TransactionStatusMeta {
            status: Err(TransactionError::AccountInUse),
            fee: 5000,
            pre_balances: vec![10, 20],
            post_balances: vec![5, 25],
            inner_instructions: Some(vec![InnerInstructions {
                index: 0,
                instructions: vec![InnerInstruction {
                    instruction: instruction.clone(),
                    stack_height: Some(2),
                }],
            }]),
            log_messages: Some(vec!["log one".to_string()]),
            pre_token_balances: Some(vec![TransactionTokenBalance {
                account_index: 1,
                mint: "mint".to_string(),
                ui_token_amount: UiTokenAmount {
                    ui_amount: Some(1.5),
                    decimals: 2,
                    amount: "150".to_string(),
                    ui_amount_string: "1.5".to_string(),
                },
                owner: "owner".to_string(),
                program_id: "program".to_string(),
            }]),
            post_token_balances: Some(vec![]),
            rewards: Some(vec![Reward {
                pubkey: "rewardee".to_string(),
                lamports: -1,
                post_balance: 7,
                reward_type: Some(RewardType::Rent),
                commission: None,
                commission_bps: Some(300),
            }]),
            loaded_addresses: LoadedAddresses {
                writable: vec![],
                readonly: vec![],
            },
            return_data: Some(TransactionReturnData {
                program_id: return_program_id,
                data: vec![1, 2, 3],
            }),
            compute_units_consumed: Some(42),
            cost_units: Some(7),
        };
        let transaction = VersionedTransaction::default();

        notifier.notify_transaction(
            42,
            9,
            3,
            &Signature::default(),
            &Hash::default(),
            false,
            &meta,
            &transaction,
        );

        // Build the expected mirror view the same way the conversion does.
        let inner_scratch = vec![mirror::InnerInstruction {
            instruction: &instruction,
            stack_height: Some(2),
        }];
        let inner_rows = vec![mirror::InnerInstructions {
            index: 0,
            instructions: &inner_scratch,
        }];
        let logs = ["log one"];
        let pre_tb = vec![mirror::TransactionTokenBalance {
            account_index: 1,
            mint: "mint",
            ui_token_amount: mirror::UiTokenAmount {
                ui_amount: Some(1.5),
                decimals: 2,
                amount: "150",
                ui_amount_string: "1.5",
            },
            owner: "owner",
            program_id: "program",
        }];
        let post_tb: Vec<mirror::TransactionTokenBalance> = vec![];
        let rewards = vec![mirror::Reward {
            pubkey: "rewardee",
            lamports: -1,
            post_balance: 7,
            reward_type: Some(RewardType::Rent),
            commission: None,
            commission_bps: Some(300),
        }];
        let loaded = LoadedAddresses {
            writable: vec![],
            readonly: vec![],
        };
        let expected_meta = mirror::TransactionStatusMeta {
            status: Err(&TransactionError::AccountInUse),
            fee: 5000,
            pre_balances: &[10, 20],
            post_balances: &[5, 25],
            inner_instructions: Some(&inner_rows),
            log_messages: Some(&logs),
            pre_token_balances: Some(&pre_tb),
            post_token_balances: Some(&post_tb),
            rewards: Some(&rewards),
            loaded_addresses: &loaded,
            return_data: Some(mirror::TransactionReturnData {
                program_id: return_program_id,
                data: &[1, 2, 3],
            }),
            compute_units_consumed: Some(42),
            cost_units: Some(7),
        };
        let expected = format!("{:?}", (42u64, 9u64, 3usize, false, &expected_meta));

        assert_eq!(*captured.lock().unwrap(), vec![expected]);
        assert!(ignored.lock().unwrap().is_empty());

        // A successful, minimal meta: the Ok status arm and the None branches.
        captured.lock().unwrap().clear();
        let ok_meta = TransactionStatusMeta {
            status: Ok(()),
            fee: 1,
            pre_balances: vec![],
            post_balances: vec![],
            inner_instructions: None,
            log_messages: None,
            pre_token_balances: None,
            post_token_balances: None,
            rewards: None,
            loaded_addresses: LoadedAddresses {
                writable: vec![],
                readonly: vec![],
            },
            return_data: None,
            compute_units_consumed: None,
            cost_units: None,
        };
        notifier.notify_transaction(
            43,
            9,
            0,
            &Signature::default(),
            &Hash::default(),
            true,
            &ok_meta,
            &transaction,
        );
        let expected_ok_meta = mirror::TransactionStatusMeta {
            status: Ok(()),
            fee: 1,
            pre_balances: &[],
            post_balances: &[],
            inner_instructions: None,
            log_messages: None,
            pre_token_balances: None,
            post_token_balances: None,
            rewards: None,
            loaded_addresses: &loaded,
            return_data: None,
            compute_units_consumed: None,
            cost_units: None,
        };
        let expected_ok = format!("{:?}", (43u64, 9u64, 0usize, true, &expected_ok_meta));
        assert_eq!(*captured.lock().unwrap(), vec![expected_ok]);
    }
}
