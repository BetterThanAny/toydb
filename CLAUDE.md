# toydb — Claude 工作约定

## 范围

- 这是一个**纯本地教学项目**：单一 crate，无网络层，无外部服务依赖
- 所有改动应能通过 `cargo test` + `cargo clippy -- -D warnings`

## 风格

- 模块结构以**关注点**分组，不以技术分层（`sql/` 而不是 `frontend/`，`storage/` 而不是 `backend/`）
- `Result<T, Error>` 走自定义 `crate::Error`（thiserror）；不要用 `anyhow` 在库代码里
- 测试紧贴模块：每个模块尾部 `#[cfg(test)] mod tests`；端到端测试放 `tests/`
- 公共 API 在 `lib.rs` 显式 `pub use`，不裸 `pub mod`
- 错误信息保留行号 / 列号 / 上下文片段（lexer / parser）

## SQL 方言

- 关键字大小写不敏感；标识符默认大小写敏感
- 字符串字面量用单引号 `'foo'`，双引号 `"col"` 是带引号标识符
- `--` 行注释；`/* */` 块注释
- 分号是语句结束符；REPL 中 SQL 多行时缺分号继续接收

## 不要做

- 不要引入未在 PLAN.md 列出的依赖。需要新依赖先在 PLAN.md 的「已安装工具」表里加一行
- 不要写 `unsafe` —— 这是教学项目，正确性 > 性能
- 不要在 commit message 里写 `Co-Authored-By: Claude` 等署名
- 不要 push 到任何 remote（项目是本地的）
- 不要因为 clippy 报错就直接 `#[allow(...)]`；先理解再决定

## 提交节奏

- 每个里程碑（M1, M2, ...）一个 commit
- commit message 第一行 ≤70 字符，jp/cn 都可，但全英文 commit 也可
- 完成里程碑前必须跑：`cargo test`、`cargo clippy --all-targets -- -D warnings`、`cargo fmt --check`

## 性能 vs 正确性

- 默认选**正确性**和**清晰**。性能优化（page 压缩、向量化、并行扫描）放到 M10 之后再考虑
- 不要为了避免 clone 而引入复杂的生命周期；初期 `Clone` 直接抄
