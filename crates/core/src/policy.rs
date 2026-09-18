//! 策略代理：决定要不要签发执行许可（§12.1、§12.2）。
//!
//! §12.2 把执行许可的判定写成了一串绑定，但**没有任何东西决定该不该发**——在本次改动之前，
//! 测试里全是手搓的许可。手搓的许可证明的是"许可类型本身可构造"，证明不了"系统会拒绝不该
//! 发生的动作"。这个模块补上那一环。
//!
//! 三条本模块负责的性质：
//!
//! 1. **授权不给子单元自动扩大**（§12.2）。动作等级不得超过本次目标声明的范围上限。
//!    超了就是 [`PermitDecision::Refused`]，**再多的人工批准也改变不了它**——要提升范围
//!    必须重新委托，而不是就地补一次同意。
//! 2. **需要批准的等级必须有覆盖这次具体动作的批准**（§12.1）。注意是"这次具体动作"：
//!    批准绑定了工具、资源范围与参数摘要，换一个目录或改一个字节就不再被覆盖。
//! 3. **失败关闭**（§12.2 末段）。全局暂停、策略版本不一致、许可构造失败，一律拒绝。
//!
//! 一个刻意的区分，写在类型上而不是注释里：**"需要批准"与"被拒绝"是两条不同的走向。**
//!
//! * [`PermitDecision::NeedsApproval`] 是"只差一次人工批准"——目标应当转入
//!   [`soca_contracts::GoalState::WaitingApproval`] 并停在那里等外部输入。
//! * [`PermitDecision::Refused`] 是"这条路上再多批准也没用"——等级超出范围、全局暂停、
//!   策略版本对不上。
//!
//! 把两者混成一种，界面上就会出现一个永远等不到结果的"等待审批"。这个区分也是
//! `Approval::covers` 那些错误全部归入 `NeedsApproval` 的原因：批准过期、次数用尽、
//! 绑定了别的动作——它们的正确下一步都是"请针对这一次动作再批一次"。

use std::collections::BTreeMap;

use soca_contracts::{
    ActionIntent, ActionLevel, Approval, ApprovalRequirement, BudgetRef, CapabilityPolicyRef,
    ExecutionPermit, GrantScope, PermissionScope, PermitId, PolicyVersion, RetryWhen, SubjectId,
    WallClock,
};

/// 一份许可的默认有效期（秒）。
///
/// 短期授权，不是常设许可（§12.2）。这个数字不需要很大：许可在同一个认知循环里签发、受理、
/// 交接，全程不超过几秒。设得太长只会让"受理时通过、交接时早已过期"这种本该被拦住的情况
/// 变得罕见而不再被测试。
pub const DEFAULT_PERMIT_TTL_SECONDS: i64 = 60;

/// 一份许可允许的使用次数。
///
/// 恒为 1。同一次动作的重复投递由动作账的去重负责（§7.3），不是靠许可的多次使用——
/// 让许可可以用两次，等于把"重试"合法化成一条不需要核对目标状态的路径。
const PERMIT_MAX_USES: u8 = 1;

/// 把一份授权范围写成一句人能读的话。
fn describe_scope(scope: &GrantScope) -> String {
    if scope.is_unbounded() {
        return "不按路径限定".to_string();
    }
    scope
        .prefixes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("、")
}

