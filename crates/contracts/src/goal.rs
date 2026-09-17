//! L6 动机与目标：受托目标栈、探索配额、人工审批（§4.1 L6、§2、§4.2、§6 第 9 步）。
//!
//! §4.1 给 L6 的一句话是：
//!
//! > L6动机与目标 | 受托目标栈、探索配额、人工审批 | **主体拥有目标，子目标受委托约束**
//!
//! 而 §2 把边界钉死在反面：
//!
//! > 第一版不承诺：**自主产生正确长期目标**、无需审批的任意系统操作、无界自我修改、
//! > 永久不遗忘、最优资源分配或人类级开放世界泛化。
//!
//! 所以本模块最重要的一条不是"怎么表示目标"，而是**"自主产生的目标表示不出来"**：
//! [`Goal::delegated`] 强制要求出处是用户明确通道（[`crate::Provenance::is_instruction_authority`]）。
//! 屏幕文字、麦克风转写、文档内容、模型输出统统不是指令来源，因此用它们构造一个根目标会
//! 在解析期失败。这条约束如果只写在文档里，它就是一个"记得别这么做"的约定；写成构造函数
//! 的参数检查，它才是一条规则。
//!
//! 另外三条同样可判定：
//!
//! * **子目标不能放宽权限。** §12.2"授权不给子单元自动扩大"。
//! * **子目标不能凭空生出额度。** 分配给全部子目标的额度之和不得超过父目标的额度。
//! * **递归有深度上限。** §4.2："禁止任务递归无限生成子任务，默认调用深度 4、每任务最多
//!   32 次单元激活，之后请求预算升级或返回部分结果。"

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::validate::assert_unique;
use crate::{
    ContractError, GoalId, PermissionScope, Provenance, SubjectId, UnitId, WallClock,
};

/// 目标树的最大深度（§4.2："默认调用深度 4"）。
pub const MAX_GOAL_DEPTH: u8 = 4;

/// 每个目标允许的最大单元激活次数（§4.2："每任务最多 32 次单元激活"）。
pub const MAX_ACTIVATIONS_PER_GOAL: u32 = 32;

/// 一个目标栈允许同时保存的最大目标数。
///
/// 文档没有给这个数字。取这个值是为了让"目标栈有界"成为一条能测的性质，而不是等内存
/// 涨起来才发现——所以它是工程选择，不是文档条款。
pub const MAX_GOALS: usize = 256;

/// 目标的生命周期状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    /// 已提交，尚未受理。
    Proposed,
    /// 正在推进。
    Active,
    /// 卡在人工审批上（§12.1）。
    WaitingApproval,
    /// 缺证据或缺权限，需要外部输入才能继续（§6 第 9 步的"请求澄清"）。
    Blocked,
    /// 已达成。终态。
    Satisfied,
    /// 已放弃：用户取消、预算耗尽或上层目标结束（§6 第 9 步）。终态。
    Abandoned,
}

impl GoalState {
    /// 全部状态。
    pub const ALL: [Self; 6] = [
        Self::Proposed,
        Self::Active,
        Self::WaitingApproval,
        Self::Blocked,
        Self::Satisfied,
        Self::Abandoned,
    ];

    /// 稳定名称，用于审计记录。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::WaitingApproval => "waiting_approval",
            Self::Blocked => "blocked",
            Self::Satisfied => "satisfied",
            Self::Abandoned => "abandoned",
        }
    }

    /// 是否已经结束。
    ///
    /// 终态不可复活。要让一个已放弃的目标重新推进，必须重新委托一个新目标——否则
    /// "取消"就只是"暂时停一下"，而 §15 要求任务取消是可依赖的。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Satisfied | Self::Abandoned)
    }

    /// 是否允许迁移到 `next`。
    pub fn can_transition_to(self, next: Self) -> bool {
        use GoalState::{Abandoned, Active, Blocked, Proposed, Satisfied, WaitingApproval};

        if self == next || self.is_terminal() {
            return false;
        }
        matches!(
            (self, next),
            (_, Satisfied | Abandoned)
                | (Proposed, Active)
                | (Active, WaitingApproval | Blocked)
                | (WaitingApproval, Active | Blocked)
                | (Blocked, Active)
        )
    }
}

