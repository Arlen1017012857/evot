# 工具层智能压缩：替代机械截断

> **状态**：设计草案 &nbsp;|&nbsp; **日期**：2026-05-05

---

## 1. 动机

当前上下文压缩管道（`src/engine/src/context/compaction/`）对工具输出采用**机械式截断**：

| 压缩层级 | 机制 | 问题 |
|:---------|:-----|:-----|
| 执行时 | 100KB 字节上限，头尾截断 | 丢失中间部分全部信息 |
| 溢出卸载 (spill) | 写入磁盘 + 4K 预览 | 模型只看到预览 |
| L1 `OversizeCapped` | 截断至 25–30 行 | 语义丢失严重 |
| L1 `AgeCleared` | 整个结果替换为 `[result cleared]` | 完全丢失 |

这些操作完全无视输出内容的**语义结构**。典型场景：`read_file` 返回 2000 行代码，关键逻辑恰好位于文件中间——被直接丢弃。

### 1.1 为什么不引入 Summarizer LLM？

另一种思路是调用一个小模型对过大的工具输出进行总结。但这存在以下问题：

| 问题 | 说明 |
|:-----|:-----|
| **延迟** | 额外 LLM 调用增加 2–5 秒，位于热路径上 |
| **可靠性** | Summarizer 可能遗漏关键细节或产生幻觉 |
| **成本** | 额外的 token 开销 |
| **上下文不足** | 压缩阶段发生在 LLM 调用**之前**，summary 看不到任务目标 |

---

## 2. 方案：工具层内联压缩

> **核心理念**：让工具在返回结果**之前**，根据输出内容的**语义结构**进行压缩，而不是等上下文膨胀后再机械截断。

### 2.1 方案对比

| 维度 | 工具层压缩 | Summarizer LLM | 现状（机械截断） |
|:-----|:-----------|:---------------|:-----------------|
| 延迟 | 0（纯本地计算） | +2–5s | 0 |
| 可靠性 | **确定性**，规则驱动 | 可能幻觉 / 遗漏 | 确定性但无知 |
| 信息保留 | 语义结构完整 | 取决于模型能力 | 随机丢失 |
| 额外依赖 | tree-sitter（项目已有） | 需要额外的 Provider | 无 |

---

## 3. 文件类型与压缩策略

### 3.1 代码文件

> `.rs` `.py` `.ts` `.go` `.java` `.c` `.cpp` `.swift` `.kt` …

**策略**：AST 结构大纲。

已有实现位于 `src/engine/src/context/compaction/phases/level1_shrink/outline.rs`，支持 20+ 种语言。需要从压缩阶段**移动**到 `ReadFileTool.execute()` 内部。

**输出示例**：

```text
[Structural outline of service.rs · 2000 lines]
fn new(config: Config) -> Self ...
fn process(&self, req: Request) -> Result<Response> ...
impl Service
  pub fn health_check(&self) -> Status ...
  async fn handle_message(&mut self, msg: Message) ...
```

**决策逻辑**：

| 条件 | 行为 |
|:-----|:-----|
| 文件 < 阈值（如 500 行 / 15KB） | 返回完整文本 |
| 文件 ≥ 阈值 | 返回 AST 大纲 |
| 小函数（≤ 3 行） | 不折叠，直接展示完整内容 |
| 不支持的语言 | 降级为头尾截断 |

### 3.2 Markdown

> `.md`

**策略**：标题树 + 首段文字。

```text
[Structure of README.md · 500 lines]

# Project Name
  首段概述文字...

## Installation
  安装步骤首段...

## Usage
  ### Basic Example
    示例代码块...
  ### Advanced Configuration
    配置说明首段...

## API Reference
  ### `GET /users`
  ### `POST /users`
  ### `DELETE /users/:id`
```

**实现**：正则匹配 `^#{1,6}\s`，每节保留首段（≤ 200 字符）。

### 3.3 JSON

> `.json`

**策略**：Key-path 类型大纲。

```text
[Structure of config.json · 800 lines]
{
  server: {
    host: string,
    port: number,
    tls: { enabled: boolean, cert: string, key: string }
  },
  databases: Array[3] of {
    name: string,
    url: string,
    pool_size: number
  },
  features: {
    flag_a: boolean,
    flag_b: boolean,
    ...
  }
}
```

