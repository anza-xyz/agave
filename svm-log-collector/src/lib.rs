#![cfg(feature = "agave-unstable-api")]
pub use log;
use std::{cell::RefCell, rc::Rc};

const LOG_MESSAGES_BYTES_LIMIT: usize = 10 * 1000;

pub struct LogCollector {
    pub messages: Vec<String>,
    pub bytes_written: usize,
    pub bytes_limit: Option<usize>,
    pub limit_warning: bool,
}

impl Default for LogCollector {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            bytes_written: 0,
            bytes_limit: Some(LOG_MESSAGES_BYTES_LIMIT),
            limit_warning: false,
        }
    }
}

impl LogCollector {
    pub fn log(&mut self, message: &str) {
        if self.accepts(message.len()) {
            self.messages.push(message.to_string());
        }
    }

    /// Like [`LogCollector::log`], but takes an owned message so it doesn't
    /// have to be copied.
    pub fn log_owned(&mut self, message: String) {
        if self.accepts(message.len()) {
            self.messages.push(message);
        }
    }

    /// Whether a message of `len` bytes fits within the limit. If it doesn't,
    /// records the truncation warning instead.
    fn accepts(&mut self, len: usize) -> bool {
        let Some(limit) = self.bytes_limit else {
            return true;
        };

        let bytes_written = self.bytes_written.saturating_add(len);
        if bytes_written >= limit {
            if !self.limit_warning {
                self.limit_warning = true;
                self.messages.push(String::from("Log truncated"));
            }
            false
        } else {
            self.bytes_written = bytes_written;
            true
        }
    }

    pub fn get_recorded_content(&self) -> &[String] {
        self.messages.as_slice()
    }

    pub fn new_ref() -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self::default()))
    }

    pub fn new_ref_with_limit(bytes_limit: Option<usize>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            bytes_limit,
            ..Self::default()
        }))
    }

    pub fn into_messages(self) -> Vec<String> {
        self.messages
    }
}

/// Convenience macro to log a message with an `Option<Rc<RefCell<LogCollector>>>`
#[macro_export]
macro_rules! ic_logger_msg {
    ($log_collector:expr, $message:expr) => {
        $crate::log::debug!(
            target: "solana_runtime::message_processor::stable_log",
            "{}",
            $message
        );
        if let Some(log_collector) = $log_collector.as_ref() {
            if let Ok(mut log_collector) = log_collector.try_borrow_mut() {
                log_collector.log($message);
            }
        }
    };
    ($log_collector:expr, $fmt:expr, $($arg:tt)*) => {
        $crate::log::debug!(
            target: "solana_runtime::message_processor::stable_log",
            $fmt,
            $($arg)*
        );
        if let Some(log_collector) = $log_collector.as_ref() {
            if let Ok(mut log_collector) = log_collector.try_borrow_mut() {
                log_collector.log_owned(format!($fmt, $($arg)*));
            }
        }
    };
}

/// Convenience macro to log a message with an `InvokeContext`
#[macro_export]
macro_rules! ic_msg {
    ($invoke_context:expr, $message:expr) => {
        $crate::ic_logger_msg!($invoke_context.get_log_collector(), $message)
    };
    ($invoke_context:expr, $fmt:expr, $($arg:tt)*) => {
        $crate::ic_logger_msg!($invoke_context.get_log_collector(), $fmt, $($arg)*)
    };
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn test_log_messages_bytes_limit() {
        let mut lc = LogCollector::default();

        for _i in 0..LOG_MESSAGES_BYTES_LIMIT * 2 {
            lc.log("x");
        }

        let logs: Vec<_> = lc.into_messages();
        assert_eq!(logs.len(), LOG_MESSAGES_BYTES_LIMIT);
        for log in logs.iter().take(LOG_MESSAGES_BYTES_LIMIT - 1) {
            assert_eq!(*log, "x".to_string());
        }
        assert_eq!(logs.last(), Some(&"Log truncated".to_string()));
    }

    #[test]
    fn test_log_owned_matches_log() {
        let messages = ["hello", "", "a longer message", "x"];
        for bytes_limit in [None, Some(0), Some(6), Some(22), Some(100)] {
            let mut borrowed = LogCollector {
                bytes_limit,
                ..LogCollector::default()
            };
            let mut owned = LogCollector {
                bytes_limit,
                ..LogCollector::default()
            };
            for message in messages {
                borrowed.log(message);
                owned.log_owned(message.to_string());
            }
            assert_eq!(borrowed.messages, owned.messages);
            assert_eq!(borrowed.bytes_written, owned.bytes_written);
            assert_eq!(borrowed.limit_warning, owned.limit_warning);
        }
    }

    #[test]
    fn test_ic_logger_msg_respects_limit() {
        // The first message is 22 bytes, so the second doesn't fit in 30.
        let log_collector = LogCollector::new_ref_with_limit(Some(30));
        let maybe_log_collector = Some(log_collector.clone());
        ic_logger_msg!(&maybe_log_collector, "Program {} invoke [{}]", "abc", 1);
        ic_logger_msg!(&maybe_log_collector, "Program {} success", "abc");
        assert_eq!(
            log_collector.borrow().get_recorded_content(),
            ["Program abc invoke [1]", "Log truncated"]
        );
    }
}
