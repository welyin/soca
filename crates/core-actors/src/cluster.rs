//! 能力簇：L2 工作空间与子单元的装配（§4.1 L2、§4.2）。
//!
//! §4.2 只有两句要求，但两句都是本节全部复杂度的来源：
//!
//! > 复合单元对外暴露同一 `observe/propose/predict/handle_result/snapshot` 合同。
//!
//! > 父单元输出包含子结果、关系索引、冲突、未决条件和证据引用，**不能只拼接子摘要或用多数
//! > 意见覆盖矛盾**。
//!
//! 第一句决定了本类型实现的是与叶单元**完全相同**的 trait，而不是一套只做汇总的接口。
//! 第二句决定了三处具体写法：候选集合逐类原样合并、证据取子单元并集、L2 的拒绝作用于整个
//! 集合而不是挑出违规的那一条。

use soca_contracts::{
    ActionIntent, BlobRef, BudgetRef, Candidate, CandidateSet, CapabilityPolicyRef, CognitiveUnit,
    ContractError, DomainId, Envelope, EvidenceRef, Expectation, GoalId, ModelProfileRef,
    OutcomeVerified, Prediction, Scope, Sha256Hex, StrategyVersion, TaskContractVersion, UnitId,
    UnitKind, UnitSnapshot, UnitState, WallClock, Workspace, WorkspaceNote, SCHEMA_VERSION,
};

use crate::evidence::{EvidenceLedger, EvidenceRecord};
use crate::leaves::{
    observation_of, ActionPrecondition, FileVersion, PendingAction, PostconditionVerify,
    Precondition,
};

/// §4.3"桌面与文件"能力簇。
///
/// 一个技能簇拥有自己的一块 L2 黑板（§4.1："各组合边界有小空间，不无限复制"）。黑板只在本
/// 类型内部可变——[`DesktopAndFilesCluster::workspace`] 只借出只读引用，因此**簇之外不存在
/// 改写黑板证据的入口**。§4.1 那句"证据不能被执行层改写"靠的就是这一点，而不是靠一条注释。
pub struct DesktopAndFilesCluster {
    unit_id: UnitId,
    scope: Scope,
    strategy_version: StrategyVersion,
    leaves: Vec<Box<dyn CognitiveUnit>>,
    workspace: Workspace,
    /// 本簇见过的每条证据的完整记录（观测对象、值、来源链、观测者）。
    ///
    /// 与黑板分工不同：黑板记"哪些证据进过这个簇"，台账记"每条证据到底说了什么"。
    /// §4.3「来源核对」需要的是后者——没有来源链，就无法判断两条证据是不是同一次观测的两次抄写。
    ledger: EvidenceLedger,
    /// 本簇直接摄入的观测数（不含子单元各自的变化）。
    ingested: u64,
}