/// 目标的出处。
///
/// 只有两种来源，而且都必须能指认到具体的人或具体的父目标。没有第三种"主体自己想到的"。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GoalOrigin {
    /// 由用户在明确通道上委托。
    Delegated {
        /// 委托的那条输入。
        provenance: Provenance,
    },
    /// 由父目标拆分而来（§4.2："父单元输出包含子结果……"的组织形式）。
    Decomposed {
        /// 父目标。
        parent: GoalId,
        /// 拆分它的单元。
        by: UnitId,
    },
}

impl GoalOrigin {
    /// 稳定名称，用于审计记录。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Delegated { .. } => "delegated",
            Self::Decomposed { .. } => "decomposed",
        }
    }
}

/// 目标的额度。
///
/// 每一项都同时记"允许多少"和"已经用了多少"：只记上限的话，判断"还剩多少"就得去别处
/// 汇总，而汇总口径一旦不一致，额度就成了建议。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalBudget {
    /// 最多允许多少次动作。
    pub max_actions: u32,
    /// 最多允许多少次单元激活（上限见 [`MAX_ACTIVATIONS_PER_GOAL`]）。
    pub max_activations: u32,
    /// 最多消耗多少 token。
    pub max_tokens: u32,
    /// 墙钟上限（毫秒）。
    pub max_wall_millis: u64,
    /// 已用动作数。
    pub spent_actions: u32,
    /// 已用激活数。
    pub spent_activations: u32,
    /// 已用 token。
    pub spent_tokens: u32,
    /// 已用墙钟（毫秒）。
    pub spent_wall_millis: u64,
}

impl GoalBudget {
    /// 构造一份新额度，已用量清零。
    pub fn new(
        max_actions: u32,
        max_activations: u32,
        max_tokens: u32,
        max_wall_millis: u64,
    ) -> Result<Self, ContractError> {
        if max_activations > MAX_ACTIVATIONS_PER_GOAL {
            return Err(ContractError::GoalBudgetExceeded {
                field: "goal.budget.max_activations",
                limit: MAX_ACTIVATIONS_PER_GOAL as usize,
                actual: max_activations as usize,
            });
        }
        Ok(Self {
            max_actions,
            max_activations,
            max_tokens,
            max_wall_millis,
            spent_actions: 0,
            spent_activations: 0,
            spent_tokens: 0,
            spent_wall_millis: 0,
        })
    }

    /// 剩余激活次数。
    pub fn remaining_activations(&self) -> u32 {
        self.max_activations.saturating_sub(self.spent_activations)
    }

    /// 剩余动作数。
    pub fn remaining_actions(&self) -> u32 {
        self.max_actions.saturating_sub(self.spent_actions)
    }

    /// 额度是否已经用完。
    pub fn is_exhausted(&self) -> bool {
        self.remaining_activations() == 0
            || self.remaining_actions() == 0
            || self.spent_tokens >= self.max_tokens
            || self.spent_wall_millis >= self.max_wall_millis
    }

    /// 记一次单元激活。
    ///
    /// 耗尽即拒，而不是"记下来然后继续"：§4.2 要求超过上限后"请求预算升级或返回部分结果"，
    /// 两条路都需要一个明确的拒绝点作为触发器。
    pub fn activate(&mut self) -> Result<(), ContractError> {
        if self.remaining_activations() == 0 {
            return Err(ContractError::GoalBudgetExceeded {
                field: "goal.budget.max_activations",
                limit: self.max_activations as usize,
                actual: self.spent_activations.saturating_add(1) as usize,
            });
        }
        self.spent_activations = self.spent_activations.saturating_add(1);
        Ok(())
    }

    /// 记若干次动作。
    pub fn spend_actions(&mut self, count: u32) -> Result<(), ContractError> {
        let projected = self.spent_actions.saturating_add(count);
        if projected > self.max_actions {
            return Err(ContractError::GoalBudgetExceeded {
                field: "goal.budget.max_actions",
                limit: self.max_actions as usize,
                actual: projected as usize,
            });
        }
        self.spent_actions = projected;
        Ok(())
    }

