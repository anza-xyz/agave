//! Comprehensive tests for UsageQueueConfig and memory optimization features

#[cfg(test)]
mod tests {
    use super::super::*;
    use std::sync::Arc;

    #[test]
    fn test_usage_queue_config_default() {
        let config = UsageQueueConfig::default();
        assert_eq!(config.base_capacity, 128);
        assert!(config.adaptive_sizing);
        assert_eq!(config.max_capacity, 2048);
    }

    #[test]
    fn test_usage_queue_config_high_throughput() {
        let config = UsageQueueConfig::high_throughput();
        assert_eq!(config.base_capacity, 1024);
        assert!(config.adaptive_sizing);
        assert_eq!(config.max_capacity, 8192);
    }

    #[test]
    fn test_usage_queue_config_low_memory() {
        let config = UsageQueueConfig::low_memory();
        assert_eq!(config.base_capacity, 64);
        assert!(!config.adaptive_sizing);
        assert_eq!(config.max_capacity, 512);
    }

    #[test]
    fn test_usage_queue_with_config() {
        let config = UsageQueueConfig {
            base_capacity: 256,
            adaptive_sizing: true,
            max_capacity: 1024,
        };

        let usage_queue = UsageQueue::with_config(config);

        // Verify the queue was created (we can't directly inspect capacity,
        // but we can verify it doesn't panic and works as expected)
        let _cloned = usage_queue.clone();
        assert_eq!(Arc::strong_count(&usage_queue.0), 2);
    }

    #[test]
    fn test_usage_queue_inner_with_config() {
        let config = UsageQueueConfig {
            base_capacity: 512,
            adaptive_sizing: false,
            max_capacity: 1024,
        };

        let inner = UsageQueueInner::with_config(config);
        assert!(inner.current_usage.is_none());
        assert!(inner.blocked_usages_from_tasks.is_empty());

        // The capacity should be set to base_capacity
        // We can't directly test capacity, but we can verify the VecDeque works
        assert_eq!(inner.blocked_usages_from_tasks.len(), 0);
    }

    #[test]
    fn test_memory_optimization_impact() {
        // Test that different configurations don't break existing functionality
        let configs = vec![
            UsageQueueConfig::default(),
            UsageQueueConfig::high_throughput(),
            UsageQueueConfig::low_memory(),
        ];

        for config in configs {
            let mut inner = UsageQueueInner::with_config(config);

            // Test basic locking functionality still works
            let lock_result = inner.try_lock(RequestedUsage::Writable);
            assert!(lock_result.is_ok());
            assert!(inner.current_usage.is_some());

            // Test unlocking
            let _ = inner.unlock(RequestedUsage::Writable);
            assert!(inner.current_usage.is_none());
        }
    }

    #[test]
    fn test_performance_characteristics() {
        use std::time::Instant;

        let high_throughput_config = UsageQueueConfig::high_throughput();
        let default_config = UsageQueueConfig::default();

        // Create multiple usage queues to simulate real-world usage
        let start = Instant::now();
        let _queues: Vec<_> = (0..100)
            .map(|_| UsageQueue::with_config(high_throughput_config))
            .collect();
        let high_throughput_time = start.elapsed();

        let start = Instant::now();
        let _queues: Vec<_> = (0..100)
            .map(|_| UsageQueue::with_config(default_config))
            .collect();
        let default_time = start.elapsed();

        // Both should complete reasonably quickly
        assert!(high_throughput_time.as_millis() < 10);
        assert!(default_time.as_millis() < 10);
    }
}
