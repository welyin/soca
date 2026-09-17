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
    BlobRef, BudgetRef, Candidate, CandidateSet, CapabilityPolicyRef, CognitiveUnit, ContractError,
    DomainId, Envelope, EvidenceRef, ModelProfileRef, OutcomeVerified, Prediction, Scope,
    Sha256Hex, StrategyVersion, TaskContractVersion, UnitId, UnitKind, UnitSnapshot, UnitState,
    WallClock, Workspace, WorkspaceNote, SCHEMA_VERSION,
};

use crate::leaves::{observation_of, ActionPrecondition, FileVersion, PostconditionVerify, Precondition};

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
            ],
            workspace: Workspace::new(),
            ingested: 0,
        })
    }

    /// L2 黑板（只读）。
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
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
            self.workspace.post(
                observation.subject.clone(),
                WorkspaceNote::Finding {
                    statement: format!("{} 的版本是 {}", observation.subject, observation.value),
                    evidence_refs: vec![observation.evidence_ref],
                },
                &self.unit_id,
                at,
            )?;
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
            // §4.2：逐类**原样合并**。尤其是冲突与未决——它们必须原封不动地出现在父单元的
            // 输出里。在这里挑一个胜者、或者丢掉重复的未决问题，就是那句"不能只拼接子摘要
            // 或用多数意见覆盖矛盾"要禁的事。
            merged.candidates.extend(set.candidates);
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
}