    /// 记若干 token。
    pub fn spend_tokens(&mut self, count: u32) -> Result<(), ContractError> {
        let projected = self.spent_tokens.saturating_add(count);
        if projected > self.max_tokens {
            return Err(ContractError::GoalBudgetExceeded {
                field: "goal.budget.max_tokens",
                limit: self.max_tokens as usize,
                actual: projected as usize,
            });
        }
        self.spent_tokens = projected;
        Ok(())
    }

    /// 记若干墙钟毫秒。
    pub fn spend_wall_millis(&mut self, count: u64) -> Result<(), ContractError> {
        let projected = self.spent_wall_millis.saturating_add(count);
        if projected > self.max_wall_millis {
            return Err(ContractError::GoalBudgetExceeded {
                field: "goal.budget.max_wall_millis",
                limit: usize::try_from(self.max_wall_millis).unwrap_or(usize::MAX),
                actual: usize::try_from(projected).unwrap_or(usize::MAX),
            });
        }
        self.spent_wall_millis = projected;
        Ok(())
    }
}

/// 探索配额（§4.1 L6 点名的第二项）。
///
/// 探索指的是"申请观测、做一次试验"这类不直接推进目标、但可能减少不确定性的动作。
/// 它与动作额度是**两个独立的桶**：探索花的是动作额度，但另有次数上限。合成一个桶的话，
/// 一个目标要么根本不敢探索，要么探索到把动作额度用光。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorationQuota {
    /// 最多允许多少次探索。
    pub max_explorations: u32,
    /// 已用次数。
    pub used: u32,
}

impl ExplorationQuota {
    /// 构造。
    pub fn new(max_explorations: u32) -> Self {
        Self {
            max_explorations,
            used: 0,
        }
    }

    /// 剩余次数。
    pub fn remaining(&self) -> u32 {
        self.max_explorations.saturating_sub(self.used)
    }

    /// 是否还允许探索。
    pub fn may_explore(&self) -> bool {
        self.remaining() > 0
    }

    /// 记一次探索。
    pub fn spend(&mut self) -> Result<(), ContractError> {
        if !self.may_explore() {
            return Err(ContractError::ExplorationQuotaExhausted {
                limit: self.max_explorations as usize,
                actual: self.used.saturating_add(1) as usize,
            });
        }
        self.used = self.used.saturating_add(1);
        Ok(())
    }
}

/// 一个受托目标。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    /// 标识。
    pub goal_id: GoalId,
    /// 归属主体。§13.3：每主体拥有自己的目标。
    pub owner: SubjectId,
    /// 目标陈述。
    pub statement: String,
    /// 出处。
    pub origin: GoalOrigin,
    /// 父目标。根目标为 `None`。
    pub parent: Option<GoalId>,
    /// 深度。根目标为 0。
    pub depth: u8,
    /// 状态。
    pub state: GoalState,
    /// 权限范围。子目标的等级不得超过父目标。
    pub permission_scope: PermissionScope,
    /// 额度。
    pub budget: GoalBudget,
    /// 探索配额。
    pub exploration: ExplorationQuota,
    /// 委托时刻。
    pub created_at: WallClock,
    /// 截止时间。不得晚于父目标。
    pub deadline: Option<WallClock>,
}

impl Goal {
    /// 构造一个由用户委托的根目标。
    ///
    /// `provenance` 必须是用户明确通道。其它任何出处——屏幕文字、麦克风转写、文档内容、
    /// 工具输出、模型派生——都会在这里被拒绝。这是 §2"第一版不承诺自主产生正确长期目标"
    /// 在代码里的样子。
    #[allow(clippy::too_many_arguments)]
    pub fn delegated(
        goal_id: GoalId,
        owner: SubjectId,
        statement: impl Into<String>,
        provenance: Provenance,
        permission_scope: PermissionScope,
        budget: GoalBudget,
        exploration: ExplorationQuota,
        created_at: WallClock,
        deadline: Option<WallClock>,
    ) -> Result<Self, ContractError> {
        if !provenance.is_instruction_authority() {
            return Err(ContractError::GoalNotDelegated {
                provenance: provenance_kind(&provenance),
            });
        }
        let statement = statement.into();
        if statement.trim().is_empty() {
            return Err(ContractError::EmptyField {
                field: "goal.statement",
            });
        }
        let goal = Self {
            goal_id,
            owner,
            statement,
            origin: GoalOrigin::Delegated { provenance },
            parent: None,
            depth: 0,
            state: GoalState::Proposed,
            permission_scope,
            budget,
            exploration,
            created_at,
            deadline,
        };
        goal.validate()?;
        Ok(goal)
    }

