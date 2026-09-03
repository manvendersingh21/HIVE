# Graph Report - hive  (2026-09-02)

## Corpus Check
- Corpus is ~26,219 words - fits in a single context window. You may not need a graph.

## Summary
- 506 nodes · 937 edges · 29 communities (20 shown, 9 thin omitted)
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 15 edges (avg confidence: 0.86)
- Token cost: 135,916 input · 0 output

## Community Hubs (Navigation)
- Config Types
- Protocol & Task Types
- Watchdog Tier-2 Review
- Original Implementation Plan
- CLI Entry Point
- File Ops Tool
- Ollama Client
- Enum Display Formatting
- SSH Transport & Log Tailing
- Task Planner
- Gemini Client
- Git Tool
- Shell Tool
- OpenAI Client
- Roadmap Phases
- Claude Client
- Error Types
- Doc Files & Graphify Meta
- Web Server Entry Point
- Fine-tune Data Collector
- Project Identity & Design Rationale
- Cloud Spend Open Question
- Fine-tune Corpus Open Question
- Worker Details Open Question
- Web Exposure Security Question
- Security Design Choices
- Web Terminal Concept
- hive-web Crate Description

## God Nodes (most connected - your core abstractions)
1. `LlmRouter` - 22 edges
2. `WorkerPool` - 19 edges
3. `TaskAssignment` - 16 edges
4. `Watchdog` - 16 edges
5. `MasterAgent` - 14 edges
6. `WatchdogConfig` - 13 edges
7. `TaskStatus` - 12 edges
8. `Tool` - 12 edges
9. `ToolRegistry` - 12 edges
10. `SshWorker` - 12 edges

## Surprising Connections (you probably didn't know these)
- `MasterAgent` --semantically_similar_to--> `MasterAgent::handle_request`  [INFERRED] [semantically similar]
  README.md → docs/implementation-plan.md
- `watchdog/mod.rs Watchdog::scan_line / Watchdog::review` --semantically_similar_to--> `Safety Watchdog`  [INFERRED] [semantically similar]
  docs/STATUS.md → README.md
- `Safety Watchdog` --semantically_similar_to--> `Watchdog ractor Actor`  [INFERRED] [semantically similar]
  README.md → docs/implementation-plan.md
- `LlmRouter` --semantically_similar_to--> `LlmRouter`  [INFERRED] [semantically similar]
  README.md → docs/implementation-plan.md
- `SshWorker (workers/ssh.rs)` --semantically_similar_to--> `WorkerPool`  [INFERRED] [semantically similar]
  docs/STATUS.md → README.md

## Import Cycles
- 2-file cycle: `hive-core/src/llm/local.rs -> hive-core/src/llm/mod.rs -> hive-core/src/llm/local.rs`

## Hyperedges (group relationships)
- **Safety Watchdog Detection to Analysis to Notification Pipeline** — docs_implementation_plan_watchdog_actor, docs_implementation_plan_safety_analyzer, docs_implementation_plan_safety_rules, docs_implementation_plan_notifier, docs_status_watchdog_mod, docs_status_watchdog_rules [INFERRED 0.85]
- **Core Critical-Path Phases: Scaffold to Router to Delegation to Watchdog** — docs_roadmap_phase_1, docs_roadmap_phase_2, docs_roadmap_phase_3, docs_roadmap_phase_10 [EXTRACTED 1.00]
- **MemorySystem Composed Components** — docs_implementation_plan_memory_system, docs_implementation_plan_knowledge_graph, docs_implementation_plan_rag_index, docs_implementation_plan_project_registry, docs_implementation_plan_knowledge_extractor [EXTRACTED 1.00]

## Communities (29 total, 9 thin omitted)

### Community 0 - "Config Types"
Cohesion: 0.07
Nodes (44): CloudLlmConfig, DatabaseConfig, default_capture_lines(), default_db_path(), default_dedup_threshold(), default_embed_model(), default_local_model(), default_master_listen_addr() (+36 more)

### Community 1 - "Protocol & Task Types"
Cohesion: 0.09
Nodes (32): DateTime, AgentResponse, AiContext, AiProvider, Complexity, HumanDecision, Incident, IncidentReviewState (+24 more)

### Community 2 - "Watchdog Tier-2 Review"
Cohesion: 0.10
Nodes (30): AtomicUsize, SafetyAnalysis, SessionInfo, extract_analysis(), extract_analysis_parses_clean_json(), inconclusive(), parse_category(), parse_severity() (+22 more)

### Community 3 - "Original Implementation Plan"
Cohesion: 0.06
Nodes (43): AiProvider enum, Rationale: nomic-embed-text for embeddings, not Qwen2.5-14B, Fine-Tuning Pipeline (DataCollector), hive-worker HTTP daemon, KnowledgeExtractor, KnowledgeGraph, LlmRouter, MasterAgent::handle_request (+35 more)

### Community 4 - "CLI Entry Point"
Cohesion: 0.10
Nodes (27): build_agent(), Cli, Commands, FinetuneAction, main(), Path, PathBuf, Result (+19 more)

