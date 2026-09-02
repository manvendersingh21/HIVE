# Graph Report - hive  (2026-09-01)

## Corpus Check
- Corpus is ~6,706 words - fits in a single context window. You may not need a graph.

## Summary
- 209 nodes · 412 edges · 19 communities (15 shown, 4 thin omitted)
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- Datetime, Display Module
- Atomicusize, Workerstatus Module
- Formatter, Agentresponse Module
- Config, Lines Module
- Path, Model Module
- Error, From Module
- Main, Check Module
- File, Root Module
- Cloudllmconfig, Key Module
- Planner, Option Module
- Databaseconfig, Path Module
- Threshold, Entities Module
- Extrarule, Notificationconfig Module
- Main, Check Module
- Finetuneconfig, Hiveconfig Module
- Mod, Datacollector Module
- Mod, Self Module
- Mod, Self Module

## God Nodes (most connected - your core abstractions)
1. `TaskAssignment` - 15 edges
2. `TaskStatus` - 12 edges
3. `HiveConfig` - 11 edges
4. `TaskCommand` - 10 edges
5. `WorkerInfo` - 9 edges
6. `WorkerPool` - 9 edges
7. `test_task_assignment_builder()` - 8 edges
8. `LlmRouter` - 8 edges
9. `WatchdogConfig` - 7 edges
10. `TaskPriority` - 7 edges

## Surprising Connections (you probably didn't know these)
- `receive_task()` --references--> `TaskAssignment`  [EXTRACTED]
  hive-worker/src/main.rs → hive-common/src/protocol.rs
- `receive_task()` --references--> `TaskStatus`  [EXTRACTED]
  hive-worker/src/main.rs → hive-common/src/protocol.rs
- `task_status()` --references--> `TaskStatus`  [EXTRACTED]
  hive-worker/src/main.rs → hive-common/src/protocol.rs
- `WorkerNode` --references--> `WorkerInfo`  [EXTRACTED]
  hive-core/src/workers/mod.rs → hive-common/src/protocol.rs
- `WorkerNode` --references--> `WorkerStatus`  [EXTRACTED]
  hive-core/src/workers/mod.rs → hive-common/src/protocol.rs

## Import Cycles
- None detected.

## Communities (19 total, 4 thin omitted)

### Community 0 - "Datetime, Display Module"
Cohesion: 0.11
Nodes (27): DateTime, Display, HashMap, AiContext, HumanDecision, Incident, IncidentReviewState, Default (+19 more)

### Community 1 - "Atomicusize, Workerstatus Module"
Cohesion: 0.08
Nodes (21): AtomicUsize, WorkerStatus, MasterAgent, Self, LlmRouter, Result, Self, String (+13 more)

### Community 2 - "Formatter, Agentresponse Module"
Cohesion: 0.18
Nodes (7): Formatter, AgentResponse, AiProvider, Complexity, Result, Option, Result

### Community 3 - "Config, Lines Module"
Cohesion: 0.15
Nodes (6): default_capture_lines(), default_max_consecutive_safe(), default_poll_interval(), default_reduced_poll(), default_web_base_url(), test_watchdog_config_defaults()

### Community 4 - "Path, Model Module"
Cohesion: 0.18
Nodes (11): default_db_path(), default_embed_model(), default_local_model(), default_master_listen_addr(), default_ollama_url(), default_provider(), default_skills_dir(), default_web_listen_addr() (+3 more)

### Community 5 - "Error, From Module"
Cohesion: 0.22
Nodes (5): Error, From, HiveError, Self, String

### Community 6 - "Main, Check Module"
Cohesion: 0.25
Nodes (7): main(), receive_task(), Path, Result, String, task_status(), Json

### Community 7 - "File, Root Module"
Cohesion: 0.57
Nodes (4): Path, Self, WorkersConfig, HiveResult

### Community 8 - "Cloudllmconfig, Key Module"
Cohesion: 0.33
Nodes (6): CloudLlmConfig, LlmConfig, LocalLlmConfig, Option, test_cloud_llm_api_key_from_env(), test_cloud_llm_direct_api_key()

### Community 9 - "Planner, Option Module"
Cohesion: 0.53
Nodes (5): Option, String, Vec, SubTask, TaskPlan

### Community 10 - "Databaseconfig, Path Module"
Cohesion: 0.50
Nodes (4): DatabaseConfig, dirs_compat(), test_default_db_path_expansion(), PathBuf

### Community 11 - "Threshold, Entities Module"
Cohesion: 0.40
Nodes (4): default_dedup_threshold(), default_max_entities(), KnowledgeGraphConfig, MemoryConfig

### Community 12 - "Extrarule, Notificationconfig Module"
Cohesion: 0.50
Nodes (5): ExtraRule, NotificationConfig, Default, Vec, WatchdogConfig

### Community 14 - "Finetuneconfig, Hiveconfig Module"
Cohesion: 0.50
Nodes (4): FinetuneConfig, HiveConfig, MasterConfig, SkillsConfig

## Knowledge Gaps
- **4 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `WorkerInfo` connect `Datetime, Display Module` to `Atomicusize, Workerstatus Module`, `Config, Lines Module`, `File, Root Module`?**
  _High betweenness centrality (0.369) - this node is a cross-community bridge._
- **Why does `WorkerNode` connect `Atomicusize, Workerstatus Module` to `Datetime, Display Module`?**
  _High betweenness centrality (0.084) - this node is a cross-community bridge._
- **Should `Datetime, Display Module` be split into smaller, more focused modules?**
  _Cohesion score 0.11394557823129252 - nodes in this community are weakly interconnected._
- **Should `Atomicusize, Workerstatus Module` be split into smaller, more focused modules?**
  _Cohesion score 0.08095238095238096 - nodes in this community are weakly interconnected._