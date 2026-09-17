//! 通用校验小工具。
//!
//! §7.2 要求"引用必须能解析为存在且仍可访问的证据"。重复引用虽然能解析，但会让
//! 证据计数虚高（§3 的研究结论：重复读取及共享来源不能算成独立新证据），因此在契约
//! 层直接拒绝。

use crate::ContractError;

/// 断言引用列表内没有重复项。
///
/// `field` 用于错误信息，必须是信封或快照里的真实字段名。
pub fn assert_unique<T>(items: &[T], field: &'static str) -> Result<(), ContractError>
where
    T: Ord + std::fmt::Display + Clone,
{
    let mut sorted = items.to_vec();
    sorted.sort();

    for window in sorted.windows(2) {
        if window[0] == window[1] {
            let count = sorted.iter().filter(|item| **item == window[0]).count();
            return Err(ContractError::DuplicateRefs {
                field,
                count,
                sample: window[0].to_string(),
            });
        }
    }
    Ok(())
}

/// 断言两个引用列表没有交集。
///
/// 同一份证据同时支撑和反对同一假设，说明冲突没有被解决，不能把它当作"有支持的候选"
/// 提交给上层（§4.2：父单元不能只拼接子摘要或用多数意见覆盖矛盾）。
pub fn assert_disjoint<T>(left: &[T], right: &[T], field: &'static str) -> Result<(), ContractError>
where
    T: Ord + std::fmt::Display + Clone,
{
    let mut left_sorted = left.to_vec();
    left_sorted.sort();
    let mut right_sorted = right.to_vec();
    right_sorted.sort();

    for item in &right_sorted {
        if left_sorted.binary_search(item).is_ok() {
            return Err(ContractError::ContradictoryEvidence {
                sample: item.to_string(),
            });
        }
    }
    let _ = field;
    Ok(())
}
