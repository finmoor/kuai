//! fork-remote 的核心逻辑：调用 git、解析输出、计算提交图布局。
//!
//! 这一层不依赖界面：本机仓库由客户端在进程内直接调用，服务器上的仓库由 agent 调用。

pub mod git;
pub mod graph;
pub mod log;
pub mod protocol;
