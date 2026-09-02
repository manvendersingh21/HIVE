//! Hive Core — Master agent brain.
//!
//! This crate contains the central intelligence of the Hive system:
//! - Agent loop (ReAct pattern for task planning and execution)
//! - LLM routing (local Ollama + cloud providers)
//! - Worker pool management (SSH delegation, load balancing)
//! - Tool system (shell, file ops, git)
//! - Skill system (TOML-defined custom skills)
//! - Memory system (projects, knowledge graph, RAG)
//! - Safety watchdog (continuous monitoring, human-in-the-loop)
//! - Fine-tuning data collection

pub mod agent;
pub mod finetune;
pub mod llm;
pub mod memory;
pub mod skills;
pub mod tools;
pub mod watchdog;
pub mod workers;