    /// 从父目标拆分出子目标。
    ///
    /// 三条约束一条都不能少：
    /// * 深度不得超过 [`MAX_GOAL_DEPTH`]（§4.2 禁止无限递归生成子任务）；
    /// * 权限等级不得放宽（§12.2"授权不给子单元自动扩大"）；
    /// * 截止时间不得晚于父目标——一个比父目标活得更久的子目标，会在父目标结束后继续
    ///   占用额度，而 §6 第 9 步要求"计划外动作不继续后台执行"。
    ///
    /// 额度由 [`GoalStack::decompose`] 统一分配，因为只有栈知道已经分出去多少。
    #[allow(clippy::too_many_arguments)]
    pub fn decomposed(
        goal_id: GoalId,
        parent: &Goal,
        statement: impl Into<String>,
        by: UnitId,
        permission_scope: PermissionScope,
        budget: GoalBudget,
        exploration: ExplorationQuota,
        created_at: WallClock,
        deadline: Option<WallClock>,
    ) -> Result<Self, ContractError> {
        let depth = parent.depth.saturating_add(1);
        if depth > MAX_GOAL_DEPTH {
            return Err(ContractError::GoalDepthExceeded {
                limit: MAX_GOAL_DEPTH as usize,
                actual: depth as usize,
            });
        }
        if permission_scope.max_action_level > parent.permission_scope.max_action_level {
            return Err(ContractError::GoalPermissionWidened {
                parent_level: parent.permission_scope.max_action_level.as_str(),
                child_level: permission_scope.max_action_level.as_str(),
            });
        }
        if let (Some(parent_deadline), Some(child_deadline)) = (parent.deadline, deadline)
            && child_deadline > parent_deadline
        {
            return Err(ContractError::InvalidTimeWindow {
                start: parent_deadline.to_string(),
                end: child_deadline.to_string(),
            });
        }

        let statement = statement.into();
        if statement.trim().is_empty() {
            return Err(ContractError::EmptyField {
                field: "goal.statement",
            });
        }
        let goal = Self {
            goal_id,
            owner: parent.owner.clone(),
            statement,
            origin: GoalOrigin::Decomposed {
                parent: parent.goal_id.clone(),
                by,
            },
            parent: Some(parent.goal_id.clone()),
            depth,
            state: GoalState::Proposed,
            permission_scope,
            budget,
            exploration,
            created_at,
            deadline,
        };
        goal.validate()?;
        Ok(goal)
    }

