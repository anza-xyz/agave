# Solana (Agave) Priority Fee 源码分析 - 关键决策点与边界条件

> **文档**: 第3部分 - 失败原因、边界条件与异常情况

---

## 目录

1. [交易失败的完整决策树](#交易失败的完整决策树)
2. [Priority Fee 无效的边界条件](#priority-fee-无效的边界条件)
3. [账户锁冲突场景分析](#账户锁冲突场景分析)
4. [极端拥堵下的行为](#极端拥堵下的行为)
5. [时间敏感错误](#时间敏感错误)

---

## 交易失败的完整决策树

### 1.1 失败点总览

```
[Client 提交交易]
        │
        ↓
┌───────────────────────────────────────────────────────────┐
│ 阶段 0: 签名验证 (SigVerify Stage - 前置)                 │
├───────────────────────────────────────────────────────────┤
│ ❌ 签名无效 → 丢弃 (不进入 Banking Stage)                  │
└───────────────────────────────────────────────────────────┘
        │ ✓ 签名有效
        ↓
┌───────────────────────────────────────────────────────────┐
│ 阶段 1: Receive & Buffer - 初步验证                       │
│ 文件: receive_and_buffer.rs:232-404                       │
├───────────────────────────────────────────────────────────┤
│ 1.1 决策判断 (line 244)                                   │
│     ❌ BufferedPacketsDecision::Forward                   │
│        → num_dropped_without_parsing++                    │
│        原因: 当前节点不是 Leader, 不处理交易               │
│                                                            │
│ 1.2 解析与 Sanitize (line 344-370)                        │
│     ❌ 无效格式 → PacketHandlingError::Sanitization       │
│        → num_dropped_on_parsing_and_sanitization++        │
│        位置: translate_to_runtime_view() line 453-457     │
│                                                            │
│ 1.3 Vote-only Bank 检查 (line 468-470)                    │
│     ❌ 非投票交易 + vote_only_bank()                       │
│        → PacketHandlingError::Sanitization                │
│                                                            │
│ 1.4 账户总数限制 (line 472-474)                           │
│     ❌ num_accounts > transaction_account_lock_limit      │
│        → PacketHandlingError::LockValidation              │
│        → num_dropped_on_lock_validation++                 │
│                                                            │
│ 1.5 ALT 地址解析 (line 491-503)                           │
│     ❌ 地址查找表不存在或无效                              │
│        → PacketHandlingError::ALTResolution               │
│        → num_dropped_on_parsing_and_sanitization++        │
│                                                            │
│ 1.6 账户锁验证 (line 419-426)                             │
│     ❌ validate_account_locks() 失败                       │
│        → PacketHandlingError::LockValidation              │
│        → num_dropped_on_lock_validation++                 │
│                                                            │
│ 1.7 Compute Budget 解析 (line 428-433)                    │
│     ❌ sanitize_and_convert_to_compute_budget_limits()失败 │
│        → PacketHandlingError::ComputeBudget               │
│        → num_dropped_on_compute_budget++                  │
│     ❌ compute_unit_limit == 0                            │
│        → 过滤 (prioritization_fee_cache.rs:238)           │
└───────────────────────────────────────────────────────────┘
        │ ✓ 通过初步验证
        ↓ calculate_priority_and_cost()
        ↓ 进入 Container
        ↓
┌───────────────────────────────────────────────────────────┐
│ 阶段 2: Check Transactions - 状态验证                     │
│ 文件: receive_and_buffer.rs:273-323                       │
├───────────────────────────────────────────────────────────┤
│ 2.1 Blockhash 年龄检查 (line 273-278)                     │
│     ❌ TransactionError::BlockhashNotFound                │
│        → num_dropped_on_age++                             │
│        原因: blockhash 过期 (超过 MAX_PROCESSING_AGE)      │
│        位置: Bank::check_transactions()                   │
│                                                            │
│ 2.2 重复交易检查 (line 291-293)                           │
│     ❌ TransactionError::AlreadyProcessed                 │
│        → num_dropped_on_already_processed++               │
│        原因: 交易已在链上                                  │
│                                                            │
│ 2.3 Fee Payer 余额检查 (line 302-311)                     │
│     ❌ Consumer::check_fee_payer_unlocked() 失败           │
│        → num_dropped_on_fee_payer++                       │
│        原因: Fee payer 余额不足                            │
└───────────────────────────────────────────────────────────┘
        │ ✓ 通过状态验证
        ↓ 进入 Priority Queue
        ↓
┌───────────────────────────────────────────────────────────┐
│ 阶段 3: Scheduler - 调度阶段                              │
│ 文件: prio_graph_scheduler.rs:110-356                     │
├───────────────────────────────────────────────────────────┤
│ 3.1 Budget 检查 (line 225)                                │
│     ❌ budget <= 0                                        │
│        → 停止调度, 交易留在队列                            │
│        原因: 区块 CU 预算耗尽                              │
│                                                            │
│ 3.2 线程可用性检查 (line 141-147)                         │
│     ❌ schedulable_threads.is_empty()                     │
│        → 停止调度, 交易留在队列                            │
│        原因: 所有线程满载                                  │
│                                                            │
│ 3.3 Pre-graph Filter (line 185-200)                       │
│     ❌ pre_graph_filter() 返回 false                       │
│        → num_filtered_out++                               │
│        → container.remove_by_id()  ← 直接丢弃!            │
│                                                            │
│ 3.4 扫描限制 (line 225, 304)                              │
│     ❌ num_scanned >= max_scanned_per_pass (1000)         │
│        → 本轮停止调度, 交易留待下次                        │
│                                                            │
│ 3.5 账户锁冲突 (line 394-399)                             │
│     ❌ !blocking_locks.check_locks()                      │
│        → UnschedulableConflicts                           │
│        → 推回队列, 等待高优先级交易完成                    │
│                                                            │
│ 3.6 多线程锁冲突 (line 419-422)                           │
│     ❌ TryLockError::MultipleConflicts                    │
│        → UnschedulableConflicts                           │
│        → 推回队列                                          │
│                                                            │
│ 3.7 线程不可用 (line 423-426)                             │
│     ❌ TryLockError::ThreadNotAllowed                     │
│        → UnschedulableThread                              │
│        → 推回队列                                          │
│                                                            │
│ 3.8 目标批次达标 (line 284-288)                           │
│     ✓ 发送批次, 继续调度                                  │
│                                                            │
│ 3.9 线程满载 (line 292-300)                               │
│     ✓ 移除该线程, 继续在其他线程调度                       │
└───────────────────────────────────────────────────────────┘
        │ ✓ 调度成功
        ↓ 发送到 Worker
        ↓
┌───────────────────────────────────────────────────────────┐
│ 阶段 4: QoS Service - 成本限制检查                        │
│ 文件: qos_service.rs:102-158                              │
├───────────────────────────────────────────────────────────┤
│ 4.1 Block Cost 限制 (cost_tracker.rs:169)                 │
│     ❌ CostTrackerError::WouldExceedBlockMaxLimit         │
│        原因: block_cost + tx_cost > 60M CU                │
│        → 交易被拒绝, 不执行                                │
│                                                            │
│ 4.2 Vote Cost 限制                                        │
│     ❌ CostTrackerError::WouldExceedVoteMaxLimit          │
│        原因: vote_cost + tx_cost > 36M CU (仅投票交易)     │
│                                                            │
│ 4.3 Account Cost 限制                                     │
│     ❌ CostTrackerError::WouldExceedAccountMaxLimit       │
│        原因: account_cost + tx_cost > 12M CU              │
│        说明: 单个可写账户的累计 cost 超限                  │
│                                                            │
│ 4.4 Account Data Block 限制                               │
│     ❌ CostTrackerError::WouldExceedAccountDataBlockLimit │
│        原因: allocated_data_size > 100MB                  │
│                                                            │
│ 4.5 Account Data Total 限制                               │
│     ❌ CostTrackerError::WouldExceedAccountDataTotalLimit │
│        原因: 总账户数据增长超限                            │
└───────────────────────────────────────────────────────────┘
        │ ✓ 通过成本检查
        ↓ 执行交易
        ↓
┌───────────────────────────────────────────────────────────┐
│ 阶段 5: 执行阶段 (SVM)                                    │
├───────────────────────────────────────────────────────────┤
│ ❌ 程序执行失败 (逻辑错误, 余额不足等)                     │
│    → 交易上链, 但标记为失败                                │
│    → Fee 仍然扣除!                                         │
└───────────────────────────────────────────────────────────┘
```

### 1.2 失败统计汇总

**文件**: receive_and_buffer.rs:50-69

```rust
pub(crate) struct ReceivingStats {
    pub num_received: usize,                             // 总接收数
    pub num_dropped_without_parsing: usize,              // 未解析就丢弃 (Forward)
    pub num_dropped_on_parsing_and_sanitization: usize,  // 解析/Sanitize 失败
    pub num_dropped_on_lock_validation: usize,           // 账户锁验证失败
    pub num_dropped_on_compute_budget: usize,            // Compute Budget 无效
    pub num_dropped_on_age: usize,                       // Blockhash 过期
    pub num_dropped_on_already_processed: usize,         // 重复交易
    pub num_dropped_on_fee_payer: usize,                 // Fee payer 余额不足
    pub num_dropped_on_capacity: usize,                  // Container 容量满
    pub num_buffered: usize,                             // 成功缓冲数
}
```

**调度器统计** (prio_graph_scheduler.rs:347-355):
```rust
Ok(SchedulingSummary {
    starting_queue_size,        // 调度开始时队列大小
    starting_buffer_size,       // 调度开始时缓冲区大小
    num_scheduled,              // 成功调度数
    num_unschedulable_conflicts,// 账户锁冲突数
    num_unschedulable_threads,  // 无可用线程数
    num_filtered_out,           // Pre-graph filter 过滤数
    filter_time_us,             // 过滤耗时
})
```

---

## Priority Fee 无效的边界条件

### 2.1 场景一: 账户锁成为瓶颈

**问题描述**: 即使提高 priority fee, 也无法更快上链。

**根本原因**: 热门账户的 writable account cost 限制。

**代码位置**: cost_tracker.rs (account_cost_limit 检查)

**示例场景**: DEX 交易高峰期

```
热门 DEX 池账户: 0xABC...

区块中已有 50 笔交易写入 0xABC, 累计 cost = 10M CU
MAX_WRITABLE_ACCOUNT_UNITS = 12M CU

你的交易:
  priority = 1_000_000 (极高)
  cost = 300K CU
  需要写入: 0xABC

检查:
  account_cost(0xABC) + tx_cost = 10M + 300K = 10.3M < 12M  ✓ 通过

但是:
  后续还有 5 笔交易也写入 0xABC, 每笔 300K CU
  10.3M + 5 * 300K = 11.8M  ← 接近限制

第 6 笔交易:
  11.8M + 300K = 12.1M > 12M  ✗ 失败!
  → CostTrackerError::WouldExceedAccountMaxLimit

结果: 即使你的 priority 最高, 如果排在第 6+ 位, 仍会被拒绝。
```

**量化分析**:
```
MAX_WRITABLE_ACCOUNT_UNITS = 12_000_000 CU
典型 DEX 交易 cost = 300_000 CU

单个账户每区块最多打包 = 12M / 300K = 40 笔交易

如果提交了 100 笔交易写同一账户, 只有前 40 笔能成功, 剩余 60 笔无论 fee 多高都会失败。
```

**对策**:
1. **分散账户**: 使用不同的账户或池
2. **等待下一区块**: 账户 cost 每个 slot 重置
3. **优化交易**: 减少 cost (减少 CU limit)

### 2.2 场景二: Block Budget 耗尽

**问题描述**: 交易在调度阶段被跳过, 留在队列。

**根本原因**: 调度器 budget 检查。

**代码位置**: prio_graph_scheduler.rs:225

```rust
while budget > 0 && num_scanned < max_scanned_per_pass {
    // ...
}
```

**示例场景**:

```
区块开始:
  budget = 60M CU

已调度交易累计 cost = 58M CU
budget = 60M - 58M = 2M CU

你的交易:
  priority = 1_000_000
  cost = 300K CU
  可以调度? 2M > 300K ✓

但是:
  调度器先处理队列前面的交易 (可能 priority 更高)
  假设前面 10 笔交易共消耗 2.5M CU
  budget = 2M - 2.5M = -0.5M < 0

  你的交易还没被扫描到, 调度循环已退出:
  while budget > 0  ← false, 退出

结果: 你的交易留在队列, 等待下一区块。
```

**时间影响**:
```
Slot 时间 ≈ 400ms
下一区块时间 = 400ms
上链延迟 = 至少 400ms
```

**概率估算**:
```
假设:
  - 区块容量: 60M CU
  - 平均交易 cost: 200K CU
  - 理论最大交易数: 300 笔

如果队列中有 500 笔交易, 你排第 350 位:
  你的被调度概率 ≈ 0%  (无论 priority 多高)
  需要等待 = ceil(350 / 300) = 2 个区块
  延迟 = 2 * 400ms = 800ms
```

### 2.3 场景三: 所有线程满载

**问题描述**: 交易标记为 `UnschedulableThread`, 无法调度。

**根本原因**: 每个执行线程的 CU 配额耗尽。

**代码位置**: prio_graph_scheduler.rs:134-147

```rust
let max_cu_per_thread = self.config.max_scheduled_cus / num_threads as u64;

for thread_id in 0..num_threads {
    if self.common.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
        >= max_cu_per_thread
    {
        schedulable_threads.remove(thread_id);
    }
}
if schedulable_threads.is_empty() {
    return Ok(SchedulingSummary { /* 没有调度任何交易 */ });
}
```

**示例** (4 线程):
```
max_cu_per_thread = 60M / 4 = 15M

当前状态:
  Thread 0: in_flight = 15M  ✗
  Thread 1: in_flight = 15M  ✗
  Thread 2: in_flight = 15M  ✗
  Thread 3: in_flight = 15M  ✗

schedulable_threads = {} (空集)
调度器直接返回, 不调度任何交易。
```

**何时发生**:
- 大量长时间执行的交易 (高 CU limit)
- 执行线程处理缓慢 (CPU 瓶颈)
- 区块快结束时 (留给新交易的空间少)

**对策**:
- 降低 `compute_unit_limit` (减少占用)
- 使用 `preflightCommitment: "confirmed"` 避免 pending 交易堆积

### 2.4 场景四: 优先级反转陷阱

**问题描述**: 高 priority 交易因低 priority 交易阻塞而无法调度。

**根本原因**: `blocking_locks` 机制保护优先级顺序。

**代码位置**: prio_graph_scheduler.rs:394-399

**示例场景**:

```
队列 (按 priority 排序):
  Tx1: priority=1000, 账户 [A, B]
  Tx2: priority=900,  账户 [B, C]
  Tx3: priority=800,  账户 [C, D]

当前执行中的交易 (其他线程):
  Thread 0: 正在执行 Tx_old, 持有账户 A 的 Write Lock

调度流程:
  1. 尝试调度 Tx1:
     - 需要 A (Write) → 冲突 (Tx_old 持有)
     - 无可用线程 → UnschedulableConflicts
     - blocking_locks.take_locks([A, B])  ← 记录 Tx1 的锁

  2. 尝试调度 Tx2:
     - 需要 B (Write)
     - blocking_locks.check_locks([B, C]) = false  ← B 被 Tx1 阻塞
     - UnschedulableConflicts
     - blocking_locks.take_locks([B, C])  ← 记录 Tx2 的锁

  3. 尝试调度 Tx3:
     - 需要 C (Write)
     - blocking_locks.check_locks([C, D]) = false  ← C 被 Tx2 阻塞
     - UnschedulableConflicts

结果:
  虽然 Tx3 与当前执行的交易无冲突, 但因为依赖链 (Tx1 → Tx2 → Tx3),
  整个链条都被阻塞。

影响: 如果 Tx1 长时间无法调度 (等待 Tx_old 完成), Tx2 和 Tx3 也被延迟,
      即使它们有足够高的 priority。
```

**量化影响**:
```
假设:
  - Tx_old 剩余执行时间: 50ms
  - 调度周期: 10ms/次

Tx1, Tx2, Tx3 都需要等待:
  延迟 = 50ms + 调度延迟
  相比无阻塞情况, 额外延迟 50ms
```

**对策**:
- **避免热门账户**: 选择冲突少的账户
- **错峰提交**: 避开高峰期 (slot 开始时)
- **拆分交易**: 减少单笔交易涉及的账户数

### 2.5 场景五: Compute Unit Limit = 0

**问题描述**: 交易被 PrioritizationFeeCache 过滤, 不参与 fee 统计。

**代码位置**: prioritization_fee_cache.rs:236-240

```rust
// filter out any transaction that requests zero compute_unit_limit
// since its priority fee amount is not instructive
if compute_budget_limits.compute_unit_limit == 0 {
    continue;  // ← 跳过此交易, 不更新 cache
}
```

**影响**:
1. 交易仍然可以执行 (如果通过其他检查)
2. 但不会影响 `get_recent_prioritization_fees()` 的结果
3. Priority 计算异常:
   ```rust
   priority = (reward * 1M) / (cost + 1)
   如果 cost 很小, priority 可能异常高
   ```

**实际案例**:
```
compute_unit_limit = 0
compute_unit_price = 1000

priority_fee = 0 * 1000 = 0
reward ≈ base_fee * 0.5 ≈ 2500
cost ≈ signature_cost + lock_cost ≈ 1000 CU
priority = 2500 * 1M / 1000 = 2_500_000

相比:
  compute_unit_limit = 200K
  compute_unit_price = 1000
  priority_fee = 200K * 1000 = 200M microlamports
  reward ≈ 200_000 lamports
  cost ≈ 202K CU
  priority = 200_000 * 1M / 202K = 990_099

结论: CU limit = 0 的交易 priority 不具可比性。
```

---

## 账户锁冲突场景分析

### 3.1 读写冲突矩阵

| 账户当前状态 | 新交易请求 Read | 新交易请求 Write |
|-------------|----------------|-----------------|
| **无锁** | ✅ 允许 | ✅ 允许 |
| **Read Lock (Thread A)** | ✅ 允许 (同线程或其他线程) | ❌ 冲突 |
| **Write Lock (Thread A)** | ❌ 冲突 | ❌ 冲突 |

### 3.2 多账户冲突示例

**场景**: 2 个线程, 3 个账户

```
当前锁状态:
  Thread 0: Write Lock on [A, B]
  Thread 1: Read Lock on [C]

待调度交易队列:
  Tx1: priority=1000, Read [A], Write [D]
  Tx2: priority=900,  Read [B, C], Write [E]
  Tx3: priority=800,  Write [C, F]
  Tx4: priority=700,  Read [D], Write [G]
```

**调度过程**:

```
1. 调度 Tx1:
   - Read A: 冲突 (Thread 0 持有 Write Lock on A)
   - 可用线程: None
   - 结果: UnschedulableConflicts
   - blocking_locks = {A, D}

2. 调度 Tx2:
   - Read B: 冲突 (Thread 0 持有 Write Lock on B)
   - 可用线程: None
   - 结果: UnschedulableConflicts
   - blocking_locks = {A, D, B, C, E}  ← 累加

3. 调度 Tx3:
   - Write C: 冲突 (Thread 1 持有 Read Lock on C)
   - 同时 C 在 blocking_locks 中 (被 Tx2 阻塞)
   - 结果: UnschedulableConflicts
   - blocking_locks = {A, D, B, C, E, F}

4. 调度 Tx4:
   - Read D: D 在 blocking_locks 中 (被 Tx1 阻塞)
   - blocking_locks.check_locks([D, G]) = false
   - 结果: UnschedulableConflicts

最终: 所有交易都因直接或间接冲突而无法调度。
```

**何时解除阻塞**:

```
Thread 0 完成当前交易, 释放 [A, B]:
  → Tx1 可调度 (Read A 无冲突)
  → Tx2 可调度 (Read B, C 无冲突)

Tx1 调度后:
  → blocking_locks 移除 [A, D]
  → Tx4 可能可调度 (如果 Tx1 已完成并释放 D)
```

### 3.3 死锁风险 (理论上)

**Solana 的调度器设计避免死锁**:
- 单向依赖: 依赖图是 DAG (有向无环图)
- 优先级顺序: 严格按 priority 调度
- 无锁等待: 不可调度的交易不会持有任何锁

**如果存在死锁 (不可能发生的反例)**:
```
Tx1: 持有 A, 等待 B
Tx2: 持有 B, 等待 A

Solana 调度器不会出现这种情况, 因为:
  - Tx 只有在 try_lock_accounts() 成功后才持有锁
  - try_lock_accounts() 是原子操作 (要么全部成功, 要么全部失败)
```

---

## 极端拥堵下的行为

### 4.1 拥堵定义

**量化指标**:
```
队列深度: 待调度交易数
  - 低负载: < 500 笔
  - 中负载: 500 - 2000 笔
  - 高负载: 2000 - 10000 笔
  - 极端拥堵: > 10000 笔

账户热度: 写入同一账户的交易数
  - 低热度: < 10 笔/账户
  - 中热度: 10 - 50 笔/账户
  - 高热度: 50 - 200 笔/账户
  - 极端热度: > 200 笔/账户
```

### 4.2 拥堵下的优先级分布

**场景**: 1万笔交易在队列, 争抢 300 个区块位置

**Priority 分布** (假设):
```
Priority 范围    | 交易数 | 百分位  | 上链概率
----------------|--------|---------|----------
10M+            | 50     | Top 0.5%| 100%
1M - 10M        | 450    | Top 5%  | 100%
100K - 1M       | 1500   | Top 20% | 50%
10K - 100K      | 3000   | Top 50% | 10%
< 10K           | 5000   | Bottom  | ~0%
```

**结论**: **只有 Top 5% priority 的交易有稳定上链保证。**

### 4.3 极端场景: 最高 Fee 也失败

**可能原因**:

#### 原因 1: 账户锁瓶颈

```
你的交易:
  priority = 100M (最高)
  写入账户: 热门 DEX 池

该账户已被 40 笔交易占满 (12M CU limit):
  → WouldExceedAccountMaxLimit
  → 即使 priority 最高也被拒绝

概率: 如果目标账户被写入 > 40 笔, 概率 = 100%
```

#### 原因 2: Block Budget 在你之前耗尽

```
你的交易 priority = 100M, 但队列中有 50 笔交易 priority > 100M:
  这 50 笔交易共消耗 59M CU
  你的交易 cost = 300K CU
  60M - 59M = 1M > 300K ✓ 理论可容纳

但是:
  调度器扫描限制 max_scanned = 1000
  如果你排在第 1200 位, 即使 priority 相对高, 也无法被扫描到

概率: 如果队列深度 > 1000 且你排在 1000 之后, 概率 ≈ 100%
```

#### 原因 3: 所有线程阻塞

```
极端情况: 前面的高 priority 交易全部因账户锁冲突而 unschedulable,
          导致 blocking_locks 包含大量账户。

你的交易即使 priority 最高, 如果涉及 blocking_locks 中的任何账户,
也会被阻塞。

概率: 取决于账户选择和网络状态, 一般 < 5%, 极端拥堵时可达 20%
```

### 4.4 拥堵下的时间延迟模型

**模型假设**:
```
队列深度: Q 笔交易
区块容量: C 笔交易/区块
你的排名: R (priority 排序)
Slot 时间: T_slot = 400ms
```

**期望上链时间**:
```
E[上链时间] = ceil(R / C) * T_slot

示例:
  Q = 5000, C = 300, R = 800
  E = ceil(800 / 300) * 400ms = 3 * 400ms = 1200ms = 1.2 秒
```

**方差** (考虑失败重试):
```
失败概率 p = (1 - C/Q) 如果 R > C
重试期望次数 = 1 / (1 - p)

示例:
  R = 800 (前 800 名), Q = 5000, C = 300
  如果前 300 名都成功, R = 800 需要等待 3 个区块
  但如果前面有交易失败 (账户锁冲突等), R 的位置会前移

  方差 σ² ≈ (Q/C - 1) * T_slot²
```

---

## 时间敏感错误

### 5.1 Blockhash 过期

**常量**: `MAX_PROCESSING_AGE` (solana_clock)

**典型值**: 150 个 slot

**过期时间**:
```
T_expire = MAX_PROCESSING_AGE * T_slot
         = 150 * 400ms
         = 60 秒
```

**检查位置**: receive_and_buffer.rs:276

**失败后果**:
- 交易被丢弃 (`num_dropped_on_age++`)
- 不会重试
- 客户端需要获取新 blockhash 并重新提交

**概率模型**:
```
假设交易在 t=0 时获取 blockhash, t=T_submit 时提交

如果 T_submit + 调度延迟 + 队列等待 > 60秒:
  → BlockhashNotFound

高峰期:
  队列等待 ≈ (R / C) * T_slot
  如果 R = 10000, C = 300:
    等待 = 10000 / 300 * 400ms = 13.3 秒  ✓ 安全

  但如果 R = 100000, C = 300:
    等待 = 100000 / 300 * 400ms = 133 秒 > 60 秒  ✗ 过期!
```

### 5.2 MaxAge 与 ALT 失效

**结构**: `MaxAge` (scheduler_messages.rs)
```rust
struct MaxAge {
    sanitized_epoch: Epoch,
    alt_invalidation_slot: Slot,
}
```

**计算**: receive_and_buffer.rs:561-571
```rust
fn calculate_max_age(
    sanitized_epoch: Epoch,
    deactivation_slot: Slot,
    current_slot: Slot,
) -> MaxAge {
    let alt_min_expire_slot = estimate_last_valid_slot(deactivation_slot.min(current_slot));
    MaxAge {
        sanitized_epoch,
        alt_invalidation_slot: alt_min_expire_slot,
    }
}
```

**作用**: 交易使用 Address Lookup Table (ALT) 时, 如果 ALT 被 deactivate,
         交易在 `alt_invalidation_slot` 之后失效。

**时间窗口**:
```
ALT deactivation 后的有效期 ≈ 512 slots ≈ 200 秒
```

**风险**: 如果交易在队列中等待超过 200 秒, 可能因 ALT 失效而无法执行。

---

## 关键常量速查表

| 常量 | 值 | 文件 | 含义 |
|------|------|------|------|
| `MAX_BLOCK_UNITS` | 60_000_000 | block_cost_limits.rs:28 | 区块最大 CU |
| `MAX_WRITABLE_ACCOUNT_UNITS` | 12_000_000 | block_cost_limits.rs:35 | 单账户最大 CU |
| `MAX_VOTE_UNITS` | 36_000_000 | block_cost_limits.rs:39 | 投票交易最大 CU |
| `MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA` | 100_000_000 | block_cost_limits.rs:43 | 区块最大数据增长 (bytes) |
| `MAX_PROCESSING_AGE` | 150 slots | solana_clock | Blockhash 有效期 |
| `MULTIPLIER` | 1_000_000 | receive_and_buffer.rs:537 | Priority 计算系数 |
| `COMPUTE_UNIT_TO_US_RATIO` | 30 | block_cost_limits.rs:8 | CU 到微秒转换 |
| `SIGNATURE_COST` | 720 | block_cost_limits.rs:10 | 签名验证成本 |
| `WRITE_LOCK_UNITS` | 300 | block_cost_limits.rs:20 | 写锁成本 |
| `look_ahead_window_size` | 256 | prio_graph_scheduler.rs:52 | 前瞻窗口大小 |
| `max_scanned_per_pass` | 1000 | prio_graph_scheduler.rs:51 | 单次扫描上限 |
| `target_transactions_per_batch` | 64 | prio_graph_scheduler.rs:53 | 目标批次大小 |

---

## 决策点速查

| 决策点 | 文件:行号 | 失败结果 |
|--------|----------|---------|
| Forward 判断 | receive_and_buffer.rs:244 | 丢弃, 不解析 |
| 解析 Sanitize | receive_and_buffer.rs:453 | 丢弃 |
| Vote-only Bank | receive_and_buffer.rs:468 | 丢弃 |
| 账户数限制 | receive_and_buffer.rs:472 | 丢弃 |
| ALT 解析 | receive_and_buffer.rs:491 | 丢弃 |
| 账户锁验证 | receive_and_buffer.rs:419 | 丢弃 |
| Compute Budget | receive_and_buffer.rs:428 | 丢弃 |
| Blockhash 年龄 | receive_and_buffer.rs:273 | 丢弃 |
| 重复交易 | receive_and_buffer.rs:291 | 丢弃 |
| Fee Payer 余额 | receive_and_buffer.rs:302 | 丢弃 |
| Budget 耗尽 | prio_graph_scheduler.rs:225 | 留队列 |
| 线程满载 | prio_graph_scheduler.rs:141 | 留队列 |
| Pre-graph Filter | prio_graph_scheduler.rs:196 | 丢弃 |
| 扫描限制 | prio_graph_scheduler.rs:304 | 留队列 |
| Blocking Locks | prio_graph_scheduler.rs:396 | 留队列 |
| 多线程冲突 | prio_graph_scheduler.rs:419 | 留队列 |
| Block Cost 限制 | cost_tracker.rs:169 | 拒绝执行 |
| Account Cost 限制 | cost_tracker.rs (内部) | 拒绝执行 |

---

**下一部分**: 时间窗口量化分析 - Slot 时间, 调度延迟, 网络负载影响, 最快上链时间计算。
