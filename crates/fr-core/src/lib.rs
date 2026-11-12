//! 核心逻辑：调用 git、解析输出、计算提交图布局、通信协议。不依赖界面，客户端和 kuaid 共用。

pub mod askpass;
pub mod blame;
pub mod dir;
pub mod exec;
pub mod git;
pub mod graph;
pub mod i18n;
pub mod ignore;
pub mod log;
pub mod patch;
pub mod protocol;
pub mod rebase;
pub mod refs;
pub mod show;
pub mod stash;
pub mod state;
pub mod status;
pub mod watch;
pub mod workfile;
