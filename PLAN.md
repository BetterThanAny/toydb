# toydb — 实现计划

一个从零写起的 SQL 数据库引擎（教学用），**不**追求生产级性能，但在每一层做到结构清晰、可测试、可演进。

## 总目标

| 层 | 关键能力 |
|---|---|
| SQL 前端 | 词法分析、递归下降解析、AST、错误定位 |
| 类型系统 | NULL / Boolean / Integer / Float / String，类型转换规则 |
| 表达式引擎 | 算术、比较、逻辑、字符串拼接、IS NULL、IN |
| 存储引擎 | 抽象的 `Engine` trait（先 in-memory，再 disk-backed） |
| 执行器 | 投影、过滤、扫描、Join、Aggregate、Sort、Limit |
| 事务 | MVCC + 快照隔离，BEGIN/COMMIT/ROLLBACK |
| 持久化 | page 文件 + B-tree 索引 + WAL + 重启恢复 |
| 工具 | REPL、表格化输出、demo 脚本、benchmark |

## 里程碑

- **M0 项目骨架** — Cargo workspace、文档框架、git
- **M1 SQL 词法分析** — `Token`、`Lexer`，全部关键字 / 字面量 / 操作符 通过
- **M2 SQL 解析器** — AST + 递归下降，覆盖 DDL + DML + 事务语句
- **M3 类型 / Catalog** — `Value` enum、`DataType`、`Column`、`Table`，Catalog
- **M4 表达式引擎** — `Expression::eval(&Row, &Schema) -> Result<Value>`
- **M5 内存执行器** — 单表 CRUD 走通端到端
- **M6 REPL** — 可交互输入 SQL，得到表格化输出
- **M7 聚合/排序/Join** — `GROUP BY` / `ORDER BY` / `LIMIT` / `JOIN`
- **M8 持久化** — page-based 文件、buffer pool、WAL、recovery
- **M9 MVCC 事务** — 多版本 KV、可见性规则、写写冲突检测
- **M10 文档/demo/bench** — README、demo SQL、Criterion benchmark

## 验收命令

每个里程碑后都跑：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

最终 demo：

```bash
cargo run --release --bin toydb -- examples/world.sql
```

## 测试矩阵

| 类别 | 模块 | 工具 |
|---|---|---|
| 单元 | lexer / parser / executor 等 | `cargo test` 每模块内 `#[cfg(test)] mod tests` |
| 集成 | 从 SQL 字符串 → 结果 | `tests/sql_*.rs` |
| 持久化 | 起 / 停 / 重启 | `tests/persistence_*.rs` |
| 事务 | 并发隔离 | `tests/txn_*.rs` |

## 已安装工具

| 工具 | 安装命令 | 时间 | 原因 | 卸载命令 |
|---|---|---|---|---|
| rustc 1.95.0 | (已存在) | — | 编译器 | `rustup self uninstall` |
| cargo 1.95.0 | (已存在) | — | 包管理 | 同上 |

后续如新增 crate（`thiserror`、`rustyline`、`pretty_assertions`），通过 `cargo add` 写入，自动在 `Cargo.toml` 里追踪。

## 风险与边界

- 不追求 ANSI SQL 兼容；只覆盖核心语法
- 不实现完整 ACID 中的 D（崩溃恢复只做 redo，不做 fuzzy checkpoint）
- 不实现网络层；只做单机进程内库 + REPL
- B-tree 实现追求正确性而非速度，不做 latch crabbing
