//! SoCA 单主体认知单元运行时（主架构 §3.1、§4.1 L2、§4.2、§4.3）。
//!
//! | 模块 | 职责 | 对应条款 |
//! |---|---|---|
//! | [`map`] | 知识地图：自我定位、已知/未知分辨、前沿识别与已知地图上的路径规划 | §4.3"世界模型" |
//! | [`leaves`] | 首批叶单元：文件版本、动作前提、动作后验证 | §4.3"桌面与文件" |
//! | [`cluster`] | 能力簇：L2 黑板装配与子单元聚合 | §4.1 L2、§4.2 |
//!
//! **现状（如实说明）：**§4.3 那张表列了 8 簇 × 8 叶 = 64 个职责槽，目前实现的是其中
//! "桌面与文件"一簇里的三个槽，加上"世界模型"簇里的知识地图。其余槽位、L3 候选竞争、L6
//! 目标栈都还没有落地。
//!
//! 之所以不一次生成 64 个，是 §4.3 自己的要求："初期只实现首批有明确测试的单元，**不让尚无
//! 实际逻辑的角色充数**"。一个没有可证伪判据的角色，和一个空字符串没有区别。
//!
//! 这里不追求"看起来聪明"。判据是可复核的：**把知识地图删掉，行为必须退化**。做不到这一点
//! 就说明学习没有真的发生，只是策略凑巧有效。
//!
//! 本 crate 不做网络、不读用户文件、不调用模型。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cluster;
pub mod leaves;
pub mod map;

pub use crate::cluster::DesktopAndFilesCluster;
pub use crate::leaves::{ActionPrecondition, FileVersion, PostconditionVerify, Precondition};
pub use crate::map::{
    Cell, KnownCell, KnowledgeMap, MapUpdate, VIEW_AGENT_COLUMN, VIEW_AGENT_ROW,
};
