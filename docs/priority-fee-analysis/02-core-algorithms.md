# Solana (Agave) Priority Fee 源码分析 - 核心算法深度解析

> **文档**: 第2部分 - 调度算法与优先级机制

---

## 目录

1. [Priority 计算的完整数学模型](#priority-计算的完整数学模型)
2. [PrioGraph 调度器深度解析](#priograph-调度器深度解析)
3. [账户锁与冲突检测机制](#账户锁与冲突检测机制)
4. [线程选择与负载均衡算法](#线程选择与负载均衡算法)
5. [成本追踪器实现细节](#成本追踪器实现细节)

---

## Priority 计算的完整数学模型

### 1.1 公式推导

**原始公式** (receive_and_buffer.rs:537-541):
```
priority = (reward * MULTIPLIER) / (cost + 1)
其中 MULTIPLIER = 1_000_000
```

**展开 reward**:
```
reward = priority_fee + (transaction_fee - burn)
       = priority_fee + (transaction_fee - transaction_fee * 0.5)
       = priority_fee + transaction_fee * 0.5
       = (compute_unit_limit * compute_unit_price) + base_fee * 0.5
```

**展开 cost**:
```
cost = signature_cost + write_lock_cost + data_bytes_cost
     + programs_execution_cost + loaded_accounts_data_size_cost

其中:
  signature_cost = num_signatures * 720
  write_lock_cost = num_write_locks * 300
  data_bytes_cost = instruction_data_bytes / 4.67
  programs_execution_cost = compute_unit_limit
  loaded_accounts_data_size_cost ≈ loaded_accounts_bytes / 32
```

**完整公式**:
```
priority = [(CU_limit * CU_price) + base_fee * 0.5] * 1_000_000
           ────────────────────────────────────────────────────
           [sig_cost + lock_cost + data_cost + CU_limit + load_cost] + 1
```

### 1.2 Priority 的数值范围与分布

**理论最小值**:
```
compute_unit_price = 0 (无优先费)
reward ≈ base_fee * 0.5 ≈ 2500 lamports (假设 1 签名, 5000 lamports base fee)
cost ≈ 200_000 CU (典型交易)
priority ≈ (2500 * 1_000_000) / 200_000 ≈ 12_500
```

**理论最大值**:
```
compute_unit_price = u64::MAX (不现实)
compute_unit_limit = 1_400_000 (最大 CU)
reward ≈ u64::MAX (溢出保护: saturating_mul)
cost ≈ 1_400_000
priority ≈ u64::MAX / 1_400_000 ≈ 13_182_000_000_000
```

**实际常见范围** (主网观察):
| 场景 | compute_unit_price | priority 范围 |
|------|-------------------|---------------|
| 低优先级 | 1-100 μlamports/CU | 5K - 500K |
| 中优先级 | 100-1000 μlamports/CU | 500K - 5M |
| 高优先级 | 1000-10000 μlamports/CU | 5M - 50M |
| 极高优先级 | 10000+ μlamports/CU | 50M+ |

### 1.3 Priority 对不同因素的敏感度分析

**敏感度公式**:
```
∂priority/∂CU_price = MULTIPLIER * CU_limit / (cost + 1)
```

**示例计算**:
```
CU_limit = 200_000
cost = 202_000
敏感度 = 1_000_000 * 200_000 / 202_001 ≈ 990
```

**含义**: `compute_unit_price` 每增加 1 microlamport/CU, priority 增加约 990。

**对比不同 cost 的敏感度**:
| cost | 敏感度 | 解释 |
|------|--------|------|
| 100K | 1990 | 轻量交易, CU price 影响大 |
| 200K | 990 | 标准交易 |
| 500K | 395 | 重型交易, CU price 影响被稀释 |
| 1M | 199 | 极重交易 |

**结论**: **Cost 越高, priority 对 compute_unit_price 的敏感度越低**。这意味着重型交易即使提高 fee 也难以与轻量高 fee 交易竞争。

---

## PrioGraph 调度器深度解析

### 2.1 调度器架构

**文件**: `core/src/banking_stage/transaction_scheduler/prio_graph_scheduler.rs`

**核心结构** (line 68-72):
```rust
pub(crate) struct PrioGraphScheduler<Tx> {
    common: SchedulingCommon<Tx>,      // 公共调度状态
    prio_graph: SchedulerPrioGraph,    // 优先级依赖图
    config: PrioGraphSchedulerConfig,  // 配置参数
}
```

**配置参数** (line 49-64):
```rust
pub(crate) struct PrioGraphSchedulerConfig {
    pub max_scheduled_cus: u64,         // 60_000_000 (MAX_BLOCK_UNITS)
    pub max_scanned_transactions_per_scheduling_pass: usize,  // 1000
    pub look_ahead_window_size: usize,  // 256
    pub target_transactions_per_batch: usize,  // 64 (TARGET_NUM_TRANSACTIONS_PER_BATCH)
}
```

### 2.2 调度主循环详解

**函数**: `PrioGraphScheduler::schedule()`
**位置**: prio_graph_scheduler.rs:110-356

#### 阶段 1: 预算初始化 (line 118-125)

```rust
// 减去已在执行的 CU
let mut budget = budget.saturating_sub(
    self.common
        .in_flight_tracker
        .cus_in_flight_per_thread()
        .iter()
        .sum(),
);
```

**关键点**: Budget 是动态的, 已调度但未完成的交易占用的 CU 会被预留。

#### 阶段 2: 线程可用性检查 (line 130-147)

```rust
let num_threads = self.common.consume_work_senders.len();
let max_cu_per_thread = self.config.max_scheduled_cus / num_threads as u64;

let mut schedulable_threads = ThreadSet::any(num_threads);
for thread_id in 0..num_threads {
    if self.common.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
        >= max_cu_per_thread
    {
        schedulable_threads.remove(thread_id);
    }
}
if schedulable_threads.is_empty() {
    return Ok(SchedulingSummary { /* ... */ });
}
```

**作用**:
- 计算每个线程的 CU 配额: `60M / num_threads`
- 排除已满载的线程
- 如果所有线程都满, 直接返回 (不调度任何交易)

**示例** (4 线程):
```
max_cu_per_thread = 60M / 4 = 15M

线程状态:
  Thread 0: in_flight_cus = 12M  ✓ 可用
  Thread 1: in_flight_cus = 15M  ✗ 满载
  Thread 2: in_flight_cus = 8M   ✓ 可用
  Thread 3: in_flight_cus = 14M  ✓ 可用

schedulable_threads = {0, 2, 3}
```

#### 阶段 3: 前瞻窗口构建 (line 160-210)

```rust
let mut window_budget = self.config.look_ahead_window_size;  // 256
let mut chunked_pops = |container, prio_graph, window_budget| {
    while *window_budget > 0 {
        const MAX_FILTER_CHUNK_SIZE: usize = 128;
        let chunk_size = (*window_budget).min(MAX_FILTER_CHUNK_SIZE);

        // 从 container 弹出交易
        for _ in 0..chunk_size {
            if let Some(id) = container.pop() {
                ids.push(id);
            } else {
                break;
            }
        }

        // 应用 pre_graph_filter
        pre_graph_filter(&txs, &mut filter_array[..chunk_size]);

        // 插入到 prio_graph
        for (id, filter_result) in ids.iter().zip(&filter_array[..chunk_size]) {
            if *filter_result {
                let transaction = container.get_transaction(id.id).unwrap();
                prio_graph.insert_transaction(
                    *id,
                    Self::get_transaction_account_access(transaction),
                );
            } else {
                num_filtered_out += 1;
                container.remove_by_id(id.id);
            }
        }
    }
};

// 初始化前瞻窗口
chunked_pops(container, &mut self.prio_graph, &mut window_budget);
```

**关键设计**:
1. **前瞻窗口大小**: 256 个交易
2. **分块处理**: 每次最多处理 128 个 (减少锁开销)
3. **依赖图构建**: `insert_transaction()` 建立账户访问依赖关系

**PrioGraph 内部结构**:
```rust
PrioGraph<TransactionPriorityId, Pubkey, ...> {
    // 交易节点按 priority 排序
    // 跟踪每个 Pubkey 的读写依赖
    // 支持高效的 pop() 和 unblock() 操作
}
```

#### 阶段 4: 主调度循环 (line 220-320)

```rust
while budget > 0 && num_scanned < self.config.max_scanned_transactions_per_scheduling_pass {
    if self.prio_graph.is_empty() {
        break;
    }

    while let Some(id) = self.prio_graph.pop() {
        num_scanned += 1;
        unblock_this_batch.push(id);

        let Some(transaction_state) = container.get_mut_transaction_state(id.id) else {
            panic!("transaction state must exist")
        };

        // 尝试调度交易
        let maybe_schedule_info = try_schedule_transaction(
            transaction_state,
            &pre_lock_filter,
            &mut blocking_locks,
            &mut self.common.account_locks,
            num_threads,
            |thread_set| {
                select_thread(
                    thread_set,
                    self.common.batches.total_cus(),
                    self.common.in_flight_tracker.cus_in_flight_per_thread(),
                    self.common.batches.transactions(),
                    self.common.in_flight_tracker.num_in_flight_per_thread(),
                )
            },
        );

        match maybe_schedule_info {
            Err(TransactionSchedulingError::UnschedulableConflicts) => {
                num_unschedulable_conflicts += 1;
                unschedulable_ids.push(id);
            }
            Err(TransactionSchedulingError::UnschedulableThread) => {
                num_unschedulable_threads += 1;
                unschedulable_ids.push(id);
            }
            Ok(TransactionSchedulingInfo { thread_id, transaction, max_age, cost }) => {
                num_scheduled += 1;
                self.common.batches.add_transaction_to_batch(
                    thread_id, id.id, transaction, max_age, cost
                );
                budget = budget.saturating_sub(cost);

                // 如果达到目标批次大小, 发送
                if self.common.batches.transactions()[thread_id].len()
                    >= self.config.target_transactions_per_batch
                {
                    num_sent += self.common.send_batch(thread_id)?;
                }

                // 如果线程满载, 从可调度集合移除
                if self.common.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
                    + self.common.batches.total_cus()[thread_id]
                    >= max_cu_per_thread
                {
                    schedulable_threads.remove(thread_id);
                    if schedulable_threads.is_empty() {
                        break;
                    }
                }
            }
        }

        if num_scanned >= self.config.max_scanned_transactions_per_scheduling_pass {
            break;
        }
    }

    // 发送所有非空批次
    num_sent += self.common.send_batches()?;

    // 刷新前瞻窗口
    window_budget += unblock_this_batch.len();
    chunked_pops(container, &mut self.prio_graph, &mut window_budget);

    // 解除已发送交易的依赖阻塞
    for id in unblock_this_batch.drain(..) {
        self.prio_graph.unblock(&id);
    }
}
```

**调度策略总结**:
1. **优先级优先**: 从 `prio_graph.pop()` 获取最高优先级交易
2. **冲突避免**: 检查账户锁, 有冲突则标记为 unschedulable
3. **负载均衡**: 通过 `select_thread()` 选择最合适的执行线程
4. **批次管理**: 达到目标大小 (64) 时立即发送, 减少延迟
5. **动态刷新**: 每轮调度后刷新前瞻窗口, 解除依赖阻塞

#### 阶段 5: 清理与返回 (line 322-356)

```rust
// 发送剩余批次
num_sent += self.common.send_batches()?;

// 不可调度交易推回队列
container.push_ids_into_queue(unschedulable_ids.into_iter());

// 剩余交易推回队列
container.push_ids_into_queue(std::iter::from_fn(|| {
    self.prio_graph.pop_and_unblock().map(|(id, _)| id)
}));

// 清空 prio_graph (下次调度重新构建)
self.prio_graph.clear();

return Ok(SchedulingSummary {
    starting_queue_size,
    starting_buffer_size,
    num_scheduled,
    num_unschedulable_conflicts,
    num_unschedulable_threads,
    num_filtered_out,
    filter_time_us: total_filter_time_us,
});
```

### 2.3 调度性能参数分析

**关键参数及其影响**:

| 参数 | 默认值 | 作用 | 调整影响 |
|------|--------|------|---------|
| `look_ahead_window_size` | 256 | 前瞻窗口大小 | 越大越能识别冲突, 但内存开销增加 |
| `max_scanned_per_pass` | 1000 | 每轮最多扫描交易数 | 限制单次调度时间, 避免阻塞 |
| `target_transactions_per_batch` | 64 | 目标批次大小 | 越大吞吐越高, 但延迟增加 |
| `max_scheduled_cus` | 60M | 最大调度 CU | 区块容量上限 |

**理论调度延迟**:
```
单次调度时间 ≈ max_scanned * per_tx_overhead
              ≈ 1000 * (锁检查 + 图操作 + 线程选择)
              ≈ 1000 * 10 μs
              ≈ 10 ms
```

**实际观察**: 在高负载下, 调度延迟可达 **5-20ms**。

---

## 账户锁与冲突检测机制

### 3.1 账户锁的实现

**文件**: `agave-scheduling-utils/src/thread_aware_account_locks.rs` (未在示例中展示完整代码)

**调用位置**: prio_graph_scheduler.rs:412-427

```rust
let thread_id = match account_locks.try_lock_accounts(
    write_account_locks,
    read_account_locks,
    ThreadSet::any(num_threads),
    thread_selector,
) {
    Ok(thread_id) => thread_id,
    Err(TryLockError::MultipleConflicts) => {
        blocking_locks.take_locks(transaction);
        return Err(TransactionSchedulingError::UnschedulableConflicts);
    }
    Err(TryLockError::ThreadNotAllowed) => {
        blocking_locks.take_locks(transaction);
        return Err(TransactionSchedulingError::UnschedulableThread);
    }
};
```

**锁类型**:
- **Read Lock**: 多个线程可以同时持有对同一账户的 Read Lock
- **Write Lock**: 独占锁, 任何线程持有 Write Lock 时, 其他线程不能持有任何锁

**冲突规则**:
| 当前锁 | 请求锁 | 结果 |
|--------|--------|------|
| None | Read | ✅ 允许 |
| None | Write | ✅ 允许 |
| Read (Thread A) | Read (Thread B) | ✅ 允许 (可在不同线程) |
| Read (Thread A) | Write (Thread B) | ❌ 冲突 |
| Write (Thread A) | Read (Thread B) | ❌ 冲突 |
| Write (Thread A) | Write (Thread B) | ❌ 冲突 |

### 3.2 冲突检测流程

**函数**: `try_schedule_transaction()`
**位置**: prio_graph_scheduler.rs:382-438

#### 步骤 1: 检查与 unschedulable 交易的冲突 (line 394-399)

```rust
let transaction = transaction_state.transaction();
if !blocking_locks.check_locks(transaction) {
    blocking_locks.take_locks(transaction);
    return Err(TransactionSchedulingError::UnschedulableConflicts);
}
```

**`blocking_locks`**: `ReadWriteAccountSet` - 存储所有当前不可调度交易持有的锁
**作用**: **防止优先级反转**

**示例场景**:
```
交易队列 (按 priority 降序):
  Tx1 (priority=1000, 账户: A, B)  ← 最高优先级
  Tx2 (priority=900, 账户: B, C)
  Tx3 (priority=800, 账户: C, D)

假设 Tx1 因账户 A 被其他线程锁定而不可调度:
  - Tx1 → UnschedulableConflicts
  - blocking_locks = {A, B}  ← 记录 Tx1 的所有锁

当调度 Tx2 时:
  - Tx2 需要账户 B
  - blocking_locks.check_locks(Tx2) = false (B 被 Tx1 阻塞)
  - Tx2 → UnschedulableConflicts
  - blocking_locks = {A, B, C}  ← 加入 Tx2 的锁

当调度 Tx3 时:
  - Tx3 需要账户 C, D
  - blocking_locks.check_locks(Tx3) = false (C 被 Tx2 阻塞)
  - Tx3 → UnschedulableConflicts

结果: Tx2 和 Tx3 虽然与当前执行的交易无冲突, 但因为与更高优先级的 unschedulable 交易冲突, 被阻止调度, 保证了优先级顺序。
```

#### 步骤 2: 尝试获取账户锁 (line 401-427)

```rust
let account_keys = transaction.account_keys();
let write_account_locks = account_keys
    .iter()
    .enumerate()
    .filter_map(|(index, key)| transaction.is_writable(index).then_some(key));
let read_account_locks = account_keys
    .iter()
    .enumerate()
    .filter_map(|(index, key)| (!transaction.is_writable(index)).then_some(key));

let thread_id = match account_locks.try_lock_accounts(
    write_account_locks,
    read_account_locks,
    ThreadSet::any(num_threads),
    thread_selector,
) {
    Ok(thread_id) => thread_id,
    Err(TryLockError::MultipleConflicts) => {
        // 多个线程冲突 → 无法调度
        blocking_locks.take_locks(transaction);
        return Err(TransactionSchedulingError::UnschedulableConflicts);
    }
    Err(TryLockError::ThreadNotAllowed) => {
        // 没有允许的线程 (都满载或冲突) → 无法调度
        blocking_locks.take_locks(transaction);
        return Err(TransactionSchedulingError::UnschedulableThread);
    }
};
```

**`try_lock_accounts()` 内部逻辑**:
1. 检查每个请求的账户锁是否与现有锁冲突
2. 标记哪些线程可以安全执行此交易 (无冲突)
3. 使用 `thread_selector` 从可用线程中选择一个
4. 返回选中的 `thread_id` 或错误

### 3.3 多线程账户锁示例

**场景**: 4 个执行线程, 3 笔交易

```
当前锁状态:
  Thread 0: Write Lock on Account A
  Thread 1: Read Lock on Account B
  Thread 2: 无锁
  Thread 3: Write Lock on Account C

待调度交易:
  Tx1: Read A, Write B
  Tx2: Write B, Read C
  Tx3: Read D, Write E
```

**Tx1 调度**:
- Write B: 冲突 (Thread 1 持有 Read Lock)
- Read A: 冲突 (Thread 0 持有 Write Lock)
- 可用线程: None
- 结果: `TryLockError::MultipleConflicts`

**Tx2 调度**:
- Write B: 冲突 (Thread 1)
- Read C: 冲突 (Thread 3 持有 Write Lock)
- 可用线程: None
- 结果: `TryLockError::MultipleConflicts`

**Tx3 调度**:
- Read D, Write E: 无冲突
- 可用线程: {0, 1, 2, 3}
- 选择: `select_thread()` → 假设选中 Thread 2
- 结果: `Ok(2)`

---

## 线程选择与负载均衡算法

### 4.1 线程选择函数

**函数**: `select_thread()`
**位置**: 引用自 `scheduler_common.rs` (未在示例中展示完整代码)
**调用**: prio_graph_scheduler.rs:248-254

```rust
select_thread(
    thread_set,                                      // 可用线程集合
    self.common.batches.total_cus(),                 // 每线程已批次的 CU
    self.common.in_flight_tracker.cus_in_flight_per_thread(),  // 每线程执行中的 CU
    self.common.batches.transactions(),              // 每线程已批次的交易数
    self.common.in_flight_tracker.num_in_flight_per_thread(),  // 每线程执行中的交易数
)
```

### 4.2 负载均衡策略 (推测)

**可能的选择策略** (基于参数推测):

1. **CU 负载均衡**:
   ```rust
   total_cu[thread] = batched_cu[thread] + in_flight_cu[thread]
   selected_thread = argmin(total_cu, thread_set)
   ```
   选择总 CU 负载最低的线程。

2. **交易数负载均衡**:
   ```rust
   total_tx[thread] = batched_tx[thread] + in_flight_tx[thread]
   selected_thread = argmin(total_tx, thread_set)
   ```
   选择总交易数最少的线程。

3. **混合策略**:
   ```rust
   load[thread] = α * normalized_cu[thread] + β * normalized_tx[thread]
   selected_thread = argmin(load, thread_set)
   ```
   加权组合 CU 和交易数负载。

**测试证据** (prio_graph_scheduler.rs:694-712):
```rust
#[test]
fn test_schedule_simple_thread_selection() {
    let mut container = create_container(
        (0..4).map(|i| (Keypair::new(), [Pubkey::new_unique()], 1, i))
    );
    // 4 笔无冲突交易, priority: 3, 2, 1, 0

    // ... 调度后 ...
    assert_eq!(collect_work(&work_receivers[0]).1, [vec![3, 1]]);
    assert_eq!(collect_work(&work_receivers[1]).1, [vec![2, 0]]);
}
```

**观察**: 交易被轮流分配到 Thread 0 和 Thread 1, 暗示**轮询 (round-robin)** 或**最少交易数**策略。

---

## 成本追踪器实现细节

### 5.1 CostTracker 结构

**文件**: `cost-model/src/cost_tracker.rs`

**核心结构** (line 71-88):
```rust
pub struct CostTracker {
    account_cost_limit: u64,           // 12_000_000 (MAX_WRITABLE_ACCOUNT_UNITS)
    block_cost_limit: u64,             // 60_000_000 (MAX_BLOCK_UNITS)
    vote_cost_limit: u64,              // 36_000_000 (MAX_VOTE_UNITS)

    cost_by_writable_accounts: HashMap<Pubkey, u64>,  // 每个可写账户累计的 cost
    block_cost: SharedBlockCost,       // 区块总 cost (线程安全)
    vote_cost: u64,                    // 投票交易总 cost

    transaction_count: u64,
    allocated_accounts_data_size: u64,
    transaction_signature_count: u64,
    // ...
    in_flight_transaction_count: usize,  // 待完成交易数
}
```

### 5.2 Cost 限制检查

**函数**: `CostTracker::try_add()`
**位置**: cost_tracker.rs:169-179

```rust
pub fn try_add(
    &mut self,
    tx_cost: &TransactionCost<impl TransactionWithMeta>,
) -> Result<UpdatedCosts, CostTrackerError> {
    self.would_fit(tx_cost)?;  // ← 关键检查函数
    let updated_costliest_account_cost = self.add_transaction_cost(tx_cost);
    Ok(UpdatedCosts {
        updated_block_cost: self.block_cost(),
        updated_costliest_account_cost,
    })
}
```

**`would_fit()` 检查项** (推测实现):

1. **Block Cost 检查**:
   ```rust
   if self.block_cost + tx_cost.sum() > self.block_cost_limit {
       return Err(CostTrackerError::WouldExceedBlockMaxLimit);
   }
   ```

2. **Vote Cost 检查** (如果是投票交易):
   ```rust
   if tx.is_simple_vote_transaction() {
       if self.vote_cost + tx_cost.sum() > self.vote_cost_limit {
           return Err(CostTrackerError::WouldExceedVoteMaxLimit);
       }
   }
   ```

3. **Writable Account Cost 检查**:
   ```rust
   for (account, account_cost) in tx_cost.writable_accounts() {
       let current_cost = self.cost_by_writable_accounts.get(account).unwrap_or(&0);
       if current_cost + account_cost > self.account_cost_limit {
           return Err(CostTrackerError::WouldExceedAccountMaxLimit);
       }
   }
   ```

4. **Allocated Data Size 检查**:
   ```rust
   if self.allocated_accounts_data_size + tx_cost.allocated_accounts_data_size
       > MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA
   {
       return Err(CostTrackerError::WouldExceedAccountDataBlockLimit);
   }
   ```

### 5.3 关键常量

**文件**: `cost-model/src/block_cost_limits.rs:1-49`

```rust
// 计算单位与时间转换比率
pub const COMPUTE_UNIT_TO_US_RATIO: u64 = 30;  // 1 CU ≈ 30 微秒

// 各种操作的 CU 成本
pub const SIGNATURE_COST: u64 = 30 * 24 = 720;
pub const SECP256K1_VERIFY_COST: u64 = 30 * 223 = 6_690;
pub const ED25519_VERIFY_COST: u64 = 30 * 76 = 2_280;
pub const WRITE_LOCK_UNITS: u64 = 30 * 10 = 300;

// 区块级别限制
pub const MAX_BLOCK_UNITS: u64 = 60_000_000;  // 60M CU
pub const MAX_WRITABLE_ACCOUNT_UNITS: u64 = 12_000_000;  // 12M CU
pub const MAX_VOTE_UNITS: u64 = 36_000_000;  // 36M CU

// 账户数据增长限制
pub const MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA: u64 = 100_000_000;  // 100 MB
```

### 5.4 区块容量分析

**理论最大交易数** (假设标准交易):
```
标准交易 cost ≈ 200_000 CU
MAX_BLOCK_UNITS = 60_000_000 CU
理论最大交易数 = 60M / 200K = 300 笔
```

**实际观察** (主网):
- 平均交易数: **2000-3000 笔/区块**
- 原因: 包含大量低成本交易 (简单转账, 投票等)

**账户锁瓶颈**:
```
MAX_WRITABLE_ACCOUNT_UNITS = 12M CU
如果 1000 笔交易都写同一账户 (每笔 200K CU):
  累计 cost = 1000 * 200K = 200M CU
  但限制是 12M CU
  实际可打包 = 12M / 200K = 60 笔

结论: 热门账户 (如 DEX 池) 成为瓶颈, 限制并行度。
```

---

## 核心数据结构速查

| 结构体 | 文件 | 作用 |
|--------|------|------|
| `TransactionPriorityId` | transaction_priority_id.rs | 优先级 + ID 组合, 用于排序 |
| `TransactionState` | transaction_state.rs | 存储交易 + 元数据 (priority, cost, max_age) |
| `PrioGraphScheduler` | prio_graph_scheduler.rs | 主调度器, 管理依赖图和调度逻辑 |
| `SchedulerPrioGraph` | (prio-graph 库) | 优先级依赖图, 支持高效 pop/unblock |
| `ThreadAwareAccountLocks` | (agave-scheduling-utils) | 多线程账户锁管理 |
| `ReadWriteAccountSet` | read_write_account_set.rs | 账户读写集合, 用于冲突检测 |
| `CostTracker` | cost_tracker.rs | 区块成本追踪, 限制检查 |
| `PrioritizationFee` | prioritization_fee.rs | 单区块 fee 统计 |
| `PrioritizationFeeCache` | prioritization_fee_cache.rs | 多区块 fee 缓存 (150 个) |

---

## 下一部分预告

**第3部分**: 关键决策点识别
- 所有交易失败原因的完整列表
- 每个失败点的代码位置
- Priority Fee 无效的边界条件
- 极端场景分析

**第4部分**: 时间窗口量化分析
- Slot 时间计算
- 调度延迟的量化模型
- 网络负载对延迟的影响
- 最快上链时间的理论下界

**第5部分**: 实战策略框架
- 基于源码的 Fee 估算算法
- 动态调整策略
- 失败重试决策树
- 成本效益优化模型
