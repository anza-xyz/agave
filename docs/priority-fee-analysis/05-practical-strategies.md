# Solana (Agave) Priority Fee 源码分析 - 实战策略框架

> **文档**: 第5部分 - Fee 估算、动态调整与优化策略

---

## 目录

1. [Priority Fee 估算算法](#priority-fee-估算算法)
2. [动态 Fee 调整策略](#动态-fee-调整策略)
3. [失败重试决策树](#失败重试决策树)
4. [成本效益优化模型](#成本效益优化模型)
5. [不同场景下的最佳实践](#不同场景下的最佳实践)

---

## Priority Fee 估算算法

### 1.1 基于 RPC 的历史 Fee 查询

**API**: `getRecentPrioritizationFees`

**代码位置**: rpc/src/rpc.rs:2386-2399

```typescript
// 查询最近 150 个区块的 fee 数据
const fees = await connection.getRecentPrioritizationFees({
  lockedWritableAccounts: [poolAddress], // 可选: 查询特定账户
});

// 返回格式:
// [
//   { slot: 12345, prioritizationFee: 5000 },
//   { slot: 12346, prioritizationFee: 6000 },
//   ...
// ]
```

**源码逻辑** (prioritization_fee_cache.rs:287-404):
```rust
// 维护最近 150 个区块的 fee 数据
const MAX_NUM_RECENT_BLOCKS: u64 = 150;

pub fn get_prioritization_fees(&self, pubkeys: &[Pubkey])
    -> Vec<(Slot, u64)>
{
    // 如果指定 pubkeys: 返回每个账户的最低 fee
    // 如果为空: 返回区块级别的最低 fee
}
```

### 1.2 百分位估算法

**算法**:
```python
def estimate_priority_fee(
    fees: List[int],
    target_percentile: float = 0.75,
    safety_margin: float = 1.2
) -> int:
    """
    基于历史 fee 数据估算

    Args:
        fees: 历史 prioritization fee 列表
        target_percentile: 目标百分位 (0-1)
        safety_margin: 安全边际系数

    Returns:
        建议的 prioritization fee (microlamports)
    """
    if not fees:
        return 0

    # 过滤异常值 (> 99.9 百分位视为异常)
    p999 = np.percentile(fees, 99.9)
    filtered_fees = [f for f in fees if f <= p999]

    # 计算目标百分位
    base_fee = np.percentile(filtered_fees, target_percentile * 100)

    # 应用安全边际
    recommended_fee = int(base_fee * safety_margin)

    return recommended_fee

# 使用示例
fees_data = [f['prioritizationFee'] for f in fees]
recommended_fee = estimate_priority_fee(fees_data, target_percentile=0.75)
```

**解释**:
- `target_percentile=0.75`: 期望超过 75% 的历史交易
- `safety_margin=1.2`: 再加 20% 缓冲, 应对负载波动
- 过滤异常值: 避免被极端高 fee 扭曲

### 1.3 账户特定 Fee 估算

**针对热门账户** (如 DEX 池):

```python
def estimate_account_specific_fee(
    account: str,
    connection: Connection,
    multiplier: float = 1.5
) -> int:
    """
    为特定账户估算 priority fee

    热门账户竞争激烈, 需要更高 fee
    """
    # 查询该账户的历史 fee
    fees = connection.get_recent_prioritization_fees(
        lockedWritableAccounts=[account]
    )

    fees_data = [f['prioritizationFee'] for f in fees]

    # 使用更高百分位和倍数
    base_fee = estimate_priority_fee(
        fees_data,
        target_percentile=0.90,  # 90 百分位
        safety_margin=multiplier  # 1.5x
    )

    return base_fee

# 使用
dex_pool = "RaydiumPoolAddress..."
fee = estimate_account_specific_fee(dex_pool, connection, multiplier=2.0)
```

### 1.4 实时负载调整

**监控队列深度** (如果可获取):

```python
def adaptive_fee_estimate(
    base_fee: int,
    queue_depth: int,  # 待处理交易数
    block_capacity: int = 300,  # 区块容量
    urgency: str = "normal"  # "low", "normal", "high", "urgent"
) -> int:
    """
    根据实时负载动态调整 fee
    """
    load_ratio = queue_depth / block_capacity

    # 负载系数
    if load_ratio < 1:
        load_multiplier = 1.0
    elif load_ratio < 3:
        load_multiplier = 1.2
    elif load_ratio < 10:
        load_multiplier = 1.5
    else:
        load_multiplier = 2.0

    # 紧急程度系数
    urgency_multipliers = {
        "low": 0.8,
        "normal": 1.0,
        "high": 1.3,
        "urgent": 2.0
    }

    final_fee = int(
        base_fee *
        load_multiplier *
        urgency_multipliers.get(urgency, 1.0)
    )

    return final_fee
```

### 1.5 基于成本优化的 Fee 设置

**优化目标**: 最小化总成本 = Priority Fee + (失败概率 * 重试成本)

```python
def cost_optimized_fee(
    base_fee: int,
    value_at_risk: float,  # 交易价值 (SOL)
    retry_cost: float,     # 重试的机会成本 (SOL)
    success_probability_fn: Callable[[int], float]  # Fee -> 成功概率函数
) -> int:
    """
    基于成本效益优化的 fee 设置
    """
    # 搜索空间: base_fee 的 0.5x 到 5x
    fee_candidates = [int(base_fee * m) for m in [0.5, 0.8, 1.0, 1.2, 1.5, 2.0, 3.0, 5.0]]

    best_fee = base_fee
    min_expected_cost = float('inf')

    for fee in fee_candidates:
        # 成功概率 (简化模型)
        p_success = success_probability_fn(fee)

        # 期望成本 = Fee成本 + 失败概率 * (重试成本 + 价值损失)
        fee_cost = fee / 1e6  # microlamports to SOL
        failure_cost = (1 - p_success) * (retry_cost + value_at_risk * 0.1)  # 假设失败损失10%价值
        expected_cost = fee_cost + failure_cost

        if expected_cost < min_expected_cost:
            min_expected_cost = expected_cost
            best_fee = fee

    return best_fee

# 成功概率模型 (基于排名)
def success_probability_model(fee: int, historical_fees: List[int]) -> float:
    """
    根据 fee 在历史分布中的位置估算成功概率
    """
    if not historical_fees:
        return 0.5

    percentile = sum(1 for f in historical_fees if f < fee) / len(historical_fees)

    # 简化模型: percentile 越高, 成功概率越大
    # 假设区块容量为 Top 30%
    if percentile >= 0.70:  # Top 30%
        return 0.95
    elif percentile >= 0.50:  # Top 50%
        return 0.70
    elif percentile >= 0.30:  # Top 70%
        return 0.40
    else:
        return 0.10
```

---

## 动态 Fee 调整策略

### 2.1 初始 Fee 设置

**流程图**:
```
开始
  ↓
查询历史 Fee (getRecentPrioritizationFees)
  ↓
计算 75 百分位 Fee
  ↓
应用场景调整:
  - 热门账户? → * 1.5
  - 高拥堵? → * 1.5
  - 紧急? → * 2.0
  ↓
设置初始 compute_unit_price
  ↓
提交交易
```

**代码示例**:
```typescript
async function setInitialFee(
  connection: Connection,
  accounts: PublicKey[],
  urgency: "normal" | "high" = "normal"
): Promise<number> {
  // 1. 查询历史 fee
  const fees = await connection.getRecentPrioritizationFees({
    lockedWritableAccounts: accounts,
  });

  // 2. 计算基础 fee
  const feeValues = fees.map(f => f.prioritizationFee);
  const p75 = percentile(feeValues, 75);

  // 3. 应用调整
  let computeUnitPrice = p75;

  if (urgency === "high") {
    computeUnitPrice *= 2.0;
  }

  // 4. 设置下限 (避免为 0)
  computeUnitPrice = Math.max(computeUnitPrice, 1);  // 至少 1 microlamport/CU

  return computeUnitPrice;
}
```

### 2.2 动态加价策略

**场景**: 交易失败或长时间未确认

**策略矩阵**:

| 失败原因 | 是否加价 | 加价幅度 | 其他操作 |
|---------|---------|---------|---------|
| BlockhashNotFound | ❌ | - | 重新获取 blockhash, 重试 |
| AlreadyProcessed | ❌ | - | 停止 (已成功) |
| AccountInUse (锁冲突) | ✅ | +50% | 等待 1-2 slot 后重试 |
| WouldExceedAccountMaxLimit | ❌ | - | 等待下一区块, 或放弃 |
| WouldExceedBlockMaxLimit | ✅ | +30% | 立即重试 |
| 超时 (未确认) | ✅ | +50% | 重新提交 |
| 其他错误 | ✅ | +20% | 检查错误, 修复后重试 |

**实现**:
```typescript
class DynamicFeeManager {
  private basePrice: number;
  private currentPrice: number;
  private retryCount: number = 0;
  private readonly maxRetries: number = 5;

  constructor(basePrice: number) {
    this.basePrice = basePrice;
    this.currentPrice = basePrice;
  }

  onFailure(error: TransactionError): {
    shouldRetry: boolean;
    newPrice: number
  } {
    this.retryCount++;

    if (this.retryCount > this.maxRetries) {
      return { shouldRetry: false, newPrice: this.currentPrice };
    }

    let multiplier = 1.0;
    let shouldRetry = true;

    switch (error.code) {
      case "BlockhashNotFound":
        // 不加价, 但需要新 blockhash
        shouldRetry = true;
        multiplier = 1.0;
        break;

      case "AlreadyProcessed":
        // 已成功, 不重试
        shouldRetry = false;
        break;

      case "AccountInUse":
        // 账户锁冲突, 加价并等待
        multiplier = 1.5;
        break;

      case "WouldExceedAccountMaxLimit":
        // 账户 CU 限制, 加价无效, 等待下一区块
        shouldRetry = true;
        multiplier = 1.0;
        // 延迟 400ms (1 slot)
        break;

      case "Timeout":
        // 超时未确认, 积极加价
        multiplier = 1.5;
        break;

      default:
        // 其他错误, 适度加价
        multiplier = 1.2;
    }

    this.currentPrice = Math.floor(this.currentPrice * multiplier);

    return {
      shouldRetry,
      newPrice: this.currentPrice
    };
  }

  onSuccess(): void {
    // 成功后, 降低下次的基础价格 (学习)
    this.basePrice = Math.max(
      this.basePrice * 0.9,
      1  // 最低 1 microlamport
    );
  }
}
```

### 2.3 指数退避与上限

**防止无限加价**:

```typescript
function exponentialBackoff(
  initialPrice: number,
  attempt: number,
  maxPrice: number = 100000  // 最大 100,000 microlamports/CU
): number {
  const backoffMultiplier = Math.pow(1.5, attempt);
  const newPrice = initialPrice * backoffMultiplier;

  return Math.min(newPrice, maxPrice);
}

// 使用
let price = 1000;  // 初始
price = exponentialBackoff(price, 1, 50000);  // 第 1 次重试: 1500
price = exponentialBackoff(price, 2, 50000);  // 第 2 次重试: 2250
price = exponentialBackoff(price, 3, 50000);  // 第 3 次重试: 3375
// ...
price = exponentialBackoff(price, 10, 50000); // 第 10 次: 50000 (达到上限)
```

---

## 失败重试决策树

### 3.1 完整决策树

```
交易提交
  ↓
等待确认 (timeout: 30-60秒)
  ↓
  ├─ 成功? → 结束
  │
  └─ 失败/超时
      ↓
      检查错误类型
      ↓
      ├─ BlockhashNotFound
      │   ↓
      │   获取新 blockhash
      │   ↓
      │   重新签名
      │   ↓
      │   提交 (不加价)
      │
      ├─ AlreadyProcessed
      │   ↓
      │   结束 (已成功)
      │
      ├─ AccountInUse (锁冲突)
      │   ↓
      │   等待 1-2 slot (400-800ms)
      │   ↓
      │   加价 50%
      │   ↓
      │   重新提交
      │
      ├─ WouldExceedAccountMaxLimit
      │   ↓
      │   判断: 重试次数 < 3?
      │   ├─ 是: 等待 1 slot, 重试
      │   └─ 否: 放弃
      │
      ├─ WouldExceedBlockMaxLimit
      │   ↓
      │   加价 30%
      │   ↓
      │   立即重试
      │
      ├─ Timeout (未确认)
      │   ↓
      │   检查交易状态 (getSignatureStatuses)
      │   ↓
      │   ├─ 仍 pending: 继续等待或加价重试
      │   ├─ 已确认: 结束
      │   └─ 未找到: 加价 50%, 重新提交
      │
      └─ 其他错误
          ↓
          分析错误:
          ├─ 程序逻辑错误 → 修复交易逻辑, 重试
          ├─ 余额不足 → 充值后重试
          ├─ 账户不存在 → 创建账户, 重试
          └─ 未知错误 → 加价 20%, 最多重试 3 次
```

### 3.2 实现

```typescript
async function retryWithStrategy(
  connection: Connection,
  transaction: Transaction,
  wallet: Wallet,
  options: {
    maxRetries: number,
    initialTimeout: number,
    onProgress?: (status: string) => void
  }
): Promise<string> {
  let feeManager = new DynamicFeeManager(
    await setInitialFee(connection, transaction.instructions[0].keys.map(k => k.pubkey))
  );

  for (let attempt = 0; attempt < options.maxRetries; attempt++) {
    try {
      options.onProgress?.(`尝试 ${attempt + 1}/${options.maxRetries}...`);

      // 设置 priority fee
      const computeBudgetIx = ComputeBudgetProgram.setComputeUnitPrice({
        microLamports: feeManager.getCurrentPrice()
      });
      transaction.instructions.unshift(computeBudgetIx);

      // 提交交易
      const signature = await wallet.sendTransaction(transaction, connection);

      // 等待确认
      const confirmation = await connection.confirmTransaction(
        signature,
        "confirmed"
      );

      if (confirmation.value.err) {
        throw new Error(confirmation.value.err.toString());
      }

      // 成功
      feeManager.onSuccess();
      return signature;

    } catch (error) {
      options.onProgress?.(`失败: ${error.message}`);

      // 分析错误并决定是否重试
      const { shouldRetry, newPrice } = feeManager.onFailure(error);

      if (!shouldRetry || attempt === options.maxRetries - 1) {
        throw new Error(`交易失败, 已重试 ${attempt + 1} 次: ${error.message}`);
      }

      // 特殊处理: blockhash 过期
      if (error.message.includes("BlockhashNotFound")) {
        const { blockhash } = await connection.getLatestBlockhash();
        transaction.recentBlockhash = blockhash;
        transaction.sign(wallet.payer);
      }

      // 特殊处理: 账户锁冲突
      if (error.message.includes("AccountInUse")) {
        await sleep(400);  // 等待 1 slot
      }

      // 特殊处理: 账户 CU 限制
      if (error.message.includes("WouldExceedAccountMaxLimit")) {
        await sleep(400);  // 等待下一区块
      }

      // 继续下一次重试
    }
  }

  throw new Error(`交易失败, 已达最大重试次数`);
}
```

### 3.3 何时放弃

**放弃条件**:
1. **达到最大重试次数** (建议 3-5 次)
2. **Blockhash 即将过期** (剩余 < 10 slots)
3. **成本超过预期** (累计 fee > 预设阈值)
4. **账户状态无法恢复** (余额不足, 账户不存在等)
5. **网络极端拥堵** (队列深度 > 100K, 预计等待 > 10 分钟)

**决策逻辑**:
```typescript
function shouldGiveUp(
  attempt: number,
  maxRetries: number,
  totalFeePaid: number,
  maxBudget: number,
  slotsElapsed: number,
  queueDepth: number
): boolean {
  if (attempt >= maxRetries) return true;
  if (totalFeePaid > maxBudget) return true;
  if (slotsElapsed > 140) return true;  // Blockhash 快过期 (150 - 10 margin)
  if (queueDepth > 100000) return true;  // 极端拥堵

  return false;
}
```

---

## 成本效益优化模型

### 4.1 总成本函数

**定义**:
```
总成本 = Priority Fee + 预期失败成本

Priority Fee = compute_unit_limit * compute_unit_price / 1e6  (SOL)

预期失败成本 = P(失败) * (重试成本 + 机会成本 + 价值损失)

其中:
  P(失败) = 1 - P(成功)
  P(成功) = f(priority_rank, queue_depth, account_contention)
  重试成本 = base_fee + 时间成本
  机会成本 = 延迟 * 价值 * 时间价值率
  价值损失 = 交易价值 * 失败损失率
```

### 4.2 最优 Fee 搜索

**目标**: 最小化总成本

**算法** (网格搜索):
```python
def find_optimal_fee(
    base_fee: int,
    transaction_value: float,  # SOL
    time_value_rate: float = 0.001,  # 每秒价值损失率
    historical_fees: List[int],
    queue_depth: int = 1000
) -> int:
    """
    搜索最优 priority fee
    """
    # 搜索空间: base_fee 的 0.5x 到 10x
    fee_range = np.linspace(base_fee * 0.5, base_fee * 10, 50)

    best_fee = base_fee
    min_total_cost = float('inf')

    for fee in fee_range:
        # 估算成功概率
        p_success = estimate_success_probability(
            fee, historical_fees, queue_depth
        )

        # Priority fee 成本
        fee_cost = fee / 1e6  # microlamports to SOL

        # 预期延迟
        expected_delay = estimate_delay(fee, historical_fees, queue_depth)

        # 机会成本
        opportunity_cost = expected_delay * transaction_value * time_value_rate

        # 失败成本
        failure_cost = (1 - p_success) * (
            base_fee / 1e6 +  # 重试 fee
            transaction_value * 0.05  # 假设失败损失 5%
        )

        # 总成本
        total_cost = fee_cost + opportunity_cost + failure_cost

        if total_cost < min_total_cost:
            min_total_cost = total_cost
            best_fee = int(fee)

    return best_fee

def estimate_success_probability(
    fee: int,
    historical_fees: List[int],
    queue_depth: int
) -> float:
    """
    基于 fee 在历史分布中的排名估算成功概率
    """
    if not historical_fees:
        return 0.5

    # 计算百分位
    percentile = sum(1 for f in historical_fees if f < fee) / len(historical_fees)

    # 估算排名
    estimated_rank = (1 - percentile) * queue_depth

    # 区块容量假设
    block_capacity = 300

    # 成功概率模型
    if estimated_rank < block_capacity * 0.5:
        return 0.98  # 前 50% 容量, 几乎确定成功
    elif estimated_rank < block_capacity:
        return 0.85  # 前 100% 容量, 高概率成功
    elif estimated_rank < block_capacity * 2:
        return 0.50  # 需等待 1 区块
    elif estimated_rank < block_capacity * 5:
        return 0.30  # 需等待 2-4 区块
    else:
        return 0.10  # 需等待 5+ 区块, 可能超时

def estimate_delay(
    fee: int,
    historical_fees: List[int],
    queue_depth: int
) -> float:
    """
    估算延迟 (秒)
    """
    percentile = sum(1 for f in historical_fees if f < fee) / len(historical_fees)
    estimated_rank = (1 - percentile) * queue_depth

    block_capacity = 300
    slots_to_wait = max(1, int(np.ceil(estimated_rank / block_capacity)))

    delay_seconds = slots_to_wait * 0.4  # 400ms per slot

    return delay_seconds
```

### 4.3 成本效益图示

**示例场景**:
```
Transaction Value = 10 SOL
Base Fee = 5000 microlamports/CU
CU Limit = 200,000
Queue Depth = 5000
Historical Fees = [1000, 2000, ..., 50000]
```

**成本分解**:

| Fee (μL/CU) | Priority Fee (SOL) | P(Success) | Exp. Delay (s) | Opp. Cost (SOL) | Failure Cost (SOL) | Total Cost (SOL) |
|------------|-------------------|------------|----------------|-----------------|-------------------|-----------------|
| 1,000 | 0.0002 | 0.10 | 6.4 | 0.064 | 0.45 | 0.514 |
| 5,000 | 0.001 | 0.50 | 3.2 | 0.032 | 0.25 | 0.283 |
| 10,000 | 0.002 | 0.85 | 0.8 | 0.008 | 0.075 | 0.085 ⭐ |
| 20,000 | 0.004 | 0.98 | 0.4 | 0.004 | 0.01 | 0.018 |
| 50,000 | 0.01 | 0.99 | 0.4 | 0.004 | 0.005 | 0.019 |

**最优解**: Fee = 10,000 μL/CU (总成本最小)

**观察**:
- 低 fee: 失败成本高, 总成本高
- 适中 fee: 平衡成功率和 fee 成本
- 高 fee: fee 成本高, 但失败成本低, 不一定最优

---

## 不同场景下的最佳实践

### 5.1 场景分类

| 场景 | 特征 | 优先级 | 时间敏感度 | 价值 |
|------|------|--------|-----------|------|
| 日常转账 | 低频, 简单 | 低 | 低 | 低 |
| Token 交易 | 中频, 标准 | 中 | 中 | 中 |
| DEX Swap | 高频, 竞争 | 高 | 高 | 高 |
| NFT Mint | 爆发, 极竞争 | 极高 | 极高 | 高 |
| Liquidation | 关键, 时间敏感 | 极高 | 极高 | 极高 |
| 批量操作 | 大量, 可延迟 | 低-中 | 低 | 总价值高 |

### 5.2 策略矩阵

#### 场景 1: 日常转账

**策略**:
```typescript
const strategy = {
  initialFee: "median",  // 使用中位数 fee
  maxRetries: 2,
  retryMultiplier: 1.2,
  timeout: 60_000,  // 60 秒
  commitment: "confirmed"
};

// 实现
const baseFee = await getMedianFee(connection);
const computeUnitPrice = Math.max(baseFee, 1);

const instruction = ComputeBudgetProgram.setComputeUnitPrice({
  microLamports: computeUnitPrice
});
```

**预期**:
- 上链时间: 1-5 秒
- 成本: 最低
- 成功率: 85-95%

#### 场景 2: DEX Swap (竞争激烈)

**策略**:
```typescript
const strategy = {
  initialFee: "p90",  // 90 百分位
  accountSpecific: true,  // 针对 DEX 池账户
  maxRetries: 3,
  retryMultiplier: 1.5,
  timeout: 30_000,  // 30 秒
  preflightCheck: false,  // 跳过预检 (加快速度)
  commitment: "processed"  // 最快确认
};

// 实现
const poolAccount = "DexPoolAddress...";
const fees = await connection.getRecentPrioritizationFees({
  lockedWritableAccounts: [poolAccount]
});

const p90 = percentile(fees.map(f => f.prioritizationFee), 90);
const computeUnitPrice = Math.floor(p90 * 1.5);  // 再加 50%

const instruction = ComputeBudgetProgram.setComputeUnitPrice({
  microLamports: computeUnitPrice
});
```

**预期**:
- 上链时间: 400-800ms
- 成本: 中-高
- 成功率: 95-98%

#### 场景 3: NFT Mint (极端拥堵)

**策略**:
```typescript
const strategy = {
  initialFee: "p95_x2",  // 95 百分位 * 2
  maxRetries: 5,
  retryMultiplier: 2.0,  // 激进加价
  timeout: 20_000,
  multipleSubmissions: true,  // 向多个 RPC 提交
  commitment: "processed"
};

// 实现
const fees = await connection.getRecentPrioritizationFees();
const p95 = percentile(fees.map(f => f.prioritizationFee), 95);
const computeUnitPrice = Math.floor(p95 * 2.0);

// 向多个 RPC 节点并行提交
const signatures = await Promise.allSettled([
  submitToRPC(transaction, rpc1),
  submitToRPC(transaction, rpc2),
  submitToRPC(transaction, rpc3)
]);

// 使用最快确认的
const firstSuccess = signatures.find(s => s.status === "fulfilled");
```

**预期**:
- 上链时间: 400-2000ms
- 成本: 高-极高
- 成功率: 50-80% (极端拥堵下)

#### 场景 4: 批量操作 (成本敏感)

**策略**:
```typescript
const strategy = {
  initialFee: "p25",  // 25 百分位 (低 fee)
  batchSize: 10,  // 每批 10 笔
  delayBetweenBatches: 2000,  // 批次间延迟 2 秒
  maxRetries: 1,
  timeout: 120_000,  // 2 分钟
  commitment: "confirmed"
};

// 实现
const fees = await connection.getRecentPrioritizationFees();
const p25 = percentile(fees.map(f => f.prioritizationFee), 25);
const computeUnitPrice = Math.max(p25, 1);

// 批量处理
for (let i = 0; i < transactions.length; i += strategy.batchSize) {
  const batch = transactions.slice(i, i + strategy.batchSize);

  await Promise.all(
    batch.map(tx => submitTransaction(tx, computeUnitPrice))
  );

  await sleep(strategy.delayBetweenBatches);
}
```

**预期**:
- 上链时间: 5-30 秒/笔
- 成本: 最低
- 成功率: 70-90%

### 5.3 监控与调整

**实时监控指标**:
```typescript
interface TransactionMetrics {
  successRate: number;      // 成功率
  avgConfirmTime: number;   // 平均确认时间 (ms)
  avgFee: number;           // 平均 fee (SOL)
  costPerSuccess: number;   // 每笔成功交易的成本 (SOL)
  retryRate: number;        // 重试率
}

class MetricsCollector {
  private metrics: TransactionMetrics[] = [];

  record(transaction: {
    success: boolean,
    confirmTime: number,
    fee: number,
    retries: number
  }): void {
    this.metrics.push({
      success: transaction.success ? 1 : 0,
      confirmTime: transaction.confirmTime,
      fee: transaction.fee,
      retries: transaction.retries
    });
  }

  analyze(): TransactionMetrics {
    const successCount = this.metrics.filter(m => m.success).length;
    const avgConfirmTime = average(this.metrics.map(m => m.confirmTime));
    const avgFee = average(this.metrics.map(m => m.fee));
    const totalCost = sum(this.metrics.map(m => m.fee));
    const costPerSuccess = totalCost / successCount;
    const retryRate = average(this.metrics.map(m => m.retries));

    return {
      successRate: successCount / this.metrics.length,
      avgConfirmTime,
      avgFee,
      costPerSuccess,
      retryRate
    };
  }

  shouldAdjustStrategy(metrics: TransactionMetrics): {
    adjust: boolean,
    direction: "increase" | "decrease",
    reason: string
  } {
    // 成功率低 → 增加 fee
    if (metrics.successRate < 0.85) {
      return {
        adjust: true,
        direction: "increase",
        reason: "成功率低于 85%"
      };
    }

    // 成功率高且成本高 → 降低 fee
    if (metrics.successRate > 0.95 && metrics.costPerSuccess > targetCost * 1.5) {
      return {
        adjust: true,
        direction: "decrease",
        reason: "成功率高, 可降低成本"
      };
    }

    // 确认时间长 → 增加 fee
    if (metrics.avgConfirmTime > 5000) {  // > 5 秒
      return {
        adjust: true,
        direction: "increase",
        reason: "确认时间过长"
      };
    }

    return { adjust: false, direction: "increase", reason: "" };
  }
}
```

---

## 总结与关键要点

### 关键结论

1. **Priority Fee 不是万能的**
   - 账户锁冲突: 无法通过加价解决
   - 区块 CU 限制: 超过限制后加价无效
   - 极端拥堵: 需等待拥堵缓解

2. **最优 Fee 是动态的**
   - 取决于网络负载
   - 取决于目标账户热度
   - 取决于时间敏感度

3. **成本效益平衡**
   - 不是越高越好
   - 存在收益递减点
   - 需根据场景优化

4. **重试策略很关键**
   - 不同错误需不同处理
   - 设置合理的放弃条件
   - 避免无限重试浪费资源

### 实战 Checklist

✅ **提交前**:
- [ ] 查询最近 fee (getRecentPrioritizationFees)
- [ ] 针对目标账户设置 fee
- [ ] 设置 compute_unit_limit (避免过高)
- [ ] 检查 blockhash 新鲜度

✅ **提交时**:
- [ ] 使用可靠的 RPC 节点
- [ ] 考虑多节点并行提交 (关键交易)
- [ ] 设置合理的 timeout
- [ ] 记录交易 signature

✅ **提交后**:
- [ ] 监控交易状态
- [ ] 准备重试逻辑
- [ ] 根据错误动态调整 fee
- [ ] 记录 metrics 用于优化

✅ **优化**:
- [ ] 分析成功率和成本
- [ ] 调整 fee 估算算法
- [ ] 优化重试策略
- [ ] 避免热门账户 (如可能)

---

## 附录: 代码模板库

完整的 TypeScript/Python 实现请参考:
- [Priority Fee Manager (TS)](./code-templates/priority-fee-manager.ts)
- [Dynamic Retry Strategy (TS)](./code-templates/retry-strategy.ts)
- [Fee Optimization (Python)](./code-templates/fee-optimization.py)
- [Metrics Collector (TS)](./code-templates/metrics-collector.ts)

---

**研究完成日期**: 2025-11-20
**基于版本**: Agave commit `bc45720`
**维护**: 定期更新以反映 Agave 代码变化
