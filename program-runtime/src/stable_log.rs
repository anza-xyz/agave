//! Stable program log messages
//!
//! The format of these log messages should not be modified to avoid breaking downstream consumers
//! of program logging

use base64::encoded_len;
use {
    base64::{prelude::BASE64_STANDARD, Engine},
    solana_log_collector::{ic_logger_msg, LogCollector},
    solana_pubkey::Pubkey,
    std::{cell::RefCell, rc::Rc},
};

/// Log a program invoke.
///
/// The general form is:
///
/// ```notrust
/// "Program <address> invoke [<depth>]"
/// ```
pub fn program_invoke(
    log_collector: &Option<Rc<RefCell<LogCollector>>>,
    program_id: &Pubkey,
    invoke_depth: usize,
) {
    //Potentially use itoa for invoke_depth, but is currently not an included dep.
    ic_logger_msg!(log_collector, &["Program ",  &program_id.to_string(), " invoke [", &invoke_depth.to_string(), "]"].join(""));
}

/// Log a message from the program itself.
///
/// The general form is:
///
/// ```notrust
/// "Program log: <program-generated output>"
/// ```
///
/// That is, any program-generated output is guaranteed to be prefixed by "Program log: "
pub fn program_log(log_collector: &Option<Rc<RefCell<LogCollector>>>, message: &str) {
    ic_logger_msg!(log_collector, &["Program log: ", message].join(""));
}

/// Emit a program data.
///
/// The general form is:
///
/// ```notrust
/// "Program data: <binary-data-in-base64>*"
/// ```
///
/// That is, any program-generated output is guaranteed to be prefixed by "Program data: "
/// Emits program data in a consistent format with length limits. The data is output in this format:
/// "Program data: <base64-data> <base64-data>..." where each piece of data is base64 encoded and
/// space-separated.
///
/// Handles memory/length limits by:
/// - Calculates remaining bytes based on LogCollector's limit
/// - Pre-allocates string capacity to reduce reallocations
pub fn program_data(log_collector: &Option<Rc<RefCell<LogCollector>>>, data: &[&[u8]]) {
    // Calculate remaining bytes available for logging
    let available_bytes = log_collector
        .as_ref()
        .map(|collector| {
            let borrowed = collector.borrow();
            borrowed.bytes_limit
                .map_or(usize::MAX, |limit| limit.saturating_sub(borrowed.bytes_written))
        })
        .unwrap_or(usize::MAX);

    // Pre-calculate capacity including prefix and estimated base64 encoding size
    let prefix_len = 13usize; // "Program data: " length
    let estimated_capacity = data.iter()
        .map(|v| encoded_len(v.len(), true).unwrap()) // Get base64 length with padding
        .try_fold(prefix_len, |acc: usize, x| {
            acc.checked_add(x).filter(|&sum| sum <= available_bytes) // Stop if exceeds limit
        })
        .unwrap_or(available_bytes);

    // Initialize result with estimated capacity to avoid reallocations
    let mut result = String::with_capacity(estimated_capacity);
    result.push_str("Program data: ");

    // Process each data chunk, truncating if needed to stay within byte limit
    for (i, &v) in data.iter().enumerate() {
        // If we run out of space in the log_collector, no need to keep processing so break out
        // It's possible to truncate this data to fill the remaining space of the log_collector,
        // but this is keeping in line with previous functionality where this specific log data entry
        // will be converted to "Log truncated" and added to the logs, rather than any encoded data.
        if available_bytes <= result.len() {
            break;
        }
        if i > 0 {
            result.push(' ');
        }

        BASE64_STANDARD.encode_string(v, &mut result);
    }

    ic_logger_msg!(log_collector, &result);
}

/// Log return data as from the program itself. This line will not be present if no return
/// data was set, or if the return data was set to zero length.
///
/// The general form is:
///
/// ```notrust
/// "Program return: <program-id> <program-generated-data-in-base64>"
/// ```
///
/// That is, any program-generated output is guaranteed to be prefixed by "Program return: "
pub fn program_return(
    log_collector: &Option<Rc<RefCell<LogCollector>>>,
    program_id: &Pubkey,
    data: &[u8],
) {
    ic_logger_msg!(log_collector, &["Program return: ", &program_id.to_string(), " ", &BASE64_STANDARD.encode(data)].join(""));
}

/// Log successful program execution.
///
/// The general form is:
///
/// ```notrust
/// "Program <address> success"
/// ```
pub fn program_success(log_collector: &Option<Rc<RefCell<LogCollector>>>, program_id: &Pubkey) {
    ic_logger_msg!(log_collector, &["Program ",  &program_id.to_string(), " success"].join(""));
}