    /// 校验自洽性。
    ///
    /// **不检查截止时间是否已过。** 一个截止时间已过的目标是完全正常的东西——它是*过期*了，
    /// 不是*不自洽*。把过期当成畸形，会让一个持久化的目标栈在截止时间到点之后再也读不回来，
    /// 而那正是"长期未完成的目标"最常见的样子。过期由 [`Goal::is_expired_at`] 回答。
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.statement.trim().is_empty() {
            return Err(ContractError::EmptyField {
                field: "goal.statement",
            });
        }
        if self.budget.max_activations > MAX_ACTIVATIONS_PER_GOAL {
            return Err(ContractError::GoalBudgetExceeded {
                field: "goal.budget.max_activations",
                limit: MAX_ACTIVATIONS_PER_GOAL as usize,
                actual: self.budget.max_activations as usize,
            });
        }
        if self.depth > MAX_GOAL_DEPTH {
            return Err(ContractError::GoalDepthExceeded {
                limit: MAX_GOAL_DEPTH as usize,
                actual: self.depth as usize,
            });
        }
        // 根目标必须由用户委托，子目标必须有父目标且是拆分出来的。
        match (&self.origin, self.parent.as_ref()) {
            (GoalOrigin::Delegated { provenance }, None) => {
                if !provenance.is_instruction_authority() {
                    return Err(ContractError::GoalNotDelegated {
                        provenance: provenance_kind(provenance),
                    });
                }
                // 委托来的目标一定是根目标。有父目标的委托目标不是"更一般的情况"，
                // 而是一条绕过拆分约束（权限收窄、额度分配）的路径。
                if self.depth != 0 {
                    return Err(ContractError::GoalDepthExceeded {
                        limit: 0,
                        actual: self.depth as usize,
                    });
                }
            }
            (GoalOrigin::Decomposed { parent, .. }, Some(declared)) => {
                if parent != declared {
                    return Err(ContractError::FailClosed(
                        "goal.origin.parent 与 goal.parent 指向不同的目标",
                    ));
                }
                if self.depth == 0 {
                    return Err(ContractError::GoalDepthExceeded {
                        limit: MAX_GOAL_DEPTH as usize,
                        actual: 0,
                    });
                }
            }
            _ => {
                return Err(ContractError::FailClosed(
                    "目标的出处与父子关系不一致：委托来的目标没有父目标，拆分来的目标必须有",
                ));
            }
        }
        Ok(())
    }

    /// 在给定时刻是否已经超过截止时间。
    ///
    /// 过期不改变目标的状态：它可以继续推进，只是每推进一步都该被问一次"还要不要继续"。
    /// 自动把它标成 `Abandoned` 会让"我刚回来，任务就没了"变成默认行为。
    pub fn is_expired_at(&self, at: WallClock) -> bool {
        self.deadline.is_some_and(|deadline| deadline <= at)
    }

    /// 是否已经结束。
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// 记一次单元激活。
    pub fn activate(&mut self) -> Result<(), ContractError> {
        if self.state.is_terminal() {
            return Err(ContractError::GoalNotActive {
                goal_id: self.goal_id.to_string(),
                state: self.state.as_str(),
            });
        }
        self.budget.activate()
    }
}

/// 目标的出处类别名，用于错误信息（不携带出处内容本身）。
fn provenance_kind(provenance: &Provenance) -> &'static str {
    match provenance {
        Provenance::User { .. } => "user",
        Provenance::Sensor { .. } => "sensor",
        Provenance::Derived { .. } => "derived",
        Provenance::Tool { .. } => "tool",
    }
}

/// 受托目标栈。
///
/// 一个主体一个栈（§13.3："每主体拥有自己的完整目标、belief、邮箱、权限及记忆域"）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalStack {
    owner: SubjectId,
    goals: BTreeMap<GoalId, Goal>,
}

impl GoalStack {
    /// 建立一个空栈。
    pub fn new(owner: SubjectId) -> Self {
        Self {
            owner,
            goals: BTreeMap::new(),
        }
    }

    /// 归属主体。
    pub fn owner(&self) -> &SubjectId {
        &self.owner
    }

    /// 目标总数。
    pub fn len(&self) -> usize {
        self.goals.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.goals.is_empty()
    }

    /// 读一个目标。
    pub fn goal(&self, goal_id: &GoalId) -> Option<&Goal> {
        self.goals.get(goal_id)
    }

    /// 可变读一个目标。
    pub fn goal_mut(&mut self, goal_id: &GoalId) -> Option<&mut Goal> {
        self.goals.get_mut(goal_id)
    }

    /// 全部目标，按键序。
    pub fn iter(&self) -> impl Iterator<Item = &Goal> {
        self.goals.values()
    }

    /// 尚未结束的目标。
    pub fn open_goals(&self) -> Vec<&Goal> {
        self.goals
            .values()
            .filter(|goal| !goal.is_terminal())
            .collect()
    }

    /// 某个目标的直接子目标。
    pub fn children_of(&self, parent: &GoalId) -> Vec<&Goal> {
        self.goals
            .values()
            .filter(|goal| goal.parent.as_ref() == Some(parent))
            .collect()
    }

