//! 时间类型。
//!
//! §7.1 的两条硬约束在这里落成类型，而不是留在注释里：
//!
//! 1. 壁上时钟（[`WallClock`]）**不是**因果排序依据。跨设备、跨机时不得用它假定全局
//!    先后，因此本 crate 不提供"按 `observed_at_utc` 排序"的工具方法。
//! 2. 单调时钟（[`Monotonic`]）**故意不实现 `Ord`**。只有 `boot_id` 相同时才允许比较，
//!    比较必须显式走 [`Monotonic::compare_same_boot`] 并处理 `None`。

use std::cmp::Ordering;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::{BootId, ContractError};

/// UTC 壁上时钟。
///
/// 序列化格式为 RFC 3339。它用来回答"这件事发生在什么时刻"，不用来回答"两件事谁先谁后"
/// ——后者必须用序列号或同 boot 的单调时钟。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WallClock(OffsetDateTime);

impl WallClock {
    /// 取当前 UTC 时间。
    pub fn now() -> Self {
        Self(OffsetDateTime::now_utc())
    }

    /// 从 RFC 3339 字符串解析。
    pub fn from_rfc3339(text: &str) -> Result<Self, ContractError> {
        OffsetDateTime::parse(text, &Rfc3339)
            .map(Self)
            .map_err(|_| ContractError::MalformedTimestamp {
                kind: "WallClock",
                actual: text.to_string(),
            })
    }

    /// 加若干秒。用于计算 TTL，不用于推断顺序。
    #[must_use]
    pub fn plus_seconds(self, seconds: i64) -> Self {
        Self(self.0 + Duration::seconds(seconds))
    }

    /// Unix 纪元以来的纳秒数，用于序列化与算术比较。
    pub fn unix_timestamp_nanos(self) -> i128 {
        self.0.unix_timestamp_nanos()
    }

    /// 取出底层 `time::OffsetDateTime`，供上层格式化与持久化使用。
    pub fn offset_date_time(self) -> OffsetDateTime {
        self.0
    }
}

impl std::fmt::Display for WallClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // OffsetDateTime 的取值范围保证 Rfc3339 格式化不会失败。
        match self.0.format(&Rfc3339) {
            Ok(text) => f.write_str(&text),
            Err(_) => f.write_str("<invalid-wall-clock>"),
        }
    }
}

impl Serialize for WallClock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let text = self
            .0
            .format(&Rfc3339)
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for WallClock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        OffsetDateTime::parse(&text, &Rfc3339)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// 单调时钟读数，绑定到一次 boot。
///
/// **故意不派生 `Ord` / `PartialOrd`**：跨 boot 的单调时间不可比（§7.1），编译器不应该
/// 让"顺手排个序"这种写法通过。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monotonic {
    /// 该读数所属的进程启动标识。
    pub boot_id: BootId,
    /// 自该次 boot 起的纳秒数。
    pub nanos: u64,
}

impl Monotonic {
    /// 构造一个读数。
    pub fn new(boot_id: BootId, nanos: u64) -> Self {
        Self { boot_id, nanos }
    }

    /// 仅当两次读数来自同一次 boot 时，返回二者间隔；否则返回 `None`。
    ///
    /// 返回 `None` 是正常结果，不是错误：调用方必须显式处理"不可比"，例如改用因果引用或
    /// 重新观测，而不是拿两个不同 boot 的单调值做减法。
    #[must_use]
    pub fn elapsed_since_same_boot(&self, earlier: &Self) -> Option<std::time::Duration> {
        if self.boot_id != earlier.boot_id {
            return None;
        }
        self.nanos
            .checked_sub(earlier.nanos)
            .map(std::time::Duration::from_nanos)
    }

    /// 仅当两次读数来自同一次 boot 时给出顺序；否则返回 `None`。
    #[must_use]
    pub fn compare_same_boot(&self, other: &Self) -> Option<Ordering> {
        if self.boot_id != other.boot_id {
            return None;
        }
        Some(self.nanos.cmp(&other.nanos))
    }
}

/// 半开还是闭区间不重要，重要的是它必须是合法的：`end` 严格晚于 `start`。
///
/// 预测的"时间窗"（§6.3）和概率的"时间范围"（§3.2）都用它，避免出现"预测在 0 秒内成立"
/// 这种无法证伪的写法。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeWindow {
    /// 窗口起点（含）。
    pub start: WallClock,
    /// 窗口终点（含）。
    pub end: WallClock,
}

impl TimeWindow {
    /// 构造并校验。
    pub fn new(start: WallClock, end: WallClock) -> Result<Self, ContractError> {
        if end <= start {
            return Err(ContractError::InvalidTimeWindow {
                start: start.to_string(),
                end: end.to_string(),
            });
        }
        Ok(Self { start, end })
    }

    /// 判断某一时刻是否落在窗口内。
    pub fn contains(&self, at: WallClock) -> bool {
        self.start <= at && at <= self.end
    }

    /// 窗口长度（秒）。
    pub fn seconds(&self) -> f64 {
        let delta = self.end.unix_timestamp_nanos() - self.start.unix_timestamp_nanos();
        delta as f64 / 1_000_000_000.0
    }
}