**实现**：递归遍历前 N 层（如 5 层），数组显示长度和第一个元素的 keys，叶子显示类型。项目已有 `serde_json` 依赖。

### 3.4 海量日志

> `.log` / `bash` 输出含日志模式

**策略**：多层压缩管道——级别分布 → 错误提取 → 模式去重 → 采样。

```text
[Log summary: 45000 lines, 15:32:01 — 15:45:33]

Level distribution:  ERROR: 12  |  WARN: 45  |  INFO: 44943

── Errors (12) ──
15:32:15 ERROR connection pool exhausted (repeated 8 times)
15:32:15   last: timeout after 30s, pool_size=10
15:38:01 ERROR database migration failed: table "users_v2" already exists
15:42:10 ERROR OOM killer invoked: process_id=8842, memory=2.1GB
15:44:55 ERROR failed to connect upstream: 192.168.1.50:8080

── Warnings (45, deduplicated to 3 patterns) ──
15:32:10 WARN slow query: 2.3s SELECT * FROM orders        [pattern ×31]
15:38:05 WARN disk usage at 87% on /dev/sda1               [pattern ×8]
15:40:22 WARN retry attempt 3/5 for service:payment         [pattern ×6]

── Sample (first 20 + last 20) ──
[first 20 lines...]
...
[last 20 lines...]
```

**实现要点**：

1. 检测时间戳前缀（ISO 8601 / syslog 格式）
2. 关键词匹配：`ERROR|FATAL|CRITICAL|panic|fail|WARN|WARNING`
3. 模式去重：相邻相同模式合并为 `[pattern ×N]`
4. 保留首尾样本供 LLM 了解上下文

### 3.5 Diff 输出

> `git diff` / `bash`

**策略**：变更摘要 + 关键 diff。

```text
[Diff summary]
Files changed: 15 (+234, −89)

src/engine/src/context/compaction/orchestrator.rs   | +45 −12
src/engine/src/context/compaction/outline.rs        | +120 −0  (new)
src/engine/src/tools/file.rs                        | +32 −8
cli/src/prompt.ts                                   | +8 −3
Cargo.lock                                          | +18 −18 (skipped)

── Key changes ──
[展开代码文件 diff，跳过 lock / 二进制文件]
```

**实现**：检测 `diff --git` 模式 → 提取文件列表 + 增删计数 → 仅展开代码文件 diff（按扩展名过滤），跳过 `Cargo.lock`、`bun.lock`、`package-lock.json`、二进制等。

### 3.6 测试 / Build 输出

> `cargo test` / `bun test` / 编译输出

**策略**：失败优先。

```text
[Test results: 234 passed, 5 failed, 2 ignored]

── Failures ──
test engine::context::compaction_tests::oversize_cap ... FAILED
  assertion `left == right` failed
  left: 45, right: 44
  at src/engine/tests/context/compaction.rs:123

test engine::provider::stream_tests::timeout ... FAILED
  ...

[234 passed folded: test_a, test_b, ...]
```

**实现**：检测 `FAILED`、`failures:`、`error[`、`error:` 等模式。失败部分保留完整上下文（前后各 3 行），成功部分折叠为计数。

### 3.7 CSV / 表格数据

**策略**：列头 + 统计 + 采样。

```text
[table.csv: 50000 rows, 5 columns]
columns: id(int), name(string), value(float), category(string), ts(datetime)
sample:  (first 5) [row1, row2, ...]
         (last 5)  [row49996, ...]
stats:   name: 234 unique, value: range [0.1, 99.3], category: 12 unique
```

### 3.8 其他未知文本

**兜底策略**：头尾截断（保持现状）。

---

## 4. 实施架构

### 4.1 核心类型：`CompressionHint`

```rust
/// 压缩策略提示，由工具在返回结果前使用。
enum CompressionHint {
    /// 基于文件扩展名推断（如 read_file 已知路径）
    ByExtension { ext: String, path: String },
    /// 基于输出内容自动检测（bash 等未知来源）
    AutoDetect,
    /// 不压缩（LLM 明确要求完整输出）
    None,
}

/// 压缩后的工具输出。
struct CompressedOutput {
    /// 压缩后的文本（替代原始 content）
    text: String,
    /// 使用的压缩策略（供 UI 显示）
    method: CompressionMethod,
}
```