/// 许可签发的判定结果（§12.1、§12.2）。
#[derive(Clone, Debug, PartialEq)]
pub enum PermitDecision {
    /// 签发。
    ///
    /// 调用方**还必须原子消费一次批准**（[`soca_storage::Store::consume_approval`]），
    /// 并且只有在消费成功之后才能真正使用它。理由见 [`PolicyAgent::decide`]。
    Issued(Box<ExecutionPermit>),
    /// 需要一次覆盖这次具体动作的人工批准。
    NeedsApproval {
        /// 动作等级。
        level: ActionLevel,
        /// 该等级的放行要求（§12.1 的表）。
        requirement: ApprovalRequirement,
        /// 为什么现在还不能放行。给人读。
        reason: String,
    },
    /// 拒绝。**再多的人工批准也改变不了它**——范围越界、能力被撤回都属于这一类。
    Refused {
        /// 拒绝原因。
        reason: String,
    },
    /// 全局暂停中（§12.1）。
    ///
    /// 与 [`PermitDecision::Refused`] 分开，因为两者对"接下来该做什么"的含义完全不同：
    /// 拒绝是**此路不通**（要放行必须重新委托或重新授予），暂停是**此刻不通**——
    /// 而"此刻"会过去。调用方因此不该把被暂停挡住的动作取走：§12.1 的暂停是可逆的，
    /// 而"可逆"不只是说锁会打开，还包括**恢复之后那份工作还在**。
    Paused {
        /// 减速/暂停的理由（用户填的）。
        reason: String,
    },
}

impl PermitDecision {
    /// 被拒时**下一步该做什么**（§13.1 的"可重试条件"）。
    ///
    /// 这个取值是从变体本身推出来的，**不是另存一个字段**。因为三档的划分依据就是它：
    /// `Refused` 是此路不通，`Paused` 是等一等，`NeedsApproval` 是去批一次。多存一份
    /// 就多一个能对不上的地方——而它一旦对不上，界面会说"等一等就会好"，
    /// 而实际是永远好不了。
    ///
    /// 那为什么还要这个方法：因为**这个信息此前只活在理由的散文里**。调用方能拿到
    /// `Refused` 与 `Paused` 这两个名字，但"该继续挂着还是该收工"这个判断，每个调用点
    /// 都得自己 `match` 一遍——每加一档，都会在某个调用点上悄悄变成默认的那个。
    pub fn retry_when(&self) -> Option<RetryWhen> {
        match self {
            Self::Issued(_) => None,
            Self::NeedsApproval { .. } => Some(RetryWhen::WhenApproved),
            Self::Refused { .. } => Some(RetryWhen::Never),
            Self::Paused { .. } => Some(RetryWhen::WhenUnpaused),
        }
    }

    /// 是否签发了许可。
    pub fn issued(&self) -> Option<&ExecutionPermit> {
        match self {
            Self::Issued(permit) => Some(permit),
            _ => None,
        }
    }

    /// 是否在等人工批准。
    pub fn needs_approval(&self) -> bool {
        matches!(self, Self::NeedsApproval { .. })
    }
}

/// 策略代理。
///
/// 它不持有存储句柄、不碰 OS、不调用模型。它需要的一切都从参数进来，因此判定过程可以在
/// 没有数据库、没有文件系统的环境里被完整测试——而"边界规则有没有被正确实现"恰恰是
/// 最不该依赖环境才能验证的那一类问题。
#[derive(Debug, Clone)]
pub struct PolicyAgent {
    policy_version: PolicyVersion,
    /// 默认能力策略。用于"当前没有具体目标"时的观测——L0 总得有一个范围可依（§6 第 1 步）。
    default_capability: CapabilityPolicyRef,
    /// 当前生效的能力授权（§12.1："范围限定授权，**撤回立即生效**"），以及各自的范围。
    ///
    /// 用**映射**而不是一个布尔值，是因为授权的粒度是能力：撤回"读已选目录"不该连带撤掉
    /// "看授权窗口"。一个全局开关做不到这件事，而它最可能的后果是用户因为怕误伤而不敢撤回——
    /// 一个不敢用的撤回，等于没有撤回。
    ///
    /// 值是 [`GrantScope`] 而不是空，因为**光有"允许哪一类动作"还不是授权**。§12.1 给 A1 的
    /// 放行要求是"**范围限定**授权"，给 A2 的是"在**指定目录**生成文件"——两句话里的范围
    /// 都不是能力名能表达的。少了它，`cap:read-selected-folder` 就只是一个名字。
    granted: BTreeMap<CapabilityPolicyRef, GrantScope>,
    permit_ttl_seconds: i64,
    /// `Some(reason)` 表示全局暂停。§12.1 末段要求暂停时先撤销尚未消费的执行授权
    /// 并停止外发；对**尚未签发**的许可，表现就是这里一律拒绝。
    paused: Option<String>,
}