### Community 5 - "File Ops Tool"
Cohesion: 0.08
Nodes (21): Box, FileOpsArgs, FileOpsTool, Default, Result, RootSchema, Self, String (+13 more)

### Community 6 - "Ollama Client"
Cohesion: 0.12
Nodes (19): ChatRequest, ChatResponse, ChatResponseMessage, EmbedRequest, EmbedResponse, OllamaClient, Client, Result (+11 more)

### Community 7 - "Enum Display Formatting"
Cohesion: 0.14
Nodes (12): Display, Formatter, Result, SafetyCategory, Severity, WorkerStatus, default_rules(), does_not_flag_benign_output() (+4 more)

### Community 8 - "SSH Transport & Log Tailing"
Cohesion: 0.17
Nodes (12): BufReader, Child, ChildStdout, LogTail, Arc, Option, Result, Self (+4 more)

### Community 9 - "Task Planner"
Cohesion: 0.20
Nodes (12): extract_plan(), extract_plan_from_clean_json(), extract_plan_from_markdown_fenced_json(), Planner, Default, Option, Result, Self (+4 more)

### Community 10 - "Gemini Client"
Cohesion: 0.22
Nodes (13): Candidate, CandidateContent, CandidatePart, Content, GeminiClient, GenerateContentRequest, GenerateContentResponse, Part (+5 more)

### Community 11 - "Git Tool"
Cohesion: 0.17
Nodes (9): GitArgs, GitTool, Default, Option, Result, RootSchema, Self, String (+1 more)

### Community 12 - "Shell Tool"
Cohesion: 0.17
Nodes (9): Default, Option, Result, RootSchema, Self, String, Value, ShellArgs (+1 more)

### Community 13 - "OpenAI Client"
Cohesion: 0.19
Nodes (12): ChatCompletionsRequest, ChatCompletionsResponse, Choice, ChoiceMessage, Message, OpenAiClient, Client, Message (+4 more)

### Community 14 - "Roadmap Phases"
Cohesion: 0.19
Nodes (14): Gap: no persistent master daemon for continuous supervision, Rationale: Phase 3 redesigned to direct SSH+tmux, no worker daemon, Phase 1 - Project Scaffold & Core Types, Phase 10 - Safety Watchdog, Phase 2 - LLM Router & Agent Loop, Phase 3 - Worker Pool & SSH Delegation, Phase 4 - Worker Daemon, Phase 5 - Web Terminal (+6 more)

### Community 15 - "Claude Client"
Cohesion: 0.20
Nodes (11): ClaudeClient, ContentBlock, Message, MessagesRequest, MessagesResponse, Client, Message, Result (+3 more)

### Community 16 - "Error Types"
Cohesion: 0.22
Nodes (5): Error, From, HiveError, Self, String

### Community 17 - "Doc Files & Graphify Meta"
Cohesion: 0.47
Nodes (3): Implementation Plan, graphify-out/ Repo Knowledge Graph, Knowledge Graph + RAG Memory Index

### Community 20 - "Project Identity & Design Rationale"
Cohesion: 0.67
Nodes (3): Hive, Decision: Rust workspace, Decision: tmux as the execution surface

## Ambiguous Edges - Review These
- `Safety Watchdog` → `Design: Watchdog pauses (not kills) flagged sessions`  [AMBIGUOUS]
  README.md · relation: conceptually_related_to
- `Design: Watchdog pauses (not kills) flagged sessions` → `Original Design: watchdog kills flagged tasks immediately`  [AMBIGUOUS]
  docs/implementation-plan.md · relation: conceptually_related_to

## Knowledge Gaps
- **31 isolated node(s):** `Message`, `EmbedRequest`, `Message`, `Knowledge Graph + RAG Memory Index`, `hive-common crate` (+26 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **9 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What is the exact relationship between `Safety Watchdog` and `Design: Watchdog pauses (not kills) flagged sessions`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._
- **What is the exact relationship between `Design: Watchdog pauses (not kills) flagged sessions` and `Original Design: watchdog kills flagged tasks immediately`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._
- **Why does `LlmRouter` connect `Ollama Client` to `Watchdog Tier-2 Review`, `CLI Entry Point`, `Task Planner`, `Gemini Client`, `OpenAI Client`, `Claude Client`?**
  _High betweenness centrality (0.192) - this node is a cross-community bridge._
- **Why does `ToolRegistry` connect `File Ops Tool` to `CLI Entry Point`?**
  _High betweenness centrality (0.183) - this node is a cross-community bridge._
- **Why does `Tool` connect `File Ops Tool` to `Git Tool`, `Shell Tool`?**
  _High betweenness centrality (0.148) - this node is a cross-community bridge._
- **What connects `Message`, `EmbedRequest`, `Message` to the rest of the system?**
  _31 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Config Types` be split into smaller, more focused modules?**
  _Cohesion score 0.06810035842293907 - nodes in this community are weakly interconnected._