### 4.2 各工具集成点

| 工具 | 输入来源 | 策略 |
|:-----|:---------|:-----|
| `read_file` | 文件扩展名已知 | `ByExtension`：`.rs` → AST，`.md` → 标题树，`.json` → 结构大纲，`.log` → 日志压缩 |
| `bash` | 输出内容未知 | `AutoDetect`：检测 diff / log / test 特征，兜底降级为头尾截断 |
| `search` | 已有上限机制 | 保持现状（已有 `max_results` + 匹配计数） |
| `web_fetch` | HTTP `Content-Type` | `text/html` → 待定，`application/json` → JSON 大纲 |

### 4.3 `read_file` 决策流程

```text
read_file(path) ─────────────────────────────────────────────┐
  │                                                          │
  ├─ 图片文件 ──────────────→ 逻辑不变（直接返回）            │
  │                                                          │
  ├─ 文件 < 阈值 ───────────→ 返回完整文本                    │
  │   (≤ 500 行 / ≤ 15KB)                                    │
  │                                                          │
  ├─ offset/limit 已指定 ───→ 返回指定范围（不压缩）          │
  │                                                          │
  └─ 大文件 ─────────────────→ 按类型路由：                   │
        ├─ 代码文件 (.rs, .py, .ts, …) → AST 大纲            │
        ├─ .md                          → 标题树              │
        ├─ .json                        → Key-path 结构大纲   │
        ├─ .log                         → 日志压缩            │
        └─ 其他                         → 头尾截断（兜底）     │
```

### 4.4 `bash` 的 AutoDetect 流程