impl Default for PolicyAgent {
    fn default() -> Self {
        let default_capability = CapabilityPolicyRef::new("cap:read-selected-folder")
            .expect("固定能力策略");
        Self {
            policy_version: PolicyVersion::new("policy-v1").expect("固定策略版本"),
            default_capability: default_capability.clone(),
            // 默认**不按路径限定**。这是一个刻意的初始状态：`PolicyAgent::default()` 不知道
            // 这次任务守望的是哪个目录，而替它猜一个（比如"当前目录"）会猜出一次**比调用方
            // 以为的更宽**的授权。收窄是调用方的事，见 `Subject::new`。
            granted: [(default_capability, GrantScope::anywhere())]
                .into_iter()
                .collect(),
            permit_ttl_seconds: DEFAULT_PERMIT_TTL_SECONDS,
            paused: None,
        }
    }
}

impl PolicyAgent {
    /// 用一个策略版本与初始授权构造。
    ///
    /// 初始授权就是**全部**生效的授权：给一个集合再让调用方自己往里加，会留下一个
    /// "构造完但还没授权"的窗口，而那个窗口里的拒绝看起来像故障。
    pub fn new(
        policy_version: PolicyVersion,
        capability_policy_ref: CapabilityPolicyRef,
        scope: GrantScope,
    ) -> Self {
        Self {
            policy_version,
            default_capability: capability_policy_ref.clone(),
            granted: [(capability_policy_ref, scope)].into_iter().collect(),
            permit_ttl_seconds: DEFAULT_PERMIT_TTL_SECONDS,
            paused: None,
        }
    }

    /// 本次策略版本。它会被写进每一份许可，因此策略更新后旧许可自然失效。
    pub fn policy_version(&self) -> &PolicyVersion {
        &self.policy_version
    }

    /// 没有具体目标时用的能力策略。
    pub fn default_capability(&self) -> &CapabilityPolicyRef {
        &self.default_capability
    }

    /// 当前生效的全部授权，按标识排序。
    pub fn granted(&self) -> Vec<&CapabilityPolicyRef> {
        self.granted.keys().collect()
    }

    /// 某项能力当前是否生效（§12.1）。
    pub fn is_granted(&self, capability: &CapabilityPolicyRef) -> bool {
        self.granted.contains_key(capability)
    }

    /// 某项能力当前的范围。
    pub fn grant_scope(&self, capability: &CapabilityPolicyRef) -> Option<&GrantScope> {
        self.granted.get(capability)
    }

    /// 授予一项能力，并给出它的范围。返回 `false` 表示此前已经授过。
    ///
    /// 已经授过时**范围按新的来**：那不是"又批了一次"，而是"这次批的范围和上次不一样"——
    /// 收窄或放宽都是调用方明确做出的动作。把旧范围留着，会让一次明确的范围调整毫无效果，
    /// 而那种失败看不出来（授权看起来生效了，只是范围还是老的）。
    pub fn grant(&mut self, capability: CapabilityPolicyRef, scope: GrantScope) -> bool {
        self.granted.insert(capability, scope).is_none()
    }

    /// 撤回一项能力。返回 `false` 表示此前就不在授权内（幂等）。
    ///
    /// 撤回**只影响之后**。已经发生的观测不会因此消失——它们已经写在事件账上了，
    /// 而"抹掉历史"不是撤回，那是篡改。让过去产生的证据与记忆失效是另一件事，走
    /// `Subject::revoke_capability` 的失效传播。
    pub fn revoke(&mut self, capability: &CapabilityPolicyRef) -> bool {
        self.granted.remove(capability).is_some()
    }

