pub mod capabilities;
pub mod continuation;
pub mod loop_;
pub mod provider;
pub mod tool;
pub mod tool_result_filter;
pub mod types;

pub use capabilities::{ProviderCapabilities, SystemPromptCapability};
pub use continuation::{
    project_strict, project_to_clean_boundary, Continuation, ContinuationError,
};
pub use loop_::{
    AgentConfig, AgentEvent, AgentInput, AgentLoop, Outcome, SpawnArgs,
    DEFAULT_COMPACTION_THRESHOLD,
};
pub use provider::{MessageInput, Provider, ProviderError, ProviderEvent};
pub use tool::{PermissionLevel, RetryPolicy, SideEffects, Tool, ToolPolicy, ToolResult, ToolSpec};
pub use types::{
    AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolArgs, ToolArgsError, ToolCall,
    Usage,
};
