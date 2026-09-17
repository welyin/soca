## SoCA工程入口

- [研究基础与工程可行性评估](design/SoCA研究基础与工程可行性评估.md)：现有研究支持哪些工程合同，哪些AGI能力仍未获验证。
- [SoCA架构与实施方案](design/SoCA架构与实施方案.md)：最小认知单元、分形层数与数量、硬件弹性、冷热存储、LLM闭环、Windows多模态与权限接口及分阶段验收。
- [开发实施规格：伸缩与认知游戏](design/SoCA工程实施规格-伸缩与认知游戏.md)：各层单元数量自动规划、拓扑迁移事务、接口与数据表、迷宫/扫雷适配与评测，以及可直接拆分的开发工单。

这些文档是工程设计基线，附[拓扑规划参考实现](design/soca_reference/topology.py)与[游戏消息Schema](design/soca_reference/game-protocol.schema.json)，不表示SoCA产品、运行中的游戏或完整AGI已经实现。原[概念层方案](design/认知联合体架构-AGI工程方案.md)保留。