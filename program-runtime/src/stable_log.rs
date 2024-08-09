//! Stable program log messages
//!
//! The format of these log messages should not be modified to avoid breaking downstream consumers
//! of program logging

use base64::encoded_len;
use {
    base64::{prelude::BASE64_STANDARD, Engine},
    solana_log_collector::{ic_logger_msg, LogCollector},
    solana_sdk::pubkey::Pubkey,
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
pub fn program_data(log_collector: &Option<Rc<RefCell<LogCollector>>>, data: &[&[u8]]) {
    // Pre-allocate the result string with an estimated capacity.
    // The estimation assumes base64 encoding increases the size by about 4/3, plus some extra for spaces (padding = true).
    let estimated_capacity = data.iter().map(|v| encoded_len(v.len(), true).unwrap()).sum::<usize>();
    let mut result = String::with_capacity(estimated_capacity);

    // Build the string manually to avoid intermediate allocations.
    result.push_str("Program data: ");

    for (i, v) in data.iter().enumerate() {
        if i > 0 {
            result.push(' ');
        }
        // Use BASE64_STANDARD.encode_string() to append directly to the existing string, instead of creating new strings for each piece of data.
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
    ic_logger_msg!(log_collector, &["Program return: ",  &program_id.to_string(), &BASE64_STANDARD.encode(data)].concat());
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
    use solana_vote::vote_account::Error;
    use super::*;

    #[test]
    fn test_program_data() {
        // Setup
        let log_collector = Some(LogCollector::new_ref());

        // Test data
        let data1 = b"Hello";
        let data2 = b"World";
        let data = [data1 as &[u8], data2 as &[u8]];

        // Call the function
        program_data(&log_collector, &data);

        // Verify the result
        let binding = log_collector.unwrap();
        let messages = &binding.borrow().messages;
        assert_eq!(messages.len(), 1);

        let expected_message = format!(
            "Program data: {} {}",
            BASE64_STANDARD.encode(data1),
            BASE64_STANDARD.encode(data2)
        );
        assert_eq!(messages[0], expected_message);
    }

    #[test]
    fn test_program_invoke() {
        // Setup
        let log_collector = Some(LogCollector::new_ref());

        // Test data
        let program_id = Pubkey::new_unique();

        // Call the function
        program_invoke(&log_collector, &program_id, 1);

        // Verify the result
        let binding = log_collector.unwrap();
        let messages = &binding.borrow().messages;
        assert_eq!(messages.len(), 1);

        let expected_message = format!(
            "Program {} invoke [{}]",
            program_id,
            1
        );
        assert_eq!(messages[0], expected_message);
    }

    #[test]
    fn test_program_failure() {
        // Setup
        let log_collector = Some(LogCollector::new_ref());

        // Test data
        let program_id = Pubkey::new_unique();
        let error = Error::InvalidOwner(program_id);

        // Call the function
        program_failure(&log_collector, &program_id, &error);

        // Verify the result
        let binding = log_collector.unwrap();
        let messages = &binding.borrow().messages;
        assert_eq!(messages.len(), 1);

        let expected_message = format!("Program {} failed: Invalid vote account owner: {}", program_id, program_id);
        assert_eq!(messages[0], expected_message);
    }
}