    /// 从根到某个目标的路径（含该目标）。
    pub fn path_to(&self, goal_id: &GoalId) -> Vec<&Goal> {
        let mut path = Vec::new();
        let mut cursor = self.goals.get(goal_id);
        while let Some(goal) = cursor {
            path.push(goal);
            cursor = goal.parent.as_ref().and_then(|parent| self.goals.get(parent));
        }
        path.reverse();
        path
    }

    /// 委托一个根目标。
    #[allow(clippy::too_many_arguments)]
    pub fn delegate(
        &mut self,
        goal_id: GoalId,
        statement: impl Into<String>,
        provenance: Provenance,
        permission_scope: PermissionScope,
        budget: GoalBudget,
        exploration: ExplorationQuota,
        at: WallClock,
        deadline: Option<WallClock>,
    ) -> Result<(), ContractError> {
        self.ensure_capacity()?;
        let goal = Goal::delegated(
            goal_id.clone(),
            self.owner.clone(),
            statement,
            provenance,
            permission_scope,
            budget,
            exploration,
            at,
            deadline,
        )?;
        self.goals.insert(goal_id, goal);
        Ok(())
    }

    /// 从父目标拆分出子目标，并分配额度。
    ///
    /// 这是"子目标受委托约束"里**约束**二字的落点：父目标持有的额度是有限的，分出去的
    /// 总额度不得超过它。没有这条检查，子目标可以凭空要求更多资源，而"受托"就退化成了
    /// 一句礼貌用语。
    #[allow(clippy::too_many_arguments)]
    pub fn decompose(
        &mut self,
        goal_id: GoalId,
        parent_id: &GoalId,
        statement: impl Into<String>,
        by: UnitId,
        permission_scope: PermissionScope,
        budget: GoalBudget,
        exploration: ExplorationQuota,
        at: WallClock,
        deadline: Option<WallClock>,
    ) -> Result<(), ContractError> {
        self.ensure_capacity()?;
        let Some(parent) = self.goals.get(parent_id) else {
            return Err(ContractError::FailClosed("父目标不在栈里"));
        };
        if parent.is_terminal() {
            return Err(ContractError::GoalNotActive {
                goal_id: parent.goal_id.to_string(),
                state: parent.state.as_str(),
            });
        }

        // 已经分给别的子目标的额度。
        let granted_activations: u32 = self
            .children_of(parent_id)
            .iter()
            .map(|child| child.budget.max_activations)
            .sum();
        let granted_actions: u32 = self
            .children_of(parent_id)
            .iter()
            .map(|child| child.budget.max_actions)
            .sum();
        let granted_tokens: u32 = self
            .children_of(parent_id)
            .iter()
            .map(|child| child.budget.max_tokens)
            .sum();

        for (field, granted, requested, ceiling) in [
            (
                "goal.budget.max_activations",
                granted_activations,
                budget.max_activations,
                parent.budget.max_activations,
            ),
            (
                "goal.budget.max_actions",
                granted_actions,
                budget.max_actions,
                parent.budget.max_actions,
            ),
            (
                "goal.budget.max_tokens",
                granted_tokens,
                budget.max_tokens,
                parent.budget.max_tokens,
            ),
        ] {
            let projected = granted.saturating_add(requested);
            if projected > ceiling {
                return Err(ContractError::GoalBudgetExceeded {
                    field,
                    limit: ceiling as usize,
                    actual: projected as usize,
                });
            }
        }

        let child = Goal::decomposed(
            goal_id.clone(),
            parent,
            statement,
            by,
            permission_scope,
            budget,
            exploration,
            at,
            deadline,
        )?;
        self.goals.insert(goal_id, child);
        Ok(())
    }

    /// 迁移一个目标的状态。
    pub fn transition(&mut self, goal_id: &GoalId, next: GoalState) -> Result<(), ContractError> {
        let Some(goal) = self.goals.get_mut(goal_id) else {
            return Err(ContractError::FailClosed("目标不在栈里"));
        };
        if !goal.state.can_transition_to(next) {
            return Err(ContractError::LifecycleViolation {
                from: goal.state.as_str(),
                to: next.as_str(),
            });
        }
        goal.state = next;
        Ok(())
    }