impl std::fmt::Debug for DesktopAndFilesCluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn CognitiveUnit` 不是 `Debug`，所以手写实现，只输出标识而不是内部状态。
        f.debug_struct("DesktopAndFilesCluster")
            .field("unit_id", &self.unit_id)
            .field("leaves", &self.leaf_ids())
            .field("workspace", &self.workspace)
            .field("ledger", &self.ledger.len())
            .field("ingested", &self.ingested)
            .finish()
    }
}

impl DesktopAndFilesCluster {
    /// 按 §4.3"桌面与文件"簇的槽位装配能力簇。
    ///
    /// 需要守望的对象与需要核验的前提由调用方给出：簇不替调用方猜它该守望什么。
    pub fn new(
        watched: impl Into<String>,
        preconditions: Vec<Precondition>,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            unit_id: UnitId::new("unit:cluster:desktop-and-files")?,
            scope: Scope {
                domain: DomainId::new("desktop-and-files")?,
                task_contract: TaskContractVersion::new("file-write-v1")?,
            },
            strategy_version: StrategyVersion::new("cluster-aggregate-v1")?,
            leaves: vec![
                Box::new(FileVersion::new(watched)?),
                Box::new(ActionPrecondition::new(preconditions)?),
                Box::new(PostconditionVerify::new()?),
                Box::new(PendingAction::new()?),
            ],
            workspace: Workspace::new(),
            ledger: EvidenceLedger::new(),
            ingested: 0,
        })
    }

    /// L2 黑板（只读）。
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// 撤回一批证据，让它们不再能作为下结论的材料（§7.2）。
    ///
    /// 语义方法而不是 `ledger_mut()`。理由与黑板只借出只读引用是同一条：**一个泛化的可变
    /// 入口，等于把"谁能在什么时候改动证据"这个问题交给调用方去守**。而这里是四个字——
    /// "撤回证据"——调用方说什么、做过什么，一看就明白。
    ///
    /// 撤回不删记录，也不动黑板：那条观测确实进过这个簇（§4.1 L2 的"存在性"），
    /// 变的是它还能不能被用来下结论（§7.2 的"**仍可访问**"）。两件事分开，是因为它们的
    /// 答案确实可以不同——而"存在"就当成"可用"，正是这条缺口此前的样子。
    pub fn retract_evidence(&mut self, refs: &[EvidenceRef], reason: &str) -> usize {
        self.ledger.retract(refs, reason)
    }

    /// 台账里已经撤回的证据条数。
    pub fn retracted_evidence(&self) -> usize {
        self.ledger.retracted_count()
    }

    /// 证据台账（只读）。
    ///
    /// 只借出只读引用，簇之外没有写入入口——与黑板同一条理由：§4.1 要求
    /// "证据不能被执行层改写"，而"缺失的写入方法"比"约定别写"更靠得住。
    pub fn ledger(&self) -> &EvidenceLedger {
        &self.ledger
    }

    /// 子单元标识。
    pub fn leaf_ids(&self) -> Vec<String> {
        self.leaves
            .iter()
            .map(|leaf| leaf.unit_id().to_string())
            .collect()
    }

    /// 子单元数量。
    pub fn leaf_count(&self) -> usize {
        self.leaves.len()
    }

    /// 直接摄入的观测数。
    pub fn ingested(&self) -> u64 {
        self.ingested
    }

    /// 投递一个待推进的动作（§6 第 3 步：动作前必须带可检查的预测）。
    ///
    /// 由簇转发而不是让调用方自己去 `leaves` 里找：簇的子单元列表是实现细节，
    /// 让外面按下标去摸第 4 个叶单元，等价于把列表顺序变成公开接口。
    pub fn queue_action(
        &mut self,
        goal_ref: GoalId,
        intent: ActionIntent,
        expectation: Expectation,
    ) -> Result<(), ContractError> {
        for leaf in &mut self.leaves {
            if let Some(pending) = leaf.as_any_mut().downcast_mut::<PendingAction>() {
                return pending.queue(goal_ref, intent, expectation);
            }
        }
        Err(ContractError::MissingRefs {
            field: "cluster.leaves.pending-action",
        })
    }

    /// 丢掉某个目标名下尚未推进的动作（§6 第 9 步）。
    pub fn release_goal(&mut self, goal_ref: &GoalId) -> usize {
        for leaf in &mut self.leaves {
            if let Some(pending) = leaf.as_any_mut().downcast_mut::<PendingAction>() {
                return pending.release_goal(goal_ref);
            }
        }
        0
    }

    /// 某个目标名下还有几个待推进的动作。
    pub fn pending_actions_for(&self, goal_ref: &GoalId) -> usize {
        self.leaves
            .iter()
            .filter_map(|leaf| leaf.as_any().downcast_ref::<PendingAction>())
            .map(|pending| pending.pending_for(goal_ref))
            .sum()
    }

    /// 取走一个已经有过结论的动作。
    ///
    /// 三种结论都算"有过结论"：执行完成、被拒绝、被判定需要审批之后由审批流程接管。
    /// 留着它会让 L3 每一轮都重新提议同一个动作——而"计划外动作不继续后台执行"（§6 第 9 步）
    /// 在慢动作下就是这个样子。
    pub fn release_action(&mut self, action_id: &soca_contracts::ActionId) -> bool {
        for leaf in &mut self.leaves {
            if let Some(pending) = leaf.as_any_mut().downcast_mut::<PendingAction>() {
                return pending.release(action_id);
            }
        }
        false
    }

    /// 尚未推进的动作数。
    pub fn pending_actions(&self) -> usize {
        self.leaves
            .iter()
            .filter_map(|leaf| leaf.as_any().downcast_ref::<PendingAction>())
            .map(PendingAction::pending)
            .sum()
    }
}

/// 候选在簇内的合并键（§13.1）。
///
/// 判据是"是不是同一件事"：同一个对象的观测请求、同一个工具配同一份参数、同一条命题配
/// 同一批证据、同一次动作。分隔符用 `\u{1f}` 而不是逗号，免得 `"ab" + "c"` 与 `"a" + "bc"`
/// 撞在一起。
fn merge_key(candidate: &Candidate) -> String {
    match candidate {
        Candidate::RequestObservation { subject_ref, .. } => {
            format!("observation\u{1f}{subject_ref}")
        }
        Candidate::Claim {
            statement,
            evidence_refs,
        } => {
            let mut refs: Vec<String> = evidence_refs.iter().map(ToString::to_string).collect();
            refs.sort_unstable();
            format!("claim\u{1f}{statement}\u{1f}{}", refs.join("\u{1f}"))
        }
        Candidate::RequestTool {
            tool_id,
            parameters,
        } => format!("tool\u{1f}{tool_id}\u{1f}{parameters}"),
        Candidate::RequestAction { intent } => format!("action\u{1f}{}", intent.action_id),
    }
}

/// 把一条重复候选并进已有那条。
///
/// 观测请求的**理由并起来**而不是丢掉。§13.1 说"低价值重复提案可合并"，但"为什么要它"
/// 是判断价值时唯一有内容的东西——把它丢了，合并之后就只剩一条光秃秃的请求，而两个子单元
/// 各自看到的缺口也不见了。
fn merge_into(existing: &mut Candidate, incoming: &Candidate) {
    if let (
        Candidate::RequestObservation { reason, .. },
        Candidate::RequestObservation {
            reason: extra, ..
        },
    ) = (&mut *existing, incoming)
        && !reason.contains(extra.as_str())
    {
        reason.push('；');
        reason.push_str(extra);
    }
}

impl CognitiveUnit for DesktopAndFilesCluster {
    fn unit_id(&self) -> &UnitId {
        &self.unit_id
    }

    fn kind(&self) -> UnitKind {
        UnitKind::Cluster
    }

    fn observe(&mut self, event: &Envelope, at: WallClock) -> Result<(), ContractError> {
        // §6 第 1–3 步：观测先成为 L2 上的一条发现，再交给子单元更新各自的局部信念。
        //
        // 顺序不能反。反过来的话，子单元会先拿证据下注，而黑板上还没有这条证据；随后
        // `propose` 的 L2 门会把自己的子单元整批挡下——一个自相矛盾的中间态。
        if let Some(observation) = observation_of(event) {
            let note = WorkspaceNote::Finding {
                statement: format!("{} 的版本是 {}", observation.subject, observation.value),
                evidence_refs: vec![observation.evidence_ref.clone()],
            };
            // 台账与黑板一起更新，顺序在黑板上先落定之后。反过来的话，一条被黑板拒绝的
            // 观测（超出主题数或字节上限）会留在台账里，于是核对时"手边有一份材料"而
            // "黑板上没有这条证据"——两个事实互相矛盾，而后续的候选校验会因此时好时坏。
            self.workspace
                .post(observation.subject.clone(), note, &self.unit_id, at)?;
            self.ledger
                .record(EvidenceRecord::from_observation(&observation));
            self.ingested = self.ingested.saturating_add(1);
        }

        for leaf in &mut self.leaves {
            leaf.observe(event, at)?;
        }
        Ok(())
    }

    fn propose(&self, at: WallClock) -> Result<CandidateSet, ContractError> {
        let mut merged = CandidateSet::empty();
        for leaf in &self.leaves {
            let set = leaf.propose(at)?;
            // §13.1："每个能力簇**先合并重复来源**，再向上提交。"
            //
            // 合并的判据是"是不是同一件事"，而不是"两条候选是不是逐字节相同"。后者在当前这些
            // 叶子上永远不会命中——两个子单元就算要的是同一个对象的观测，理由的措辞也不同——
            // 于是一段永远不会执行的去重代码，看起来和没有去重一模一样。
            //
            // 证据类的候选**不**归并到一条上。把不同来源的证据并到一条候选里，会让它看起来
            // 得到了更多支持，而"到底是谁找到的"就说不清了；§4.2 要求父单元的输出里带着那份
            // 归属，而现在的 `Candidate` 还没有承载它。所以只有命题与证据集合都一致时才合并。
            for candidate in set.candidates {
                let key = merge_key(&candidate);
                match merged
                    .candidates
                    .iter_mut()
                    .find(|existing| merge_key(existing) == key)
                {
                    None => merged.candidates.push(candidate),
                    Some(existing) => merge_into(existing, &candidate),
                }
            }
            // §4.2：冲突与未决**原样合并**。它们必须原封不动地出现在父单元的输出里——
            // 在这里挑一个胜者、或者丢掉重复的未决问题，就是那句"不能只拼接子摘要或用多数
            // 意见覆盖矛盾"要禁的事。注意它们连去重都不做：两个子单元对同一对象提出同一个
            // 未决问题，恰恰说明那个缺口是共通的。
            merged.conflicts.extend(set.conflicts);
            merged.unresolved.extend(set.unresolved);
        }

        // §6 第 4 步：L2 验证候选结构与证据存在性。
        //
        // 注意这里拒绝的是**整个集合**，不是挑出违规的那一条。一个引用了不存在证据的子单元
        // 会把簇的输出一起挡下，而不是让它的假证据混在别人的真证据里蒙混过关。若按条豁免，
        // §7.2 的"引用必须能解析为存在且仍可访问的证据"就等于没有——因为总有一条能过去。
        self.workspace.validate_candidates(&merged)?;
        Ok(merged)
    }

    fn predict(&self, candidate: &Candidate, at: WallClock) -> Result<Prediction, ContractError> {
        // 簇不自己编预测。"同一个合同"不等于"同一份实现"：预测必须由**提出该候选的那个
        // 子单元**给出。簇若自行编造，它就变成了第二个独立判断来源，而那个判断背后没有任何
        // 局部信念支撑——§8 的"状态、目标、证据在 Core 与存储中，不只在上下文窗口里"正
        // 是针对这种凭空的判断。
        let mut failure = ContractError::MissingRefs {
            field: "cluster.leaves",
        };
        for leaf in &self.leaves {
            match leaf.predict(candidate, at) {
                Ok(prediction) => return Ok(prediction),
                Err(error) => failure = error,
            }
        }
        Err(failure)
    }

    fn handle_result(
        &mut self,
        outcome: &OutcomeVerified,
        at: WallClock,
    ) -> Result<(), ContractError> {
        // §6 第 7–8 步：判定结果本身是 L2 上的一条笔记。先落笔记，再让子单元比较预测与观测；
        // 否则子单元会拿着一个尚未进入公共证据集的判定去更新自己的信念。
        self.workspace.post(
            outcome.action_id.to_string(),
            WorkspaceNote::Outcome {
                action_id: outcome.action_id.clone(),
                verdict: outcome.verdict,
                observation_refs: outcome.observation_refs.clone(),
            },
            &self.unit_id,
            at,
        )?;

        for leaf in &mut self.leaves {
            leaf.handle_result(outcome, at)?;
        }
        Ok(())
    }

    fn snapshot(&self) -> UnitSnapshot {
        // §4.2：父单元输出包含"子结果、关系索引、冲突、未决条件和证据引用"，不能只拼接
        // 子摘要。所以这里取的是子单元证据的**并集**，而不是簇自己攒的一份概括——概括会
        // 丢掉"哪条证据来自哪个子单元"这个信息，而那正是重放与追责时唯一有用的部分。
        let mut evidence: Vec<EvidenceRef> = Vec::new();
        let mut pending = Vec::new();
        let mut revision = self.ingested;

        for leaf in &self.leaves {
            let child = leaf.snapshot();
            for reference in child.evidence_refs {
                if !evidence.contains(&reference) {
                    evidence.push(reference);
                }
            }
            for action in child.pending_action_ids {
                if !pending.contains(&action) {
                    pending.push(action);
                }
            }
            revision = revision.saturating_add(child.belief_revision);
        }

        let digest = Sha256Hex::of_bytes(self.unit_id.as_str().as_bytes());
        UnitSnapshot {
            unit_id: self.unit_id.clone(),
            kind: UnitKind::Cluster,
            schema_version: SCHEMA_VERSION,
            scope: self.scope.clone(),
            goal_refs: Vec::new(),
            belief_revision: revision,
            belief_snapshot_ref: BlobRef::new(format!("blob:cluster-{digest}"))
                .expect("摘要定长，必然合法"),
            evidence_refs: evidence,
            relation_refs: Vec::new(),
            strategy_version: self.strategy_version.clone(),
            model_profile_ref: ModelProfileRef::new("profile:none-deterministic")
                .expect("固定模型画像"),
            capability_policy_ref: CapabilityPolicyRef::new("cap:read-selected-folder")
                .expect("固定能力策略"),
            budget_ref: BudgetRef::new("budget:task-cluster").expect("固定预算"),
            pending_action_ids: pending,
            last_applied_sequence: 0,
            state: UnitState::Ready,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