```text
bash 输出 ───────────────────────────────────────────────────┐
  │                                                          │
  ├─ 检测到 `diff --git` ──────────────→ 变更摘要             │
  ├─ 检测到 `FAILED` / test results ──→ 失败优先              │
  ├─ 检测到 `error[` / compiling ─────→ 构建错误提取           │
  ├─ 检测到 时间戳 + 日志级别 ────────→ 日志压缩               │
  └─ 以上均未匹配 ────────────────────→ 头尾截断（保持现状）    │
```

### 4.5 LLM 获取完整内容的逃生路径

LLM 可通过已有参数绕过压缩，获取完整输出：

| 方式 | 说明 |
|:-----|:-----|
| `read_file(path, offset=N, limit=M)` | 精确读取指定范围，**不压缩** |
| `read_file(path, no_compress=true)` | 强制完整返回（兜底，可能触发现有 1MB 上限） |
| `bash("cat large_file.log")` | 走 AutoDetect 压缩路径 |

---

## 5. 开放问题

### 5.1 `bash` AutoDetect 的可靠性

不需要完美，只需比机械截断好。检测不到模式 → 降级为头尾截断（保持现状），不会引入回退。

### 5.2 压缩粒度控制

LLM 能否通过工具参数控制压缩程度？

候选设计：

```text
compress: "outline" | "full" | "auto"    // 默认 "auto"
```

### 5.3 与现有压缩管道的关系

工具层压缩后，是否还需要 L1 Shrink？

| 阶段 | 策略 |
|:-----|:-----|
| **短期** | 共存。工具层压缩减少进入管道的 token 数，管道作为最后兜底 |
| **长期** | 如果工具层覆盖了主要大输出场景，可逐步简化 L1 Shrink |

### 5.4 阈值设定

多少行 / 字节触发压缩？

| 候选值 | 说明 |
|:-------|:-----|
| 500 行 / 15KB | 对标当前 `oversize_abs_tokens: 6000`（≈ 24KB 字符） |

> 需要根据实际使用数据进一步调优。

### 5.5 实现优先级

| 优先级 | 内容 | 说明 |
|:-------|:-----|:-----|
| **P0** | 代码文件 AST 大纲 + Markdown 标题树 | 移动已有代码，覆盖最高频场景 |
| **P1** | JSON 大纲 + 日志压缩 | 高频数据格式 |
| **P2** | Diff / 测试 AutoDetect | bash 输出场景 |
| **P3** | CSV 等 | 低频场景 |

### 5.6 现有解决方案调研

以下是社区中与本方案可比较的上下文压缩 / 过滤方案。

---

#### context-mode

> 仓库：[mksglu/context-mode](https://github.com/mksglu/context-mode)

**核心理念**："Think in Code"——鼓励 AI 写脚本来分析文件，而不是让 AI 直接读取大量文件内容。

**关键数据**：分析项目结构时，传统方式需要读 47 个文件，换成写一个分析脚本只需要执行 1 次，token 消耗从 **7,742 → 1,009**。

**架构特性**：

- 工具输出压缩后存入本地 SQLite 数据库，需要时通过全文搜索（FTS5 + BM25）找回
- 支持 11 种语言的代码执行：JS、TS、Python、Shell、Ruby、Go、Rust、PHP、Perl、R、Elixir
- 输出只返回 stdout，stderr 和执行细节不污染上下文

**六个沙盒工具**：

| 工具 | 功能 |
|:-----|:-----|
| `ctx_execute` | 运行代码，只把 stdout 放入上下文 |
| `ctx_batch_execute` | 一次调用执行多个命令 |
| `ctx_execute_file` | 在沙盒中处理文件 |
| `ctx_index` | 将内容分块建立 FTS5 索引（BM25 排序） |
| `ctx_search` | 按需检索已索引内容 |
| `ctx_fetch_and_index` | 抓取 URL → Markdown → 建索引（24h TTL 缓存） |

**与本方案的关系**：context-mode 的"执行脚本代替读文件"是一种**互补策略**——我们不一定要展开所有文件，可以让 LLM 写代码来提取关键信息。可考虑作为 `bash` 工具的一种推荐模式。

---

#### sqz

> 仓库：[ojuschugh1/sqz](https://github.com/ojuschugh1/sqz.git)

**核心理念**：透明多层级压缩管道，AI 工具完全无感知。

**压缩管道**：

```text
输入内容
  │
  ▼
[1] 逐命令格式化器  ──→  git / docker / npm 等定制压缩
  │
  ▼
[2] 结构性摘要      ──→  提取 imports + 签名 + 调用图
  │
  ▼
[3] 去重缓存 ⭐     ──→  SHA-256 哈希，重复输出 = 13 token
  │
  ▼
[4] JSON 管道       ──→  剥离 null / debug / 扁平化 / TOON
  │
  ▼
[5] 安全模式        ──→  熵分析跳过堆栈 / 密钥 / 错误信息
  │
  ▼
压缩输出
```

**透明 Hook 机制**（最优雅的设计）：

1. sqz 通过 `PreToolUse` Hook 拦截 AI 工具的命令执行
2. 当 Claude Code 执行 `git status` 时，Hook 在输出返回给 Claude **之前**介入
3. 将输出重写为压缩版本后返回
4. Claude 只看到一个更紧凑的 `git status` 输出，完全不知道这是被压缩过的

**与本方案的关系**：sqz 的透明 Hook 模式值得借鉴——我们的工具层压缩也可以做得对 LLM 透明，让 LLM 看到的始终是紧凑但语义完整的输出。但 sqz 的 SHA-256 去重（重复输出仅 13 token）是我们可以额外学习的优化手段。

---

#### Context Gateway

> 仓库：[Compresr-ai/Context-Gateway](https://github.com/Compresr-ai/Context-Gateway)

**核心理念**：利用小型语言模型（SLM）+ 训练分类器，在工具输出进入 LLM 上下文窗口之前进行智能压缩。

**技术创新**：

- 不是简单地丢弃信息，而是通过训练分类器识别信息中的"信号"
- 解决了 LLM 处理长上下文时精度下降和成本高昂的问题
- 在不牺牲模型性能的前提下，通过智能信息筛选优化 LLM 应用效率

**与本方案的关系**：本方案明确**不引入 SLM/LLM**（见 §1.1），因此与 Context Gateway 走的是相反的技术路线——我们追求确定性规则压缩，它追求模型驱动的智能压缩。二者可在不同场景互补。

---

#### rtk

> 仓库：[rtk-ai/rtk](https://github.com/rtk-ai/rtk)

**核心理念**：在命令输出到达 LLM 上下文之前进行过滤和压缩。

与本方案的 `bash` AutoDetect 路径类似，但 rtk 侧重过滤而非结构化压缩。可作为 AutoDetect 策略的参考实现。