    /// 放弃一个目标及其**全部后代**，返回被放弃的数量。
    ///
    /// §6 第 9 步："结束后能力簇解散临时队伍，单元转温/冷态，**计划外动作不继续后台执行**。"
    /// 只把父目标标记为结束而留着子目标继续跑，正是这句话要禁的情形——子目标会继续消耗
    /// 额度、继续提出候选，而它的存在依据已经没有了。
    pub fn abandon(&mut self, goal_id: &GoalId) -> Result<usize, ContractError> {
        if !self.goals.contains_key(goal_id) {
            return Err(ContractError::FailClosed("目标不在栈里"));
        }

        // 先收集整棵子树，再统一改状态：边遍历边改会让遍历顺序影响结果。
        let mut doomed: Vec<GoalId> = vec![goal_id.clone()];
        let mut cursor = 0;
        while cursor < doomed.len() {
            let current = doomed[cursor].clone();
            for child in self.children_of(&current) {
                doomed.push(child.goal_id.clone());
            }
            cursor += 1;
        }

        let mut abandoned = 0;
        for id in &doomed {
            if let Some(goal) = self.goals.get_mut(id)
                && !goal.is_terminal()
            {
                goal.state = GoalState::Abandoned;
                abandoned += 1;
            }
        }
        Ok(abandoned)
    }

    /// 记一次单元激活。
    pub fn activate(&mut self, goal_id: &GoalId) -> Result<(), ContractError> {
        let Some(goal) = self.goals.get_mut(goal_id) else {
            return Err(ContractError::FailClosed("目标不在栈里"));
        };
        goal.activate()
    }

    /// 记一次探索。额度在两个桶里都要扣。
    pub fn explore(&mut self, goal_id: &GoalId) -> Result<(), ContractError> {
        let Some(goal) = self.goals.get_mut(goal_id) else {
            return Err(ContractError::FailClosed("目标不在栈里"));
        };
        goal.exploration.spend()?;
        goal.budget.activate()
    }

    fn ensure_capacity(&self) -> Result<(), ContractError> {
        if self.goals.len() >= MAX_GOALS {
            return Err(ContractError::GoalLimitExceeded {
                limit: MAX_GOALS,
                actual: self.goals.len() + 1,
            });
        }
        Ok(())
    }

    /// 校验整个栈：每个子目标的权限不得宽于父目标，深度与父子关系自洽。
    pub fn validate(&self) -> Result<(), ContractError> {
        let mut ids: Vec<GoalId> = Vec::new();
        for goal in self.goals.values() {
            goal.validate()?;
            if goal.owner != self.owner {
                return Err(ContractError::FailClosed(
                    "目标栈里混入了别的所有者的目标（§13.3）",
                ));
            }
            if let Some(parent_id) = &goal.parent {
                let Some(parent) = self.goals.get(parent_id) else {
                    return Err(ContractError::FailClosed("子目标的父目标不在栈里"));
                };
                if goal.permission_scope.max_action_level > parent.permission_scope.max_action_level
                {
                    return Err(ContractError::GoalPermissionWidened {
                        parent_level: parent.permission_scope.max_action_level.as_str(),
                        child_level: goal.permission_scope.max_action_level.as_str(),
                    });
                }
                if goal.depth != parent.depth.saturating_add(1) {
                    return Err(ContractError::GoalDepthExceeded {
                        limit: parent.depth.saturating_add(1) as usize,
                        actual: goal.depth as usize,
                    });
                }
                // 父目标已经结束，子目标却还开着：§6 第 9 步不允许。
                if parent.is_terminal() && !goal.is_terminal() {
                    return Err(ContractError::FailClosed(
                        "父目标已结束，子目标却仍然未结束（§6 第 9 步）",
                    ));
                }
            }
            ids.push(goal.goal_id.clone());
        }
        assert_unique(&ids, "goal_stack.goals")?;
        Ok(())
    }
}
