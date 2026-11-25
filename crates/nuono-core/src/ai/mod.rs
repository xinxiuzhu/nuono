#![doc = include_str!("example.md")]

pub mod llm_provider;
pub(crate) mod memory_embeddings;
#[path = "usage.rs"]
pub(crate) mod usage_tracking;

pub use llm_provider::{OpenAiCompatibleProvider, ProviderType, parse_provider_type};

pub use nuono_ai::*;
pub use nuono_app::ai::{ConversationManager, model_route_matches};

pub mod web;
pub(crate) mod web_tools;