/// Log program execution failure
///
/// The general form is:
///
/// ```notrust
/// "Program <address> failed: <program error details>"
/// ```
pub fn program_failure<E: std::fmt::Display>(
    log_collector: &Option<Rc<RefCell<LogCollector>>>,
    program_id: &Pubkey,
    err: &E,
) {
    ic_logger_msg!(log_collector, &["Program ",  &program_id.to_string(), " failed: ", &err.to_string()].join(""));
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::prelude::BASE64_STANDARD;
    use solana_sdk::instruction::InstructionError;

    // Helper function to get messages from log collector
    fn get_messages(log_collector: &Option<Rc<RefCell<LogCollector>>>) -> Vec<String> {
        log_collector
            .as_ref()
            .map(|collector| collector.borrow().messages.clone())
            .unwrap_or_default()
    }

    #[test]
    fn test_program_invoke() {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let invoke_depth = 3;

        program_invoke(&log_collector, &program_id, invoke_depth);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            format!("Program {} invoke [{}]", program_id, invoke_depth)
        );
    }

    #[test]
    fn test_program_invoke_no_collector() {
        let program_id = Pubkey::new_unique();
        // Should not panic when log_collector is None
        program_invoke(&None, &program_id, 1);
    }

    #[test]
    fn test_program_log() {
        let log_collector = Some(LogCollector::new_ref());
        let message = "Test message";

        program_log(&log_collector, message);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], format!("Program log: {}", message));
    }

    #[test]
    fn test_program_log_empty_message() {
        let log_collector = Some(LogCollector::new_ref());
        program_log(&log_collector, "");

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], "Program log: ");
    }

    #[test]
    fn test_program_data_single_item() {
        let log_collector = Some(LogCollector::new_ref());
        let data = [b"Hello" as &[u8]];

        program_data(&log_collector, &data);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            format!("Program data: {}", BASE64_STANDARD.encode(data[0]))
        );
    }

    #[test]
    fn test_program_data_multiple_items() {
        let log_collector = Some(LogCollector::new_ref());
        let data1 = b"Hello";
        let data2 = b"World";
        let data = [data1 as &[u8], data2 as &[u8]];

        program_data(&log_collector, &data);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            format!(
                "Program data: {} {}",
                BASE64_STANDARD.encode(data1),
                BASE64_STANDARD.encode(data2)
            )
        );
    }

    #[test]
    fn test_program_data_empty() {
        let log_collector = Some(LogCollector::new_ref());
        let data: [&[u8]; 0] = [];

        program_data(&log_collector, &data);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], "Program data: ");
    }

    #[test]
    fn test_program_return() {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let data = vec![1, 2, 3, 4];

        program_return(&log_collector, &program_id, &data);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            format!(
                "Program return: {} {}",
                program_id,
                BASE64_STANDARD.encode(&data)
            )
        );
    }

    #[test]
    fn test_program_return_empty_data() {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let empty_data: Vec<u8> = vec![];

        program_return(&log_collector, &program_id, &empty_data);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            format!("Program return: {} {}", program_id, BASE64_STANDARD.encode(&empty_data))
        );
    }

    #[test]
    fn test_program_success() {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();

        program_success(&log_collector, &program_id);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], format!("Program {} success", program_id));
    }

    #[test]
    fn test_program_failure() {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let error = InstructionError::ProgramFailedToComplete;

        program_failure(&log_collector, &program_id, &error);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            format!("Program {} failed: Program failed to complete", program_id)
        );
    }

    #[test]
    fn test_program_failure_custom_error() {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();
        let error = "Custom error message";

        program_failure(&log_collector, &program_id, &error);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            format!("Program {} failed: {}", program_id, error)
        );
    }

    #[test]
    fn test_multiple_logs_sequence() {
        let log_collector = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();

        // Sequence of different log types
        program_invoke(&log_collector, &program_id, 1);
        program_log(&log_collector, "Processing");
        program_data(&log_collector, &[b"data" as &[u8]]);
        program_return(&log_collector, &program_id, b"result");
        program_success(&log_collector, &program_id);

        let messages = get_messages(&log_collector);
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0], format!("Program {} invoke [1]", program_id));
        assert_eq!(messages[1], "Program log: Processing");
        assert_eq!(messages[2], format!("Program data: {}", BASE64_STANDARD.encode(b"data")));
        assert_eq!(
            messages[3],
            format!("Program return: {} {}", program_id, BASE64_STANDARD.encode(b"result"))
        );
        assert_eq!(messages[4], format!("Program {} success", program_id));
    }

    #[test]
    fn test_concurrent_log_collectors() {
        let collector1 = Some(LogCollector::new_ref());
        let collector2 = Some(LogCollector::new_ref());
        let program_id = Pubkey::new_unique();

        program_invoke(&collector1, &program_id, 1);
        program_invoke(&collector2, &program_id, 2);

        let messages1 = get_messages(&collector1);
        let messages2 = get_messages(&collector2);

        assert_eq!(messages1.len(), 1);
        assert_eq!(messages2.len(), 1);
        assert_eq!(messages1[0], format!("Program {} invoke [1]", program_id));
        assert_eq!(messages2[0], format!("Program {} invoke [2]", program_id));
    }

    #[test]
    fn test_program_data_overflow_handling_over_log_collector_byte_limit() {
        let log_collector = Some(LogCollector::new_ref());

        let inner_data = vec![0u8; 0];
        let data = vec![&inner_data[..]; 10001];

        // To test an actual overflow condition, use higher values. For example:
        // let inner_data = vec![0u8; 14000000000];
        // let data = vec![&inner_data[..]; 1000000000];

        program_data(&log_collector, &data);
        let collector = log_collector.unwrap();
        let borrow = collector.borrow();
        let logs = borrow.get_recorded_content();

        // Assert the expected log behavior
        assert!(!logs.is_empty());
        assert!(logs[0].starts_with("Log truncated"));
    }

    #[test]
    fn test_program_data_overflow_handling_under_log_collector_byte_limit() {
        let log_collector = Some(LogCollector::new_ref());

        let inner_data = vec![0u8; 0];
        let data = vec![&inner_data[..]; 4];

        program_data(&log_collector, &data);
        let collector = log_collector.unwrap();
        let borrow = collector.borrow();
        let logs = borrow.get_recorded_content();

        assert!(!logs.is_empty());
        assert!(logs[0].starts_with("Program data: "));
    }

    #[test]
    fn test_program_data_overflow_handling_log_collector_no_limit() {
        let log_collector = Some(LogCollector::new_ref_with_limit(None));

        let inner_data = vec![0u8; 0];
        let data = vec![&inner_data[..]; 4];

        program_data(&log_collector, &data);
        let collector = log_collector.unwrap();
        let borrow = collector.borrow();
        let logs = borrow.get_recorded_content();

        assert!(!logs.is_empty());
        assert!(logs[0].starts_with("Program data: "));
    }

    #[test]
    fn test_program_data_overflow_handling_log_collector_over_custom_limit_multi_line() {
        let log_collector = Some(LogCollector::new_ref_with_limit(Some(1000)));

        program_log(&log_collector, "");

        let inner_data = vec![b'A'; 1];
        let data = vec![&inner_data[..]; 10001];

        program_data(&log_collector, &data);

        program_success(&log_collector, &Pubkey::new_unique());
        let collector = log_collector.clone().unwrap();
        let borrow = collector.borrow();
        let logs = borrow.get_recorded_content();

        assert!(logs[1].starts_with("Log truncated"));
        assert!(logs[2].starts_with("Program"));
        assert!(logs[2].ends_with("success"));
    }

    #[test]
    fn test_program_data_overflow_handling_log_collector_under_custom_limit() {
        let log_collector = Some(LogCollector::new_ref_with_limit(Some(1000)));

        program_log(&log_collector, "");

        let inner_data = vec![0u8; 0];
        let data = vec![&inner_data[..]; 973];

        program_data(&log_collector, &data);
        let collector = log_collector.unwrap();
        let borrow = collector.borrow();
        let logs = borrow.get_recorded_content();

        assert!(!logs.is_empty());
        assert!(logs[0].starts_with("Program log: "));
        assert!(logs[1].starts_with("Program data: "));
    }

    #[test]
    fn test_program_data_overflow_handling_log_collector_over_custom_limit() {
        let log_collector = Some(LogCollector::new_ref_with_limit(Some(1000)));

        program_log(&log_collector, "");

        let inner_data = vec![0u8; 0];
        let data = vec![&inner_data[..]; 974];

        program_data(&log_collector, &data);
        let collector = log_collector.unwrap();
        let borrow = collector.borrow();
        let logs = borrow.get_recorded_content();

        assert!(!logs.is_empty());
        assert!(logs[0].starts_with("Program log: "));
        assert!(logs[1].starts_with("Log truncated"));
    }

}