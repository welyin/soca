//! SoCA 单主体认知单元运行时（实施规格 §3.3，工单 ENG-06 的 SoCA 侧）。
//!
//! 本 crate 把"公开协议 + 规则宿主"接成**一个会积累知识的认知主体**：8 个职责叶单元、
//! 1 个能力簇、1 个主体，全部确定性、不调用模型。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`map`] | 知识地图：自我定位、已知/未知分辨、前沿识别与已知地图上的路径规划 |
//!
//! 这里不追求"看起来聪明"。判据是可复核的：**把知识地图删掉，行为必须退化**。做不到这一点
//! 就说明学习没有真的发生，只是策略凑巧有效。
//!
//! 本 crate 不做网络、不读用户文件、不调用模型。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod map;

pub use crate::map::{
    Cell, KnownCell, KnowledgeMap, MapUpdate, VIEW_AGENT_COLUMN, VIEW_AGENT_ROW,
};