    /// 撤回全部授权。§12.1 末段的"全局暂停"之外，这是更强的一档。
    pub fn revoke_all(&mut self) -> usize {
        let count = self.granted.len();
        self.granted.clear();
        count
    }

    /// 许可有效期（秒）。
    pub fn permit_ttl_seconds(&self) -> i64 {
        self.permit_ttl_seconds
    }

    /// 设定许可有效期。
    pub fn with_permit_ttl_seconds(mut self, seconds: i64) -> Self {
        self.permit_ttl_seconds = seconds;
        self
    }

    /// 全局暂停。§12.1 末段。
    pub fn pause(&mut self, reason: impl Into<String>) {
        self.paused = Some(reason.into());
    }

    /// 恢复。
    pub fn resume(&mut self) {
        self.paused = None;
    }

    /// 当前是否暂停。
    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }

    /// 暂停原因。
    pub fn pause_reason(&self) -> Option<&str> {
        self.paused.as_deref()
    }

    /// 判定是否签发许可（§12.1、§12.2）。
    ///
    /// 判定顺序是有意的，从"再多批准也没用"的那一类开始：
    ///
    /// 1. 全局暂停；
    /// 2. 动作等级超出本次目标的范围上限（§12.2：授权不给子单元自动扩大）；
    /// 3. 能力策略版本与本次会话声明的不一致；
    /// 4. 需要批准的等级必须有覆盖这次具体动作的批准；
    /// 5. 构造许可。
    ///
    /// **关于第 4 步与"消费批准"的分工。** 本方法只读批准、不消费它；消费由调用方用
    /// [`soca_storage::Store::consume_approval`] 原子完成，且必须在签发**之后**做。分开放是
    /// 因为它们要挡的是两件不同的事：本方法挡的是"这次动作不在批准的范围内"，而原子消费挡的
    /// 是"同一个批准被两次签发同时用掉"。把消费挪进来会带来一个更糟的失败模式——
    /// 判定失败也吃掉一次批准，于是用户批了三次、系统一次也没执行。
    ///
    /// 因此调用方的顺序是：判定 → 若签发则原子消费 → 消费失败就**丢弃许可**（它还没被
    /// 任何地方受理过，所以丢弃是安全的）。
    #[allow(clippy::too_many_arguments)]
    pub fn decide(
        &self,
        intent: &ActionIntent,
        scope: &PermissionScope,
        approval: Option<&Approval>,
        now: WallClock,
        permit_id: PermitId,
        subject_id: SubjectId,
        budget_ref: BudgetRef,
    ) -> PermitDecision {
        // 暂停单列一档，**不并进 `Refused`**。两者对"接下来该做什么"的含义完全不同：
        //
        // * `Refused` 是**此路不通**——范围越界、能力被撤回。要放行必须重新委托或重新授予，
        //   所以调用方把那个动作取走是对的。
        // * `Paused` 是**此刻不通**。§12.1 的暂停是可逆的，而"可逆"不只是说锁会打开：
        //   恢复之后那份还没做的工作得**还在**。并进 `Refused` 的话，暂停会顺手把待办丢掉，
        //   用户恢复之后发现该做的事没了，而账上只有一条"被拒绝"——一次静默的放弃。
        if let Some(reason) = &self.paused {
            return PermitDecision::Paused {
                reason: reason.clone(),
            };
        }

        if intent.risk > scope.max_action_level {
            return PermitDecision::Refused {
                reason: format!(
                    "动作等级 {} 超出本次目标声明的上限 {}；授权不给子单元自动扩大，\
                     要提升必须重新委托（§12.2）",
                    intent.risk.as_str(),
                    scope.max_action_level.as_str()
                ),
            };
        }

        // §12.1 的"范围限定授权，撤回立即生效"。撤回之后，这个能力名下不再签发任何许可——
        // 而这不是"运气不好"，它和等级超范围一样属于"再多批准也没用"的那一类：
        // 要恢复得先重新授予，不是就地补一次同意。
        let Some(grant) = self.grant_scope(&scope.capability_policy_ref) else {
            return PermitDecision::Refused {
                reason: format!(
                    "能力策略 {} 当前不在生效授权内（已撤回或从未授予）；\
                     范围限定授权撤回立即生效（§12.1）",
                    scope.capability_policy_ref
                ),
            };
        };

        // §12.1 那句"**范围限定**授权"的后半句。上一步回答"这一类动作允许吗"，
        // 这一步回答"允许在哪些对象上"。
        //
        // 归入 `Refused` 而不是 `NeedsApproval`，与等级超范围同一个理由：范围是授权本身的
        // 属性，再多的人工批准也改不了它。如果补一次"同意"就能换来写系统目录的权限，
        // 那么用户点的那个同意就不是他以为的那件事——而这类判定的全部价值，恰恰在于
        // 它挡住的是**用户以为自己没批准的事**。
        if !grant.covers(&intent.object_scope) {
            return PermitDecision::Refused {
                reason: format!(
                    "动作对象 {} 超出能力 {} 的授权范围（{}）；要触及别处必须重新授权，\
                     不是就地补一次同意（§12.1）",
                    intent.object_scope,
                    scope.capability_policy_ref,
                    describe_scope(grant)
                ),
            };
        }

        let mut approval_id = None;
        if intent.risk.requires_approval_id() {
            let requirement = intent.risk.approval_requirement();
            let Some(approval) = approval else {
                return PermitDecision::NeedsApproval {
                    level: intent.risk,
                    requirement,
                    reason: format!(
                        "{} 的放行要求是「{}」，当前没有可用于这次动作的批准",
                        intent.risk.as_str(),
                        requirement_name(requirement)
                    ),
                };
            };
            if let Err(error) = approval.covers(intent, now) {
                // 有批准但不覆盖这次动作，**不是**拒绝。两者的区别是能不能靠再一次人工批准
                // 解决：这种情况能。报成拒绝的话，界面会说"不可行"，而实际上只差一次批准。
                return PermitDecision::NeedsApproval {
                    level: intent.risk,
                    requirement,
                    reason: format!("{error}；需要针对这一次动作的批准"),
                };
            }
            approval_id = Some(approval.approval_id.clone());
        }

        match ExecutionPermit::issue_for(
            intent,
            permit_id,
            subject_id,
            now,
            self.permit_ttl_seconds,
            PERMIT_MAX_USES,
            budget_ref,
            self.policy_version.clone(),
            // 许可记录的是**实际授权这次动作**的那一项，而不是一个全局默认值。
            // 记默认值的话，事后审计看到的是"当时默认是什么"，而不是"当时是什么批的"。
            scope.capability_policy_ref.clone(),
            approval_id,
        ) {
            Ok(permit) => PermitDecision::Issued(Box::new(permit)),
            Err(error) => PermitDecision::Refused {
                reason: format!("许可构造失败，按失败关闭处理：{error}"),
            },
        }
    }
}

/// 放行要求的中文名，用于给用户看的理由。
fn requirement_name(requirement: ApprovalRequirement) -> &'static str {
    match requirement {
        ApprovalRequirement::TaskScopeAndBudget => "已有任务范围与预算即可",
        ApprovalRequirement::StandingScopeGrant => "范围限定授权",
        ApprovalRequirement::PreviewOrPerTaskApproval => "预览、目标版本核对，或每任务明确批准",
        ApprovalRequirement::PerActionHumanApproval => "每动作人工审批",
        ApprovalRequirement::ForbiddenInCognitiveLoop => "普通认知循环无此能力",
    }
}
