//! Versioned typed workflow DAG/IR, legacy recipe adapter, validation, and a
//! deterministic headless executor. Browser/Git/event adapters are not
//! smuggled into this core: persistent triggers remain capability-gated and
//! all node effects execute through injected adapters.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

pub const WORKFLOW_SCHEMA_VERSION: u32 = 1;
pub const WORKFLOW_IR_VERSION: u32 = 1;
pub const WORKFLOW_RUN_HISTORY_VERSION: u32 = 1;
pub const LEGACY_RECIPE_VERSION: u32 = 1;

pub type WorkflowResult<T> = Result<T, WorkflowError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowError {
    InvalidDefinition(String),
    TypeMismatch(String),
    Cycle(String),
    MissingApproval(String),
    InvalidSecret(String),
    UnsupportedTrigger(String),
    BudgetExceeded(String),
    Execution(String),
    Cancelled,
    NeedsReconciliation(String),
    Replay(String),
    Json(String),
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDefinition(message) => write!(formatter, "invalid workflow: {message}"),
            Self::TypeMismatch(message) => write!(formatter, "workflow type mismatch: {message}"),
            Self::Cycle(message) => write!(formatter, "workflow cycle: {message}"),
            Self::MissingApproval(message) => {
                write!(formatter, "workflow approval missing: {message}")
            }
            Self::InvalidSecret(message) => write!(formatter, "workflow secret invalid: {message}"),
            Self::UnsupportedTrigger(message) => {
                write!(formatter, "workflow trigger unavailable: {message}")
            }
            Self::BudgetExceeded(message) => {
                write!(formatter, "workflow budget exceeded: {message}")
            }
            Self::Execution(message) => write!(formatter, "workflow execution failed: {message}"),
            Self::Cancelled => formatter.write_str("workflow cancelled"),
            Self::NeedsReconciliation(message) => {
                write!(formatter, "workflow needs reconciliation: {message}")
            }
            Self::Replay(message) => write!(formatter, "workflow replay rejected: {message}"),
            Self::Json(message) => write!(formatter, "workflow JSON error: {message}"),
        }
    }
}

impl std::error::Error for WorkflowError {}

impl From<serde_json::Error> for WorkflowError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn validate_id(label: &str, value: &str) -> WorkflowResult<()> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(WorkflowError::InvalidDefinition(format!(
            "{label} must be a bounded ASCII identifier"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowValueType {
    String,
    Integer,
    Decimal,
    Boolean,
    Json,
    Artifact,
    Unit,
    Array { item: Box<WorkflowValueType> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum WorkflowValue {
    String(String),
    Integer(i64),
    Decimal(f64),
    Boolean(bool),
    Json(Value),
    Artifact(ArtifactReference),
    Unit,
    Array(Vec<WorkflowValue>),
}

impl WorkflowValue {
    pub fn value_type(&self) -> WorkflowResult<WorkflowValueType> {
        match self {
            Self::String(_) => Ok(WorkflowValueType::String),
            Self::Integer(_) => Ok(WorkflowValueType::Integer),
            Self::Decimal(value) if value.is_finite() => Ok(WorkflowValueType::Decimal),
            Self::Decimal(_) => Err(WorkflowError::TypeMismatch(
                "non-finite decimal value".to_string(),
            )),
            Self::Boolean(_) => Ok(WorkflowValueType::Boolean),
            Self::Json(_) => Ok(WorkflowValueType::Json),
            Self::Artifact(reference) => {
                reference.validate()?;
                Ok(WorkflowValueType::Artifact)
            }
            Self::Unit => Ok(WorkflowValueType::Unit),
            Self::Array(values) => {
                let first = values.first().ok_or_else(|| {
                    WorkflowError::TypeMismatch(
                        "empty array literal needs an explicit typed workflow input".to_string(),
                    )
                })?;
                let item = first.value_type()?;
                if values
                    .iter()
                    .skip(1)
                    .any(|value| value.value_type().as_ref() != Ok(&item))
                {
                    return Err(WorkflowError::TypeMismatch(
                        "array contains mixed value types".to_string(),
                    ));
                }
                Ok(WorkflowValueType::Array {
                    item: Box::new(item),
                })
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactReference {
    pub artifact_id: String,
    pub sha256: String,
    pub media_type: String,
}

impl ArtifactReference {
    fn validate(&self) -> WorkflowResult<()> {
        validate_id("artifact_id", &self.artifact_id)?;
        if self.sha256.len() != 64
            || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.media_type.is_empty()
            || self.media_type.len() > 160
        {
            return Err(WorkflowError::TypeMismatch(
                "artifact reference is malformed".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum InputBinding {
    WorkflowInput { input_id: String },
    NodeOutput { node_id: String, port: String },
    Literal { value: WorkflowValue },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretDeclaration {
    pub secret_id: String,
    pub purpose: String,
    pub allowed_node_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretBinding {
    pub secret_id: String,
    pub vault_reference: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Pure,
    ReadOnly,
    LocalMutation,
    ExternalMutation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Transient,
    RateLimited,
    Timeout,
    Validation,
    Permission,
    Permanent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicy {
    pub maximum_attempts: u32,
    pub initial_backoff_ms: u64,
    pub maximum_backoff_ms: u64,
    pub retry_on: BTreeSet<FailureClass>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            maximum_attempts: 1,
            initial_backoff_ms: 0,
            maximum_backoff_ms: 0,
            retry_on: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IdempotencyPolicy {
    None,
    Keyed { key_template: String },
    VerifiedState { verifier_id: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    Safe,
    RequiresApproval,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceEstimate {
    pub model_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microunits: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodePermissionPolicy {
    pub permission_ids: BTreeSet<String>,
    /// Mutation nodes must bind their `approval` Boolean input to this node.
    pub approval_node_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConditionGuard {
    pub condition_node_id: String,
    pub expected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowNode {
    pub node_id: String,
    pub kind: WorkflowNodeKind,
    pub inputs: BTreeMap<String, InputBinding>,
    pub secret_ids: BTreeSet<String>,
    pub permission_policy: NodePermissionPolicy,
    pub retry: RetryPolicy,
    pub timeout_ms: u64,
    pub estimate: ResourceEstimate,
    pub idempotency: IdempotencyPolicy,
    pub replay: ReplayPolicy,
    pub guard: Option<ConditionGuard>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowNodeKind {
    PromptModel {
        model_selector: String,
    },
    Agent {
        agent_profile: String,
        effect: EffectClass,
    },
    Subagent {
        agent_profile: String,
        effect: EffectClass,
    },
    Tool {
        tool_id: String,
        effect: EffectClass,
    },
    Mcp {
        server_id: String,
        tool_name: String,
        effect: EffectClass,
    },
    Browser {
        action: String,
        effect: EffectClass,
    },
    Git {
        action: String,
        effect: EffectClass,
    },
    PullRequest {
        action: String,
        effect: EffectClass,
    },
    Shell {
        shell_profile: String,
    },
    Verify {
        verifier_id: String,
    },
    Transform {
        transform_id: String,
    },
    Condition,
    BoundedLoop {
        maximum_iterations: u32,
    },
    HumanApproval {
        approval_policy_id: String,
    },
    Artifact {
        media_type: String,
    },
    Output,
    LegacyRecipe {
        recipe: LegacyRecipeV1,
    },
}

impl WorkflowNodeKind {
    pub fn effect(&self) -> EffectClass {
        match self {
            Self::PromptModel { .. }
            | Self::Verify { .. }
            | Self::Transform { .. }
            | Self::Condition
            | Self::BoundedLoop { .. }
            | Self::HumanApproval { .. }
            | Self::Artifact { .. }
            | Self::Output
            | Self::LegacyRecipe { .. } => EffectClass::Pure,
            Self::Agent { effect, .. }
            | Self::Subagent { effect, .. }
            | Self::Tool { effect, .. }
            | Self::Mcp { effect, .. }
            | Self::Browser { effect, .. }
            | Self::Git { effect, .. }
            | Self::PullRequest { effect, .. } => *effect,
            Self::Shell { .. } => EffectClass::LocalMutation,
        }
    }

    fn signature(&self) -> WorkflowResult<NodeSignature> {
        let signature = match self {
            Self::PromptModel { model_selector } => {
                validate_id("model_selector", model_selector)?;
                NodeSignature::single(
                    "prompt",
                    WorkflowValueType::String,
                    WorkflowValueType::String,
                )
            }
            Self::Agent {
                agent_profile,
                effect,
            } => {
                validate_id("agent_profile", agent_profile)?;
                signature_with_optional_approval(
                    "prompt",
                    WorkflowValueType::String,
                    WorkflowValueType::String,
                    *effect,
                )
            }
            Self::Subagent {
                agent_profile,
                effect,
            } => {
                validate_id("agent_profile", agent_profile)?;
                signature_with_optional_approval(
                    "prompt",
                    WorkflowValueType::String,
                    WorkflowValueType::String,
                    *effect,
                )
            }
            Self::Tool { tool_id, effect } => {
                validate_id("tool_id", tool_id)?;
                signature_with_optional_approval(
                    "arguments",
                    WorkflowValueType::Json,
                    WorkflowValueType::Json,
                    *effect,
                )
            }
            Self::Mcp {
                server_id,
                tool_name,
                effect,
            } => {
                validate_id("MCP server_id", server_id)?;
                validate_id("MCP tool_name", tool_name)?;
                signature_with_optional_approval(
                    "arguments",
                    WorkflowValueType::Json,
                    WorkflowValueType::Json,
                    *effect,
                )
            }
            Self::Browser { action, effect } => {
                validate_id("browser action", action)?;
                signature_with_optional_approval(
                    "arguments",
                    WorkflowValueType::Json,
                    WorkflowValueType::Json,
                    *effect,
                )
            }
            Self::Git { action, effect } => {
                validate_id("Git action", action)?;
                signature_with_optional_approval(
                    "arguments",
                    WorkflowValueType::Json,
                    WorkflowValueType::Json,
                    *effect,
                )
            }
            Self::PullRequest { action, effect } => {
                validate_id("pull-request action", action)?;
                signature_with_optional_approval(
                    "arguments",
                    WorkflowValueType::Json,
                    WorkflowValueType::Json,
                    *effect,
                )
            }
            Self::Shell { shell_profile } => {
                validate_id("shell_profile", shell_profile)?;
                signature_with_optional_approval(
                    "command",
                    WorkflowValueType::String,
                    WorkflowValueType::Json,
                    EffectClass::LocalMutation,
                )
            }
            Self::Verify { verifier_id } => {
                validate_id("verifier_id", verifier_id)?;
                NodeSignature::single("input", WorkflowValueType::Json, WorkflowValueType::Json)
            }
            Self::Transform { transform_id } => {
                validate_id("transform_id", transform_id)?;
                NodeSignature::single("input", WorkflowValueType::Json, WorkflowValueType::Json)
            }
            Self::Condition => NodeSignature::single(
                "condition",
                WorkflowValueType::Boolean,
                WorkflowValueType::Boolean,
            ),
            Self::BoundedLoop { maximum_iterations } => {
                if !(1..=10_000).contains(maximum_iterations) {
                    return Err(WorkflowError::InvalidDefinition(
                        "bounded loop maximum_iterations must be 1..=10000".to_string(),
                    ));
                }
                NodeSignature::single("input", WorkflowValueType::Json, WorkflowValueType::Json)
            }
            Self::HumanApproval { approval_policy_id } => {
                validate_id("approval_policy_id", approval_policy_id)?;
                NodeSignature::single(
                    "summary",
                    WorkflowValueType::String,
                    WorkflowValueType::Boolean,
                )
            }
            Self::Artifact { media_type } => {
                if media_type.is_empty() || media_type.len() > 160 {
                    return Err(WorkflowError::InvalidDefinition(
                        "invalid artifact media type".to_string(),
                    ));
                }
                NodeSignature::single(
                    "content",
                    WorkflowValueType::String,
                    WorkflowValueType::Artifact,
                )
            }
            Self::Output => {
                NodeSignature::single("value", WorkflowValueType::Json, WorkflowValueType::Json)
            }
            Self::LegacyRecipe { recipe } => {
                recipe.validate()?;
                NodeSignature {
                    inputs: recipe
                        .params
                        .keys()
                        .map(|name| (name.clone(), WorkflowValueType::String))
                        .collect(),
                    outputs: BTreeMap::from([("out".to_string(), WorkflowValueType::String)]),
                }
            }
        };
        Ok(signature)
    }
}

fn signature_with_optional_approval(
    input_name: &str,
    input_type: WorkflowValueType,
    output_type: WorkflowValueType,
    effect: EffectClass,
) -> NodeSignature {
    let mut signature = NodeSignature::single(input_name, input_type, output_type);
    if matches!(
        effect,
        EffectClass::LocalMutation | EffectClass::ExternalMutation
    ) {
        signature
            .inputs
            .insert("approval".to_string(), WorkflowValueType::Boolean);
    }
    signature
}

#[derive(Debug, Clone)]
struct NodeSignature {
    inputs: BTreeMap<String, WorkflowValueType>,
    outputs: BTreeMap<String, WorkflowValueType>,
}

impl NodeSignature {
    fn single(
        input_name: &str,
        input_type: WorkflowValueType,
        output_type: WorkflowValueType,
    ) -> Self {
        Self {
            inputs: BTreeMap::from([(input_name.to_string(), input_type)]),
            outputs: BTreeMap::from([("out".to_string(), output_type)]),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowOutput {
    pub value_type: WorkflowValueType,
    pub binding: InputBinding,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowBudgets {
    pub maximum_node_executions: u64,
    pub maximum_model_calls: u64,
    pub maximum_input_tokens: u64,
    pub maximum_output_tokens: u64,
    pub maximum_cost_microunits: u64,
    pub maximum_wall_time_ms: u64,
}

impl Default for WorkflowBudgets {
    fn default() -> Self {
        Self {
            maximum_node_executions: 1_000,
            maximum_model_calls: 100,
            maximum_input_tokens: 1_000_000,
            maximum_output_tokens: 1_000_000,
            maximum_cost_microunits: 100_000_000,
            maximum_wall_time_ms: 24 * 60 * 60 * 1_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DaemonCapability {
    PersistentCron,
    FilesystemWatch,
    SignedWebhook,
    EventIngestion,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowTrigger {
    Manual,
    InAppCron {
        expression: String,
    },
    PersistentCron {
        expression: String,
    },
    Filesystem {
        canonical_root: String,
        pattern: String,
    },
    SignedWebhook {
        webhook_id: String,
        secret_reference: String,
        replay_window_ms: u64,
    },
    EventIngestion {
        topic: String,
        consumer_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowDefinition {
    pub schema_version: u32,
    pub workflow_id: String,
    pub workflow_version: u32,
    pub name: String,
    pub inputs: BTreeMap<String, WorkflowValueType>,
    pub secrets: BTreeMap<String, SecretDeclaration>,
    pub nodes: Vec<WorkflowNode>,
    pub outputs: BTreeMap<String, WorkflowOutput>,
    pub budgets: WorkflowBudgets,
    pub maximum_concurrency: usize,
    pub triggers: Vec<WorkflowTrigger>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyRecipeTargetV1 {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub ollama: Option<String>,
    pub local_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LegacyRecipeV1 {
    pub version: u32,
    pub name: String,
    pub target: LegacyRecipeTargetV1,
    pub permission_mode: String,
    pub system: Option<String>,
    pub prompt: String,
    pub params: BTreeMap<String, Option<String>>,
    pub maximum_iterations: Option<usize>,
    pub timeout_seconds: Option<u64>,
}

impl LegacyRecipeV1 {
    pub fn validate(&self) -> WorkflowResult<()> {
        if self.version != LEGACY_RECIPE_VERSION {
            return Err(WorkflowError::InvalidDefinition(
                "unsupported legacy recipe version".to_string(),
            ));
        }
        validate_id("legacy recipe name", &self.name)?;
        let target_count = [
            self.target.provider.is_some(),
            self.target.ollama.is_some(),
            self.target.local_url.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if target_count != 1
            || (self.target.provider.is_some() != self.target.model.is_some())
            || self.permission_mode == "bypass"
            || self.permission_mode.is_empty()
            || self.prompt.trim().is_empty()
            || self.prompt.len() > 2 * 1024 * 1024
            || self
                .maximum_iterations
                .is_some_and(|value| value == 0 || value > 10_000)
            || self
                .timeout_seconds
                .is_some_and(|value| value == 0 || value > 86_400)
        {
            return Err(WorkflowError::InvalidDefinition(
                "legacy recipe violates target, permission, prompt, or bound constraints"
                    .to_string(),
            ));
        }
        for name in self.params.keys() {
            validate_id("legacy recipe parameter", name)?;
        }
        Ok(())
    }
}

pub fn adapt_legacy_recipe(recipe: LegacyRecipeV1) -> WorkflowResult<WorkflowDefinition> {
    recipe.validate()?;
    let inputs = recipe
        .params
        .keys()
        .map(|name| (name.clone(), WorkflowValueType::String))
        .collect::<BTreeMap<_, _>>();
    let bindings = recipe
        .params
        .keys()
        .map(|name| {
            (
                name.clone(),
                InputBinding::WorkflowInput {
                    input_id: name.clone(),
                },
            )
        })
        .collect();
    let timeout_ms = recipe.timeout_seconds.unwrap_or(60).saturating_mul(1_000);
    Ok(WorkflowDefinition {
        schema_version: WORKFLOW_SCHEMA_VERSION,
        workflow_id: format!("legacy:{}", recipe.name),
        workflow_version: 1,
        name: recipe.name.clone(),
        inputs,
        secrets: BTreeMap::new(),
        nodes: vec![WorkflowNode {
            node_id: "legacy-recipe".to_string(),
            kind: WorkflowNodeKind::LegacyRecipe { recipe },
            inputs: bindings,
            secret_ids: BTreeSet::new(),
            permission_policy: NodePermissionPolicy {
                permission_ids: BTreeSet::new(),
                approval_node_id: None,
            },
            retry: RetryPolicy::default(),
            timeout_ms,
            estimate: ResourceEstimate {
                model_calls: 1,
                input_tokens: 100_000,
                output_tokens: 100_000,
                cost_microunits: 10_000_000,
            },
            idempotency: IdempotencyPolicy::None,
            replay: ReplayPolicy::Safe,
            guard: None,
        }],
        outputs: BTreeMap::from([(
            "text".to_string(),
            WorkflowOutput {
                value_type: WorkflowValueType::String,
                binding: InputBinding::NodeOutput {
                    node_id: "legacy-recipe".to_string(),
                    port: "out".to_string(),
                },
            },
        )]),
        budgets: WorkflowBudgets::default(),
        maximum_concurrency: 1,
        triggers: vec![WorkflowTrigger::Manual],
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowIrNode {
    pub node: WorkflowNode,
    pub dependencies: BTreeSet<String>,
    pub level: u32,
    pub input_types: BTreeMap<String, WorkflowValueType>,
    pub output_types: BTreeMap<String, WorkflowValueType>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowIr {
    pub ir_version: u32,
    pub workflow_id: String,
    pub workflow_version: u32,
    pub definition_sha256: String,
    pub inputs: BTreeMap<String, WorkflowValueType>,
    pub secrets: BTreeMap<String, SecretDeclaration>,
    pub nodes: Vec<WorkflowIrNode>,
    pub outputs: BTreeMap<String, WorkflowOutput>,
    pub budgets: WorkflowBudgets,
    pub maximum_concurrency: usize,
    pub triggers: Vec<WorkflowTrigger>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct WorkflowCapabilityCatalog {
    /// Prompt selectors use `backend:model`; selectors without `:` use the
    /// whole value as the backend capability token.
    pub model_backends: BTreeSet<String>,
    pub agents: BTreeMap<String, EffectClass>,
    pub subagents: BTreeMap<String, EffectClass>,
    pub tools: BTreeMap<String, EffectClass>,
    /// Key format is `server_id:tool_name`.
    pub mcp_tools: BTreeMap<String, EffectClass>,
    pub browser_actions: BTreeMap<String, EffectClass>,
    pub git_actions: BTreeMap<String, EffectClass>,
    pub pull_request_actions: BTreeMap<String, EffectClass>,
    pub shell_profiles: BTreeSet<String>,
    pub transforms: BTreeSet<String>,
    pub verifiers: BTreeSet<String>,
}

pub fn compile_workflow(
    definition: &WorkflowDefinition,
    daemon_capabilities: &BTreeSet<DaemonCapability>,
    capabilities: &WorkflowCapabilityCatalog,
) -> WorkflowResult<WorkflowIr> {
    validate_workflow_header(definition)?;
    validate_triggers(&definition.triggers, daemon_capabilities)?;
    let mut nodes = BTreeMap::<String, (&WorkflowNode, NodeSignature)>::new();
    for node in &definition.nodes {
        validate_id("node_id", &node.node_id)?;
        if nodes.contains_key(&node.node_id) {
            return Err(WorkflowError::InvalidDefinition(format!(
                "duplicate node id: {}",
                node.node_id
            )));
        }
        let signature = node.kind.signature()?;
        validate_declared_capability(node, capabilities)?;
        validate_node_static(node, &signature, definition)?;
        nodes.insert(node.node_id.clone(), (node, signature));
    }
    let mut dependencies = BTreeMap::<String, BTreeSet<String>>::new();
    for (node_id, (node, signature)) in &nodes {
        let mut node_dependencies = BTreeSet::new();
        if node.inputs.len() != signature.inputs.len()
            || node
                .inputs
                .keys()
                .any(|port| !signature.inputs.contains_key(port))
        {
            return Err(WorkflowError::TypeMismatch(format!(
                "node {node_id} input ports differ from its typed signature"
            )));
        }
        for (port, expected) in &signature.inputs {
            let binding = node.inputs.get(port).expect("key set checked");
            let actual = binding_type(binding, definition, &nodes)?;
            if &actual != expected {
                return Err(WorkflowError::TypeMismatch(format!(
                    "node {node_id}.{port} expects {expected:?}, found {actual:?}"
                )));
            }
            if let InputBinding::NodeOutput {
                node_id: dependency,
                ..
            } = binding
            {
                node_dependencies.insert(dependency.clone());
            }
        }
        if let Some(guard) = &node.guard {
            let (_, guard_signature) = nodes.get(&guard.condition_node_id).ok_or_else(|| {
                WorkflowError::InvalidDefinition(format!(
                    "node {node_id} guard references missing node {}",
                    guard.condition_node_id
                ))
            })?;
            if guard_signature.outputs.get("out") != Some(&WorkflowValueType::Boolean) {
                return Err(WorkflowError::TypeMismatch(
                    "condition guard must reference a Boolean output".to_string(),
                ));
            }
            node_dependencies.insert(guard.condition_node_id.clone());
        }
        if let Some(approval_node_id) = &node.permission_policy.approval_node_id {
            let (approval_node, approval_signature) =
                nodes.get(approval_node_id).ok_or_else(|| {
                    WorkflowError::MissingApproval(format!(
                        "node {node_id} references missing approval node {approval_node_id}"
                    ))
                })?;
            if !matches!(approval_node.kind, WorkflowNodeKind::HumanApproval { .. })
                || approval_signature.outputs.get("out") != Some(&WorkflowValueType::Boolean)
                || !matches!(
                    node.inputs.get("approval"),
                    Some(InputBinding::NodeOutput { node_id, port })
                        if node_id == approval_node_id && port == "out"
                )
            {
                return Err(WorkflowError::MissingApproval(format!(
                    "node {node_id} approval must bind directly to HumanApproval {approval_node_id}.out"
                )));
            }
            node_dependencies.insert(approval_node_id.clone());
        }
        dependencies.insert(node_id.clone(), node_dependencies);
    }
    let (order, levels) = topological_order(&dependencies)?;
    for (output_id, output) in &definition.outputs {
        validate_id("workflow output id", output_id)?;
        let actual = binding_type(&output.binding, definition, &nodes)?;
        if actual != output.value_type {
            return Err(WorkflowError::TypeMismatch(format!(
                "workflow output {output_id} declares {:?}, found {actual:?}",
                output.value_type
            )));
        }
    }
    let mut canonical = definition.clone();
    canonical
        .nodes
        .sort_by(|left, right| left.node_id.cmp(&right.node_id));
    let definition_sha256 = sha256(&serde_json::to_vec(&canonical)?);
    let ir_nodes = order
        .into_iter()
        .map(|node_id| {
            let (node, signature) = nodes.get(&node_id).expect("topological key exists");
            WorkflowIrNode {
                node: (*node).clone(),
                dependencies: dependencies.remove(&node_id).unwrap_or_default(),
                level: levels[&node_id],
                input_types: signature.inputs.clone(),
                output_types: signature.outputs.clone(),
            }
        })
        .collect();
    Ok(WorkflowIr {
        ir_version: WORKFLOW_IR_VERSION,
        workflow_id: definition.workflow_id.clone(),
        workflow_version: definition.workflow_version,
        definition_sha256,
        inputs: definition.inputs.clone(),
        secrets: definition.secrets.clone(),
        nodes: ir_nodes,
        outputs: definition.outputs.clone(),
        budgets: definition.budgets.clone(),
        maximum_concurrency: definition.maximum_concurrency,
        triggers: definition.triggers.clone(),
    })
}

fn validate_declared_capability(
    node: &WorkflowNode,
    capabilities: &WorkflowCapabilityCatalog,
) -> WorkflowResult<()> {
    let expected = match &node.kind {
        WorkflowNodeKind::PromptModel { model_selector } => {
            let backend = model_selector
                .split_once(':')
                .map_or(model_selector.as_str(), |(backend, _)| backend);
            if !capabilities.model_backends.contains(backend) {
                return Err(WorkflowError::InvalidDefinition(format!(
                    "node {} model backend is unavailable: {backend}",
                    node.node_id
                )));
            }
            None
        }
        WorkflowNodeKind::Agent {
            agent_profile,
            effect,
        } => Some((capabilities.agents.get(agent_profile), effect, "agent")),
        WorkflowNodeKind::Subagent {
            agent_profile,
            effect,
        } => Some((
            capabilities.subagents.get(agent_profile),
            effect,
            "subagent",
        )),
        WorkflowNodeKind::Tool { tool_id, effect } => {
            Some((capabilities.tools.get(tool_id), effect, "tool"))
        }
        WorkflowNodeKind::Mcp {
            server_id,
            tool_name,
            effect,
        } => {
            let key = format!("{server_id}:{tool_name}");
            if capabilities.mcp_tools.get(&key) != Some(effect) {
                return Err(WorkflowError::InvalidDefinition(format!(
                    "node {} MCP effect/capability is unknown or mismatched",
                    node.node_id
                )));
            }
            None
        }
        WorkflowNodeKind::Browser { action, effect } => Some((
            capabilities.browser_actions.get(action),
            effect,
            "browser action",
        )),
        WorkflowNodeKind::Git { action, effect } => {
            Some((capabilities.git_actions.get(action), effect, "Git action"))
        }
        WorkflowNodeKind::PullRequest { action, effect } => Some((
            capabilities.pull_request_actions.get(action),
            effect,
            "pull-request action",
        )),
        WorkflowNodeKind::Shell { shell_profile } => {
            if !capabilities.shell_profiles.contains(shell_profile) {
                return Err(WorkflowError::InvalidDefinition(format!(
                    "node {} shell profile is unavailable: {shell_profile}",
                    node.node_id
                )));
            }
            None
        }
        WorkflowNodeKind::Transform { transform_id } => {
            if !capabilities.transforms.contains(transform_id) {
                return Err(WorkflowError::InvalidDefinition(format!(
                    "node {} transform is unavailable: {transform_id}",
                    node.node_id
                )));
            }
            None
        }
        WorkflowNodeKind::Verify { verifier_id } => {
            if !capabilities.verifiers.contains(verifier_id) {
                return Err(WorkflowError::InvalidDefinition(format!(
                    "node {} verifier is unavailable: {verifier_id}",
                    node.node_id
                )));
            }
            None
        }
        _ => None,
    };
    if let Some((catalog_effect, declared_effect, label)) = expected {
        if catalog_effect != Some(declared_effect) {
            return Err(WorkflowError::InvalidDefinition(format!(
                "node {} {label} effect/capability is unknown or mismatched",
                node.node_id
            )));
        }
    }
    Ok(())
}

fn validate_workflow_header(definition: &WorkflowDefinition) -> WorkflowResult<()> {
    if definition.schema_version != WORKFLOW_SCHEMA_VERSION
        || definition.workflow_version == 0
        || definition.name.trim().is_empty()
        || definition.name.len() > 256
        || definition.nodes.is_empty()
        || definition.nodes.len() > 10_000
        || definition.outputs.is_empty()
        || !(1..=64).contains(&definition.maximum_concurrency)
        || definition.triggers.is_empty()
    {
        return Err(WorkflowError::InvalidDefinition(
            "workflow header, counts, outputs, concurrency, or triggers are invalid".to_string(),
        ));
    }
    validate_id("workflow_id", &definition.workflow_id)?;
    for input_id in definition.inputs.keys() {
        validate_id("workflow input", input_id)?;
    }
    for (secret_id, secret) in &definition.secrets {
        validate_id("secret_id", secret_id)?;
        if secret.secret_id != *secret_id
            || secret.purpose.trim().is_empty()
            || secret.allowed_node_ids.is_empty()
        {
            return Err(WorkflowError::InvalidSecret(format!(
                "invalid secret declaration {secret_id}"
            )));
        }
    }
    if definition.budgets.maximum_node_executions == 0
        || definition.budgets.maximum_wall_time_ms == 0
    {
        return Err(WorkflowError::InvalidDefinition(
            "workflow execution/wall budgets must be positive".to_string(),
        ));
    }
    Ok(())
}

fn validate_node_static(
    node: &WorkflowNode,
    signature: &NodeSignature,
    definition: &WorkflowDefinition,
) -> WorkflowResult<()> {
    if node.timeout_ms == 0 || node.timeout_ms > definition.budgets.maximum_wall_time_ms {
        return Err(WorkflowError::InvalidDefinition(format!(
            "node {} timeout is zero or over workflow wall budget",
            node.node_id
        )));
    }
    if node.retry.maximum_attempts == 0
        || node.retry.maximum_attempts > 100
        || node.retry.initial_backoff_ms > node.retry.maximum_backoff_ms
        || node.retry.maximum_backoff_ms > 60 * 60 * 1_000
        || (node.retry.maximum_attempts > 1 && node.retry.retry_on.is_empty())
    {
        return Err(WorkflowError::InvalidDefinition(format!(
            "node {} retry policy is invalid",
            node.node_id
        )));
    }
    let effect = node.kind.effect();
    if matches!(
        effect,
        EffectClass::LocalMutation | EffectClass::ExternalMutation
    ) {
        if node.permission_policy.permission_ids.is_empty()
            || node.permission_policy.approval_node_id.is_none()
            || !signature.inputs.contains_key("approval")
        {
            return Err(WorkflowError::MissingApproval(format!(
                "mutation node {} needs permissions and HumanApproval binding",
                node.node_id
            )));
        }
        if node.retry.maximum_attempts > 1 && matches!(node.idempotency, IdempotencyPolicy::None) {
            return Err(WorkflowError::InvalidDefinition(format!(
                "mutation node {} cannot retry without idempotency/verified-state policy",
                node.node_id
            )));
        }
    } else if node.permission_policy.approval_node_id.is_some()
        && !signature.inputs.contains_key("approval")
    {
        return Err(WorkflowError::InvalidDefinition(format!(
            "pure/read node {} declares an unsupported approval input",
            node.node_id
        )));
    }
    if effect == EffectClass::ExternalMutation
        && node.retry.retry_on.contains(&FailureClass::Timeout)
    {
        return Err(WorkflowError::InvalidDefinition(format!(
            "external mutation node {} cannot blindly retry timeout/ambiguous effects",
            node.node_id
        )));
    }
    if matches!(node.replay, ReplayPolicy::Safe)
        && matches!(
            effect,
            EffectClass::LocalMutation | EffectClass::ExternalMutation
        )
    {
        return Err(WorkflowError::InvalidDefinition(format!(
            "mutation node {} cannot declare replay-safe",
            node.node_id
        )));
    }
    for secret_id in &node.secret_ids {
        let secret = definition.secrets.get(secret_id).ok_or_else(|| {
            WorkflowError::InvalidSecret(format!(
                "node {} references undeclared secret {secret_id}",
                node.node_id
            ))
        })?;
        if !secret.allowed_node_ids.contains(&node.node_id) {
            return Err(WorkflowError::InvalidSecret(format!(
                "secret {secret_id} is not scoped to node {}",
                node.node_id
            )));
        }
    }
    for permission in &node.permission_policy.permission_ids {
        validate_id("permission id", permission)?;
    }
    match &node.idempotency {
        IdempotencyPolicy::Keyed { key_template } if key_template.trim().is_empty() => {
            return Err(WorkflowError::InvalidDefinition(
                "idempotency key template cannot be empty".to_string(),
            ));
        }
        IdempotencyPolicy::VerifiedState { verifier_id } => {
            validate_id("state verifier id", verifier_id)?;
        }
        _ => {}
    }
    Ok(())
}

fn binding_type(
    binding: &InputBinding,
    definition: &WorkflowDefinition,
    nodes: &BTreeMap<String, (&WorkflowNode, NodeSignature)>,
) -> WorkflowResult<WorkflowValueType> {
    match binding {
        InputBinding::WorkflowInput { input_id } => {
            definition.inputs.get(input_id).cloned().ok_or_else(|| {
                WorkflowError::TypeMismatch(format!("unknown workflow input {input_id}"))
            })
        }
        InputBinding::NodeOutput { node_id, port } => nodes
            .get(node_id)
            .and_then(|(_, signature)| signature.outputs.get(port))
            .cloned()
            .ok_or_else(|| {
                WorkflowError::TypeMismatch(format!("unknown node output {node_id}.{port}"))
            }),
        InputBinding::Literal { value } => value.value_type(),
    }
}

fn topological_order(
    dependencies: &BTreeMap<String, BTreeSet<String>>,
) -> WorkflowResult<(Vec<String>, BTreeMap<String, u32>)> {
    for (node, deps) in dependencies {
        if deps.contains(node)
            || deps
                .iter()
                .any(|dependency| !dependencies.contains_key(dependency))
        {
            return Err(WorkflowError::Cycle(format!(
                "node {node} has a self or missing dependency"
            )));
        }
    }
    let mut indegree = dependencies
        .iter()
        .map(|(node, deps)| (node.clone(), deps.len()))
        .collect::<BTreeMap<_, _>>();
    let mut children = BTreeMap::<String, BTreeSet<String>>::new();
    for (node, deps) in dependencies {
        for dependency in deps {
            children
                .entry(dependency.clone())
                .or_default()
                .insert(node.clone());
        }
    }
    let mut ready = indegree
        .iter()
        .filter_map(|(node, count)| (*count == 0).then_some(node.clone()))
        .collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(dependencies.len());
    let mut levels = BTreeMap::new();
    while let Some(node) = ready.pop_first() {
        let level = dependencies[&node]
            .iter()
            .map(|dependency| levels[dependency] + 1)
            .max()
            .unwrap_or(0);
        levels.insert(node.clone(), level);
        order.push(node.clone());
        for child in children.get(&node).into_iter().flatten() {
            let count = indegree.get_mut(child).expect("child exists");
            *count -= 1;
            if *count == 0 {
                ready.insert(child.clone());
            }
        }
    }
    if order.len() != dependencies.len() {
        let remaining = indegree
            .into_iter()
            .filter_map(|(node, count)| (count > 0).then_some(node))
            .collect::<Vec<_>>();
        return Err(WorkflowError::Cycle(format!(
            "cycle includes: {}",
            remaining.join(", ")
        )));
    }
    Ok((order, levels))
}

pub fn validate_triggers(
    triggers: &[WorkflowTrigger],
    daemon_capabilities: &BTreeSet<DaemonCapability>,
) -> WorkflowResult<()> {
    for trigger in triggers {
        let required = match trigger {
            WorkflowTrigger::Manual => None,
            WorkflowTrigger::InAppCron { expression } => {
                validate_cron(expression)?;
                None
            }
            WorkflowTrigger::PersistentCron { expression } => {
                validate_cron(expression)?;
                Some(DaemonCapability::PersistentCron)
            }
            WorkflowTrigger::Filesystem {
                canonical_root,
                pattern,
            } => {
                if !canonical_root.starts_with('/')
                    || canonical_root.contains("/../")
                    || pattern.is_empty()
                    || pattern.len() > 512
                {
                    return Err(WorkflowError::UnsupportedTrigger(
                        "filesystem trigger root/pattern is invalid".to_string(),
                    ));
                }
                Some(DaemonCapability::FilesystemWatch)
            }
            WorkflowTrigger::SignedWebhook {
                webhook_id,
                secret_reference,
                replay_window_ms,
            } => {
                validate_id("webhook_id", webhook_id)?;
                validate_id("webhook secret reference", secret_reference)?;
                if !(1_000..=15 * 60_000).contains(replay_window_ms) {
                    return Err(WorkflowError::UnsupportedTrigger(
                        "signed webhook replay window is invalid".to_string(),
                    ));
                }
                Some(DaemonCapability::SignedWebhook)
            }
            WorkflowTrigger::EventIngestion { topic, consumer_id } => {
                validate_id("event topic", topic)?;
                validate_id("event consumer_id", consumer_id)?;
                Some(DaemonCapability::EventIngestion)
            }
        };
        if required.is_some_and(|capability| !daemon_capabilities.contains(&capability)) {
            return Err(WorkflowError::UnsupportedTrigger(format!(
                "resident daemon capability {required:?} is unavailable"
            )));
        }
    }
    Ok(())
}

fn validate_cron(expression: &str) -> WorkflowResult<()> {
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5
        || expression.len() > 256
        || expression.bytes().any(|byte| {
            !(byte.is_ascii_digit() || matches!(byte, b' ' | b'*' | b',' | b'-' | b'/'))
        })
    {
        return Err(WorkflowError::UnsupportedTrigger(
            "cron expression must be a bounded five-field numeric expression".to_string(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Headless execution, history, inspection, replay, and reconciliation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ResourceUsage {
    pub model_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microunits: u64,
    pub loop_iterations: u64,
}

impl ResourceUsage {
    fn add_assign(&mut self, other: &Self) {
        self.model_calls = self.model_calls.saturating_add(other.model_calls);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cost_microunits = self.cost_microunits.saturating_add(other.cost_microunits);
        self.loop_iterations = self.loop_iterations.saturating_add(other.loop_iterations);
    }

    fn within_estimate(&self, estimate: &ResourceEstimate) -> bool {
        self.model_calls <= estimate.model_calls
            && self.input_tokens <= estimate.input_tokens
            && self.output_tokens <= estimate.output_tokens
            && self.cost_microunits <= estimate.cost_microunits
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeExecutionRequest {
    pub run_id: String,
    pub workflow_id: String,
    pub node: WorkflowNode,
    pub inputs: BTreeMap<String, WorkflowValue>,
    /// Opaque vault references only; secret values never enter the IR/history.
    pub secrets: BTreeMap<String, SecretBinding>,
    pub attempt: u32,
    pub deadline_unix_ms: u64,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeAdapterResult {
    Succeeded {
        outputs: BTreeMap<String, WorkflowValue>,
        usage: ResourceUsage,
    },
    Failed {
        class: FailureClass,
        message: String,
        retryable: bool,
        usage: ResourceUsage,
    },
    AmbiguousExternalEffect {
        receipt: String,
        pending_outputs: BTreeMap<String, WorkflowValue>,
        usage: ResourceUsage,
    },
}

pub trait WorkflowNodeExecutor: Send + Sync {
    fn execute(
        &self,
        request: NodeExecutionRequest,
        cancel: &CancellationToken,
    ) -> Result<NodeAdapterResult, String>;

    /// Releases resources scoped to a completed, failed, or cancelled run.
    /// Implementations that own subprocesses should override this hook. The
    /// headless executor invokes it through a drop guard, including on early
    /// error returns and panics during unwinding.
    fn finish_run(&self, _run_id: &str) {}
}

pub trait WorkflowClock: Send + Sync {
    fn now_unix_ms(&self) -> u64;
    fn sleep_ms(&self, duration_ms: u64, cancel: &CancellationToken) -> Result<(), String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowRunRequest {
    pub run_id: String,
    pub inputs: BTreeMap<String, WorkflowValue>,
    pub secret_bindings: BTreeMap<String, SecretBinding>,
    pub trigger: WorkflowTrigger,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    NeedsReconciliation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NodeRunStatus {
    Pending,
    Running,
    Succeeded,
    Failed {
        class: FailureClass,
        message: String,
    },
    Skipped {
        reason: String,
    },
    NeedsReconciliation {
        receipt: String,
    },
    Reused {
        source_run_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeRunRecord {
    pub node_id: String,
    pub status: NodeRunStatus,
    pub inputs: BTreeMap<String, WorkflowValue>,
    pub secret_references: BTreeMap<String, SecretBinding>,
    pub outputs: BTreeMap<String, WorkflowValue>,
    pub pending_outputs: BTreeMap<String, WorkflowValue>,
    pub attempts: u32,
    pub started_unix_ms: Option<u64>,
    pub finished_unix_ms: Option<u64>,
    pub usage: ResourceUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorkflowRunEventKind {
    RunStarted,
    NodeFinished {
        node_id: String,
        status: NodeRunStatus,
    },
    NodeReused {
        node_id: String,
        source_run_id: String,
    },
    RunFinished {
        status: WorkflowRunStatus,
    },
    Reconciled {
        node_id: String,
        decision: ReconciliationDecision,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowRunEvent {
    pub sequence: u64,
    pub unix_ms: u64,
    pub kind: WorkflowRunEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowRunHistory {
    pub schema_version: u32,
    pub run_id: String,
    pub workflow_id: String,
    pub definition_sha256: String,
    pub status: WorkflowRunStatus,
    pub started_unix_ms: u64,
    pub finished_unix_ms: Option<u64>,
    pub trigger: WorkflowTrigger,
    pub input_snapshot: BTreeMap<String, WorkflowValue>,
    pub secret_reference_snapshot: BTreeMap<String, SecretBinding>,
    pub nodes: BTreeMap<String, NodeRunRecord>,
    pub outputs: BTreeMap<String, WorkflowValue>,
    pub usage: ResourceUsage,
    pub events: Vec<WorkflowRunEvent>,
}

impl WorkflowRunHistory {
    fn push_event(&mut self, unix_ms: u64, kind: WorkflowRunEventKind) {
        self.events.push(WorkflowRunEvent {
            sequence: self.events.len() as u64 + 1,
            unix_ms,
            kind,
        });
    }

    pub fn inspect_node(&self, node_id: &str) -> WorkflowResult<&NodeRunRecord> {
        self.nodes.get(node_id).ok_or_else(|| {
            WorkflowError::Execution(format!("run has no node record for {node_id}"))
        })
    }
}

#[derive(Debug, Clone)]
struct BudgetTracker {
    budgets: WorkflowBudgets,
    reserved_executions: u64,
    reserved_model_calls: u64,
    reserved_input_tokens: u64,
    reserved_output_tokens: u64,
    reserved_cost: u64,
}

impl BudgetTracker {
    fn new(budgets: WorkflowBudgets) -> Self {
        Self {
            budgets,
            reserved_executions: 0,
            reserved_model_calls: 0,
            reserved_input_tokens: 0,
            reserved_output_tokens: 0,
            reserved_cost: 0,
        }
    }

    fn reserve(&mut self, node: &WorkflowNode) -> WorkflowResult<()> {
        let attempts = u64::from(node.retry.maximum_attempts);
        let execution = attempts;
        let model_calls = node.estimate.model_calls.checked_mul(attempts);
        let input_tokens = node.estimate.input_tokens.checked_mul(attempts);
        let output_tokens = node.estimate.output_tokens.checked_mul(attempts);
        let cost = node.estimate.cost_microunits.checked_mul(attempts);
        let (Some(model_calls), Some(input_tokens), Some(output_tokens), Some(cost)) =
            (model_calls, input_tokens, output_tokens, cost)
        else {
            return Err(WorkflowError::BudgetExceeded(
                "node retry estimate overflow".to_string(),
            ));
        };
        let next_executions = self.reserved_executions.checked_add(execution);
        let next_model = self.reserved_model_calls.checked_add(model_calls);
        let next_input = self.reserved_input_tokens.checked_add(input_tokens);
        let next_output = self.reserved_output_tokens.checked_add(output_tokens);
        let next_cost = self.reserved_cost.checked_add(cost);
        let (
            Some(next_executions),
            Some(next_model),
            Some(next_input),
            Some(next_output),
            Some(next_cost),
        ) = (
            next_executions,
            next_model,
            next_input,
            next_output,
            next_cost,
        )
        else {
            return Err(WorkflowError::BudgetExceeded(
                "workflow reservation overflow".to_string(),
            ));
        };
        if next_executions > self.budgets.maximum_node_executions
            || next_model > self.budgets.maximum_model_calls
            || next_input > self.budgets.maximum_input_tokens
            || next_output > self.budgets.maximum_output_tokens
            || next_cost > self.budgets.maximum_cost_microunits
        {
            return Err(WorkflowError::BudgetExceeded(format!(
                "node {} worst-case retry reservation exceeds workflow budgets",
                node.node_id
            )));
        }
        self.reserved_executions = next_executions;
        self.reserved_model_calls = next_model;
        self.reserved_input_tokens = next_input;
        self.reserved_output_tokens = next_output;
        self.reserved_cost = next_cost;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct PreparedNode {
    ir_node: WorkflowIrNode,
    inputs: BTreeMap<String, WorkflowValue>,
    secrets: BTreeMap<String, SecretBinding>,
}

#[derive(Debug, Clone)]
struct NodeCompletion {
    node_id: String,
    status: NodeRunStatus,
    inputs: BTreeMap<String, WorkflowValue>,
    secrets: BTreeMap<String, SecretBinding>,
    outputs: BTreeMap<String, WorkflowValue>,
    pending_outputs: BTreeMap<String, WorkflowValue>,
    attempts: u32,
    started_unix_ms: u64,
    finished_unix_ms: u64,
    usage: ResourceUsage,
}

pub struct HeadlessWorkflowExecutor<'a> {
    node_executor: &'a dyn WorkflowNodeExecutor,
    clock: &'a dyn WorkflowClock,
}

struct WorkflowRunFinishGuard<'a> {
    executor: &'a dyn WorkflowNodeExecutor,
    run_id: String,
}

impl Drop for WorkflowRunFinishGuard<'_> {
    fn drop(&mut self) {
        self.executor.finish_run(&self.run_id);
    }
}

impl<'a> HeadlessWorkflowExecutor<'a> {
    pub fn new(node_executor: &'a dyn WorkflowNodeExecutor, clock: &'a dyn WorkflowClock) -> Self {
        Self {
            node_executor,
            clock,
        }
    }

    pub fn run(
        &self,
        ir: &WorkflowIr,
        request: WorkflowRunRequest,
        cancel: &CancellationToken,
    ) -> WorkflowResult<WorkflowRunHistory> {
        self.run_internal(ir, request, cancel, None)
    }

    pub fn replay(
        &self,
        ir: &WorkflowIr,
        request: WorkflowRunRequest,
        source: &WorkflowRunHistory,
        plan: &ReplayPlan,
        cancel: &CancellationToken,
    ) -> WorkflowResult<WorkflowRunHistory> {
        if plan.workflow_id != ir.workflow_id
            || plan.definition_sha256 != ir.definition_sha256
            || plan.source_run_id != source.run_id
        {
            return Err(WorkflowError::Replay(
                "replay plan is not bound to this IR/source run".to_string(),
            ));
        }
        self.run_internal(ir, request, cancel, Some((source, plan)))
    }

    fn run_internal(
        &self,
        ir: &WorkflowIr,
        request: WorkflowRunRequest,
        cancel: &CancellationToken,
        replay: Option<(&WorkflowRunHistory, &ReplayPlan)>,
    ) -> WorkflowResult<WorkflowRunHistory> {
        validate_run_request(ir, &request)?;
        if cancel.is_cancelled() {
            return Err(WorkflowError::Cancelled);
        }
        let _finish_guard = WorkflowRunFinishGuard {
            executor: self.node_executor,
            run_id: request.run_id.clone(),
        };
        let started = self.clock.now_unix_ms();
        let mut history = WorkflowRunHistory {
            schema_version: WORKFLOW_RUN_HISTORY_VERSION,
            run_id: request.run_id.clone(),
            workflow_id: ir.workflow_id.clone(),
            definition_sha256: ir.definition_sha256.clone(),
            status: WorkflowRunStatus::Running,
            started_unix_ms: started,
            finished_unix_ms: None,
            trigger: request.trigger.clone(),
            input_snapshot: request.inputs.clone(),
            secret_reference_snapshot: request.secret_bindings.clone(),
            nodes: BTreeMap::new(),
            outputs: BTreeMap::new(),
            usage: ResourceUsage::default(),
            events: Vec::new(),
        };
        history.push_event(started, WorkflowRunEventKind::RunStarted);
        let mut values = BTreeMap::<(String, String), WorkflowValue>::new();
        let mut reused = BTreeSet::new();
        if let Some((source, plan)) = replay {
            for node_id in &plan.reused_node_ids {
                let record = source.inspect_node(node_id)?;
                if !matches!(
                    record.status,
                    NodeRunStatus::Succeeded | NodeRunStatus::Reused { .. }
                ) {
                    return Err(WorkflowError::Replay(format!(
                        "source node {node_id} has no successful output to reuse"
                    )));
                }
                for (port, value) in &record.outputs {
                    values.insert((node_id.clone(), port.clone()), value.clone());
                }
                let mut reused_record = record.clone();
                reused_record.status = NodeRunStatus::Reused {
                    source_run_id: source.run_id.clone(),
                };
                history.nodes.insert(node_id.clone(), reused_record);
                history.push_event(
                    self.clock.now_unix_ms(),
                    WorkflowRunEventKind::NodeReused {
                        node_id: node_id.clone(),
                        source_run_id: source.run_id.clone(),
                    },
                );
                reused.insert(node_id.clone());
            }
        }
        let mut budget = BudgetTracker::new(ir.budgets.clone());
        let max_level = ir.nodes.iter().map(|node| node.level).max().unwrap_or(0);
        for level in 0..=max_level {
            if cancel.is_cancelled() {
                finish_history(
                    &mut history,
                    WorkflowRunStatus::Cancelled,
                    self.clock.now_unix_ms(),
                );
                return Ok(history);
            }
            if self.clock.now_unix_ms().saturating_sub(started) > ir.budgets.maximum_wall_time_ms {
                finish_history(
                    &mut history,
                    WorkflowRunStatus::Failed,
                    self.clock.now_unix_ms(),
                );
                return Err(WorkflowError::BudgetExceeded(
                    "workflow wall-time budget elapsed".to_string(),
                ));
            }
            let mut prepared = Vec::new();
            for ir_node in ir.nodes.iter().filter(|node| node.level == level) {
                if reused.contains(&ir_node.node.node_id) {
                    continue;
                }
                if let Some(guard) = &ir_node.node.guard {
                    let guard_value = values
                        .get(&(guard.condition_node_id.clone(), "out".to_string()))
                        .ok_or_else(|| {
                            WorkflowError::Execution(format!(
                                "guard value for {} is unavailable",
                                ir_node.node.node_id
                            ))
                        })?;
                    if guard_value != &WorkflowValue::Boolean(guard.expected) {
                        let now = self.clock.now_unix_ms();
                        let status = NodeRunStatus::Skipped {
                            reason: "condition guard did not match".to_string(),
                        };
                        history.nodes.insert(
                            ir_node.node.node_id.clone(),
                            NodeRunRecord {
                                node_id: ir_node.node.node_id.clone(),
                                status: status.clone(),
                                inputs: BTreeMap::new(),
                                secret_references: BTreeMap::new(),
                                outputs: BTreeMap::new(),
                                pending_outputs: BTreeMap::new(),
                                attempts: 0,
                                started_unix_ms: Some(now),
                                finished_unix_ms: Some(now),
                                usage: ResourceUsage::default(),
                            },
                        );
                        history.push_event(
                            now,
                            WorkflowRunEventKind::NodeFinished {
                                node_id: ir_node.node.node_id.clone(),
                                status,
                            },
                        );
                        continue;
                    }
                }
                let inputs = resolve_node_inputs(ir_node, &request.inputs, &values)?;
                if matches!(
                    ir_node.node.kind.effect(),
                    EffectClass::LocalMutation | EffectClass::ExternalMutation
                ) && inputs.get("approval") != Some(&WorkflowValue::Boolean(true))
                {
                    return Err(WorkflowError::MissingApproval(format!(
                        "node {} did not receive an affirmative approval",
                        ir_node.node.node_id
                    )));
                }
                let secrets = ir_node
                    .node
                    .secret_ids
                    .iter()
                    .map(|secret_id| {
                        request
                            .secret_bindings
                            .get(secret_id)
                            .cloned()
                            .map(|binding| (secret_id.clone(), binding))
                            .ok_or_else(|| {
                                WorkflowError::InvalidSecret(format!(
                                    "run has no vault reference for {secret_id}"
                                ))
                            })
                    })
                    .collect::<WorkflowResult<BTreeMap<_, _>>>()?;
                budget.reserve(&ir_node.node)?;
                prepared.push(PreparedNode {
                    ir_node: ir_node.clone(),
                    inputs,
                    secrets,
                });
            }
            for batch in prepared.chunks(ir.maximum_concurrency) {
                let completions = Mutex::new(Vec::<WorkflowResult<NodeCompletion>>::new());
                std::thread::scope(|scope| {
                    for prepared in batch.iter().cloned() {
                        let completions = &completions;
                        let run_id = request.run_id.clone();
                        let workflow_id = ir.workflow_id.clone();
                        scope.spawn(move || {
                            let result = run_prepared_node(
                                self.node_executor,
                                self.clock,
                                &run_id,
                                &workflow_id,
                                prepared,
                                cancel,
                            );
                            completions
                                .lock()
                                .expect("completion mutex poisoned")
                                .push(result);
                        });
                    }
                });
                let mut completions = completions.into_inner().map_err(|_| {
                    WorkflowError::Execution("completion mutex poisoned".to_string())
                })?;
                let mut completed = Vec::with_capacity(completions.len());
                for completion in completions.drain(..) {
                    completed.push(completion?);
                }
                completed.sort_by(|left, right| left.node_id.cmp(&right.node_id));
                let mut terminal_failure = None;
                for completion in completed {
                    for (port, value) in &completion.outputs {
                        values.insert((completion.node_id.clone(), port.clone()), value.clone());
                    }
                    history.usage.add_assign(&completion.usage);
                    let record = NodeRunRecord {
                        node_id: completion.node_id.clone(),
                        status: completion.status.clone(),
                        inputs: completion.inputs,
                        secret_references: completion.secrets,
                        outputs: completion.outputs,
                        pending_outputs: completion.pending_outputs,
                        attempts: completion.attempts,
                        started_unix_ms: Some(completion.started_unix_ms),
                        finished_unix_ms: Some(completion.finished_unix_ms),
                        usage: completion.usage,
                    };
                    history.nodes.insert(completion.node_id.clone(), record);
                    history.push_event(
                        completion.finished_unix_ms,
                        WorkflowRunEventKind::NodeFinished {
                            node_id: completion.node_id.clone(),
                            status: completion.status.clone(),
                        },
                    );
                    match completion.status {
                        NodeRunStatus::Failed { .. } => {
                            terminal_failure.get_or_insert(WorkflowRunStatus::Failed);
                        }
                        NodeRunStatus::NeedsReconciliation { .. } => {
                            terminal_failure = Some(WorkflowRunStatus::NeedsReconciliation);
                        }
                        _ => {}
                    }
                }
                if let Some(status) = terminal_failure {
                    finish_history(&mut history, status, self.clock.now_unix_ms());
                    return Ok(history);
                }
            }
        }
        for (output_id, output) in &ir.outputs {
            let value = resolve_binding_value(&output.binding, &request.inputs, &values)?;
            if value.value_type()? != output.value_type {
                return Err(WorkflowError::TypeMismatch(format!(
                    "runtime workflow output {output_id} has wrong type"
                )));
            }
            history.outputs.insert(output_id.clone(), value);
        }
        finish_history(
            &mut history,
            WorkflowRunStatus::Succeeded,
            self.clock.now_unix_ms(),
        );
        Ok(history)
    }
}

fn finish_history(history: &mut WorkflowRunHistory, status: WorkflowRunStatus, unix_ms: u64) {
    history.status = status;
    history.finished_unix_ms = Some(unix_ms);
    history.push_event(unix_ms, WorkflowRunEventKind::RunFinished { status });
}

fn validate_run_request(ir: &WorkflowIr, request: &WorkflowRunRequest) -> WorkflowResult<()> {
    validate_id("run_id", &request.run_id)?;
    if request.inputs.len() != ir.inputs.len()
        || request.inputs.keys().any(|id| !ir.inputs.contains_key(id))
    {
        return Err(WorkflowError::TypeMismatch(
            "run inputs differ from workflow input declaration".to_string(),
        ));
    }
    for (input_id, expected) in &ir.inputs {
        let value = request.inputs.get(input_id).expect("key set checked");
        if &value.value_type()? != expected {
            return Err(WorkflowError::TypeMismatch(format!(
                "run input {input_id} has wrong type"
            )));
        }
    }
    if request.secret_bindings.len() != ir.secrets.len()
        || request
            .secret_bindings
            .keys()
            .any(|secret| !ir.secrets.contains_key(secret))
    {
        return Err(WorkflowError::InvalidSecret(
            "run secret references differ from workflow declarations".to_string(),
        ));
    }
    for (secret_id, binding) in &request.secret_bindings {
        if binding.secret_id != *secret_id {
            return Err(WorkflowError::InvalidSecret(
                "secret binding identity mismatch".to_string(),
            ));
        }
        validate_id("secret vault reference", &binding.vault_reference)?;
    }
    if !ir.triggers.contains(&request.trigger) {
        return Err(WorkflowError::UnsupportedTrigger(
            "run trigger is not declared by workflow".to_string(),
        ));
    }
    if serde_json::to_vec(&request.inputs)?.len() > 16 * 1024 * 1024 {
        return Err(WorkflowError::TypeMismatch(
            "run input snapshot exceeds 16 MiB".to_string(),
        ));
    }
    Ok(())
}

fn resolve_node_inputs(
    ir_node: &WorkflowIrNode,
    workflow_inputs: &BTreeMap<String, WorkflowValue>,
    values: &BTreeMap<(String, String), WorkflowValue>,
) -> WorkflowResult<BTreeMap<String, WorkflowValue>> {
    ir_node
        .node
        .inputs
        .iter()
        .map(|(port, binding)| {
            let value = resolve_binding_value(binding, workflow_inputs, values)?;
            if value.value_type()? != ir_node.input_types[port] {
                return Err(WorkflowError::TypeMismatch(format!(
                    "runtime input {}.{port} has wrong type",
                    ir_node.node.node_id
                )));
            }
            Ok((port.clone(), value))
        })
        .collect()
}

fn resolve_binding_value(
    binding: &InputBinding,
    workflow_inputs: &BTreeMap<String, WorkflowValue>,
    values: &BTreeMap<(String, String), WorkflowValue>,
) -> WorkflowResult<WorkflowValue> {
    match binding {
        InputBinding::WorkflowInput { input_id } => workflow_inputs
            .get(input_id)
            .cloned()
            .ok_or_else(|| WorkflowError::Execution(format!("missing workflow input {input_id}"))),
        InputBinding::NodeOutput { node_id, port } => values
            .get(&(node_id.clone(), port.clone()))
            .cloned()
            .ok_or_else(|| WorkflowError::Execution(format!("missing output {node_id}.{port}"))),
        InputBinding::Literal { value } => Ok(value.clone()),
    }
}

fn run_prepared_node(
    executor: &dyn WorkflowNodeExecutor,
    clock: &dyn WorkflowClock,
    run_id: &str,
    workflow_id: &str,
    prepared: PreparedNode,
    cancel: &CancellationToken,
) -> WorkflowResult<NodeCompletion> {
    let started = clock.now_unix_ms();
    let node = &prepared.ir_node.node;
    let mut usage = ResourceUsage::default();
    let mut backoff = node.retry.initial_backoff_ms;
    for attempt in 1..=node.retry.maximum_attempts {
        if cancel.is_cancelled() {
            return Err(WorkflowError::Cancelled);
        }
        let attempt_started = clock.now_unix_ms();
        let deadline = attempt_started.saturating_add(node.timeout_ms);
        let idempotency_key = match &node.idempotency {
            IdempotencyPolicy::None => None,
            IdempotencyPolicy::Keyed { key_template } => Some(sha256(
                format!("{run_id}:{}:{key_template}", node.node_id).as_bytes(),
            )),
            IdempotencyPolicy::VerifiedState { verifier_id } => Some(sha256(
                format!("{run_id}:{}:verify:{verifier_id}", node.node_id).as_bytes(),
            )),
        };
        let adapter_result = executor
            .execute(
                NodeExecutionRequest {
                    run_id: run_id.to_string(),
                    workflow_id: workflow_id.to_string(),
                    node: node.clone(),
                    inputs: prepared.inputs.clone(),
                    secrets: prepared.secrets.clone(),
                    attempt,
                    deadline_unix_ms: deadline,
                    idempotency_key,
                },
                cancel,
            )
            .map_err(WorkflowError::Execution)?;
        let timed_out = clock.now_unix_ms() > deadline;
        let result = if timed_out {
            if node.kind.effect() == EffectClass::ExternalMutation {
                NodeAdapterResult::AmbiguousExternalEffect {
                    receipt: "timeout-without-verified-external-state".to_string(),
                    pending_outputs: BTreeMap::new(),
                    usage: ResourceUsage::default(),
                }
            } else {
                NodeAdapterResult::Failed {
                    class: FailureClass::Timeout,
                    message: "node exceeded cooperative deadline".to_string(),
                    retryable: true,
                    usage: ResourceUsage::default(),
                }
            }
        } else {
            adapter_result
        };
        match result {
            NodeAdapterResult::Succeeded {
                outputs,
                usage: attempt_usage,
            } => {
                validate_node_outputs(&prepared.ir_node, &outputs)?;
                validate_node_usage(node, &attempt_usage)?;
                usage.add_assign(&attempt_usage);
                return Ok(NodeCompletion {
                    node_id: node.node_id.clone(),
                    status: NodeRunStatus::Succeeded,
                    inputs: prepared.inputs,
                    secrets: prepared.secrets,
                    outputs,
                    pending_outputs: BTreeMap::new(),
                    attempts: attempt,
                    started_unix_ms: started,
                    finished_unix_ms: clock.now_unix_ms(),
                    usage,
                });
            }
            NodeAdapterResult::AmbiguousExternalEffect {
                receipt,
                pending_outputs,
                usage: attempt_usage,
            } => {
                if node.kind.effect() != EffectClass::ExternalMutation
                    || receipt.trim().is_empty()
                    || receipt.len() > 8_192
                {
                    return Err(WorkflowError::Execution(
                        "ambiguous result is only valid for external mutations with a receipt"
                            .to_string(),
                    ));
                }
                if !pending_outputs.is_empty() {
                    validate_node_outputs(&prepared.ir_node, &pending_outputs)?;
                }
                validate_node_usage(node, &attempt_usage)?;
                usage.add_assign(&attempt_usage);
                return Ok(NodeCompletion {
                    node_id: node.node_id.clone(),
                    status: NodeRunStatus::NeedsReconciliation { receipt },
                    inputs: prepared.inputs,
                    secrets: prepared.secrets,
                    outputs: BTreeMap::new(),
                    pending_outputs,
                    attempts: attempt,
                    started_unix_ms: started,
                    finished_unix_ms: clock.now_unix_ms(),
                    usage,
                });
            }
            NodeAdapterResult::Failed {
                class,
                message,
                retryable,
                usage: attempt_usage,
            } => {
                validate_node_usage(node, &attempt_usage)?;
                usage.add_assign(&attempt_usage);
                let may_retry = retryable
                    && attempt < node.retry.maximum_attempts
                    && node.retry.retry_on.contains(&class)
                    && (matches!(
                        node.kind.effect(),
                        EffectClass::Pure | EffectClass::ReadOnly
                    ) || !matches!(node.idempotency, IdempotencyPolicy::None));
                if may_retry {
                    clock
                        .sleep_ms(backoff, cancel)
                        .map_err(WorkflowError::Execution)?;
                    backoff = backoff
                        .max(1)
                        .saturating_mul(2)
                        .min(node.retry.maximum_backoff_ms);
                    continue;
                }
                return Ok(NodeCompletion {
                    node_id: node.node_id.clone(),
                    status: NodeRunStatus::Failed { class, message },
                    inputs: prepared.inputs,
                    secrets: prepared.secrets,
                    outputs: BTreeMap::new(),
                    pending_outputs: BTreeMap::new(),
                    attempts: attempt,
                    started_unix_ms: started,
                    finished_unix_ms: clock.now_unix_ms(),
                    usage,
                });
            }
        }
    }
    Err(WorkflowError::Execution(
        "retry loop exited without a terminal result".to_string(),
    ))
}

fn validate_node_outputs(
    ir_node: &WorkflowIrNode,
    outputs: &BTreeMap<String, WorkflowValue>,
) -> WorkflowResult<()> {
    if outputs.len() != ir_node.output_types.len()
        || outputs
            .keys()
            .any(|port| !ir_node.output_types.contains_key(port))
    {
        return Err(WorkflowError::TypeMismatch(format!(
            "node {} returned wrong output ports",
            ir_node.node.node_id
        )));
    }
    for (port, expected) in &ir_node.output_types {
        if outputs[port].value_type()? != *expected {
            return Err(WorkflowError::TypeMismatch(format!(
                "node {}.{port} returned wrong type",
                ir_node.node.node_id
            )));
        }
    }
    if serde_json::to_vec(outputs)?.len() > 32 * 1024 * 1024 {
        return Err(WorkflowError::TypeMismatch(
            "node output exceeds 32 MiB".to_string(),
        ));
    }
    Ok(())
}

fn validate_node_usage(node: &WorkflowNode, usage: &ResourceUsage) -> WorkflowResult<()> {
    if !usage.within_estimate(&node.estimate) {
        return Err(WorkflowError::BudgetExceeded(format!(
            "node {} exceeded its declared per-attempt resource estimate",
            node.node_id
        )));
    }
    if let WorkflowNodeKind::BoundedLoop { maximum_iterations } = node.kind {
        if usage.loop_iterations > u64::from(maximum_iterations) {
            return Err(WorkflowError::BudgetExceeded(format!(
                "bounded loop {} exceeded maximum_iterations",
                node.node_id
            )));
        }
    } else if usage.loop_iterations != 0 {
        return Err(WorkflowError::Execution(
            "non-loop node reported loop iterations".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplayPlan {
    pub workflow_id: String,
    pub definition_sha256: String,
    pub source_run_id: String,
    pub boundary_node_id: String,
    pub reused_node_ids: BTreeSet<String>,
    pub execute_node_ids: BTreeSet<String>,
}

pub fn plan_replay(
    ir: &WorkflowIr,
    source: &WorkflowRunHistory,
    boundary_node_id: &str,
    replay_approval_granted: bool,
) -> WorkflowResult<ReplayPlan> {
    if source.workflow_id != ir.workflow_id || source.definition_sha256 != ir.definition_sha256 {
        return Err(WorkflowError::Replay(
            "source run was produced by different workflow IR".to_string(),
        ));
    }
    let boundary = ir
        .nodes
        .iter()
        .find(|node| node.node.node_id == boundary_node_id)
        .ok_or_else(|| WorkflowError::Replay("replay boundary node not found".to_string()))?;
    match boundary.node.replay {
        ReplayPolicy::Never => {
            return Err(WorkflowError::Replay(
                "boundary node forbids replay".to_string(),
            ));
        }
        ReplayPolicy::RequiresApproval if !replay_approval_granted => {
            return Err(WorkflowError::Replay(
                "boundary node requires replay approval".to_string(),
            ));
        }
        _ => {}
    }
    let mut children = BTreeMap::<String, BTreeSet<String>>::new();
    for node in &ir.nodes {
        for dependency in &node.dependencies {
            children
                .entry(dependency.clone())
                .or_default()
                .insert(node.node.node_id.clone());
        }
    }
    let mut execute_node_ids = BTreeSet::from([boundary_node_id.to_string()]);
    let mut queue = vec![boundary_node_id.to_string()];
    while let Some(node) = queue.pop() {
        for child in children.get(&node).into_iter().flatten() {
            if execute_node_ids.insert(child.clone()) {
                queue.push(child.clone());
            }
        }
    }
    let reused_node_ids = ir
        .nodes
        .iter()
        .map(|node| node.node.node_id.clone())
        .filter(|node| !execute_node_ids.contains(node))
        .collect::<BTreeSet<_>>();
    for node_id in &reused_node_ids {
        let record = source.inspect_node(node_id)?;
        if !matches!(
            record.status,
            NodeRunStatus::Succeeded | NodeRunStatus::Reused { .. }
        ) {
            return Err(WorkflowError::Replay(format!(
                "node {node_id} is not safe to reuse from its terminal status"
            )));
        }
    }
    for node_id in &execute_node_ids {
        if source.nodes.get(node_id).is_some_and(|record| {
            matches!(record.status, NodeRunStatus::NeedsReconciliation { .. })
        }) {
            return Err(WorkflowError::Replay(format!(
                "node {node_id} has an unreconciled external effect"
            )));
        }
    }
    Ok(ReplayPlan {
        workflow_id: ir.workflow_id.clone(),
        definition_sha256: ir.definition_sha256.clone(),
        source_run_id: source.run_id.clone(),
        boundary_node_id: boundary_node_id.to_string(),
        reused_node_ids,
        execute_node_ids,
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationDecision {
    VerifiedApplied,
    VerifiedNotApplied,
    Abandon,
}

pub fn reconcile_node(
    history: &mut WorkflowRunHistory,
    node_id: &str,
    decision: ReconciliationDecision,
    now_unix_ms: u64,
) -> WorkflowResult<()> {
    let record = history.nodes.get_mut(node_id).ok_or_else(|| {
        WorkflowError::NeedsReconciliation(format!("node record {node_id} not found"))
    })?;
    if !matches!(record.status, NodeRunStatus::NeedsReconciliation { .. }) {
        return Err(WorkflowError::NeedsReconciliation(format!(
            "node {node_id} is not awaiting reconciliation"
        )));
    }
    match decision {
        ReconciliationDecision::VerifiedApplied => {
            record.outputs = std::mem::take(&mut record.pending_outputs);
            record.status = NodeRunStatus::Succeeded;
        }
        ReconciliationDecision::VerifiedNotApplied => {
            record.pending_outputs.clear();
            record.status = NodeRunStatus::Failed {
                class: FailureClass::Transient,
                message: "verified external effect was not applied; safe replay may be planned"
                    .to_string(),
            };
        }
        ReconciliationDecision::Abandon => {
            record.pending_outputs.clear();
            record.status = NodeRunStatus::Failed {
                class: FailureClass::Permanent,
                message: "external effect abandoned after manual reconciliation".to_string(),
            };
        }
    }
    history.status = WorkflowRunStatus::Failed;
    history.push_event(
        now_unix_ms,
        WorkflowRunEventKind::Reconciled {
            node_id: node_id.to_string(),
            decision,
        },
    );
    Ok(())
}

#[derive(Debug, Clone)]
pub struct WorkflowCoreFixture {
    pub fixture_id: String,
    pub workflow: WorkflowDefinition,
}

/// Five deterministic fixtures shared by visual-editor and headless tests.
pub fn workflow_core_fixtures() -> Vec<WorkflowCoreFixture> {
    vec![
        prompt_fixture(),
        condition_fixture(),
        parallel_transform_fixture(),
        approval_tool_fixture(),
        bounded_loop_fixture(),
    ]
}

pub fn workflow_core_fixture_capabilities() -> WorkflowCapabilityCatalog {
    WorkflowCapabilityCatalog {
        model_backends: BTreeSet::from(["default".to_string()]),
        tools: BTreeMap::from([("fixture_write".to_string(), EffectClass::LocalMutation)]),
        transforms: BTreeSet::from(["identity".to_string()]),
        ..WorkflowCapabilityCatalog::default()
    }
}

fn fixture_workflow(
    id: &str,
    nodes: Vec<WorkflowNode>,
    outputs: BTreeMap<String, WorkflowOutput>,
) -> WorkflowDefinition {
    WorkflowDefinition {
        schema_version: WORKFLOW_SCHEMA_VERSION,
        workflow_id: format!("fixture:{id}"),
        workflow_version: 1,
        name: format!("Fixture {id}"),
        inputs: BTreeMap::new(),
        secrets: BTreeMap::new(),
        nodes,
        outputs,
        budgets: WorkflowBudgets {
            maximum_node_executions: 100,
            maximum_model_calls: 10,
            maximum_input_tokens: 100_000,
            maximum_output_tokens: 100_000,
            maximum_cost_microunits: 10_000_000,
            maximum_wall_time_ms: 60_000,
        },
        maximum_concurrency: 4,
        triggers: vec![WorkflowTrigger::Manual],
    }
}

fn fixture_node(
    node_id: &str,
    kind: WorkflowNodeKind,
    inputs: BTreeMap<String, InputBinding>,
) -> WorkflowNode {
    WorkflowNode {
        node_id: node_id.to_string(),
        kind,
        inputs,
        secret_ids: BTreeSet::new(),
        permission_policy: NodePermissionPolicy {
            permission_ids: BTreeSet::new(),
            approval_node_id: None,
        },
        retry: RetryPolicy::default(),
        timeout_ms: 10_000,
        estimate: ResourceEstimate {
            model_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost_microunits: 0,
        },
        idempotency: IdempotencyPolicy::None,
        replay: ReplayPolicy::Safe,
        guard: None,
    }
}

fn output_from(node_id: &str, value_type: WorkflowValueType) -> WorkflowOutput {
    WorkflowOutput {
        value_type,
        binding: InputBinding::NodeOutput {
            node_id: node_id.to_string(),
            port: "out".to_string(),
        },
    }
}

fn prompt_fixture() -> WorkflowCoreFixture {
    let mut node = fixture_node(
        "prompt",
        WorkflowNodeKind::PromptModel {
            model_selector: "default".to_string(),
        },
        BTreeMap::from([(
            "prompt".to_string(),
            InputBinding::Literal {
                value: WorkflowValue::String("Summarize the fixture".to_string()),
            },
        )]),
    );
    node.estimate = ResourceEstimate {
        model_calls: 1,
        input_tokens: 1_000,
        output_tokens: 1_000,
        cost_microunits: 1_000_000,
    };
    WorkflowCoreFixture {
        fixture_id: "prompt".to_string(),
        workflow: fixture_workflow(
            "prompt",
            vec![node],
            BTreeMap::from([(
                "text".to_string(),
                output_from("prompt", WorkflowValueType::String),
            )]),
        ),
    }
}

fn condition_fixture() -> WorkflowCoreFixture {
    let condition = fixture_node(
        "condition",
        WorkflowNodeKind::Condition,
        BTreeMap::from([(
            "condition".to_string(),
            InputBinding::Literal {
                value: WorkflowValue::Boolean(true),
            },
        )]),
    );
    let mut guarded = fixture_node(
        "guarded",
        WorkflowNodeKind::Transform {
            transform_id: "identity".to_string(),
        },
        BTreeMap::from([(
            "input".to_string(),
            InputBinding::Literal {
                value: WorkflowValue::Json(serde_json::json!({"branch": "true"})),
            },
        )]),
    );
    guarded.guard = Some(ConditionGuard {
        condition_node_id: "condition".to_string(),
        expected: true,
    });
    WorkflowCoreFixture {
        fixture_id: "condition".to_string(),
        workflow: fixture_workflow(
            "condition",
            vec![condition, guarded],
            BTreeMap::from([(
                "condition".to_string(),
                output_from("condition", WorkflowValueType::Boolean),
            )]),
        ),
    }
}

fn parallel_transform_fixture() -> WorkflowCoreFixture {
    let left = fixture_node(
        "left",
        WorkflowNodeKind::Transform {
            transform_id: "identity".to_string(),
        },
        BTreeMap::from([(
            "input".to_string(),
            InputBinding::Literal {
                value: WorkflowValue::Json(serde_json::json!({"side": "left"})),
            },
        )]),
    );
    let right = fixture_node(
        "right",
        WorkflowNodeKind::Transform {
            transform_id: "identity".to_string(),
        },
        BTreeMap::from([(
            "input".to_string(),
            InputBinding::Literal {
                value: WorkflowValue::Json(serde_json::json!({"side": "right"})),
            },
        )]),
    );
    WorkflowCoreFixture {
        fixture_id: "parallel-transform".to_string(),
        workflow: fixture_workflow(
            "parallel-transform",
            vec![left, right],
            BTreeMap::from([
                (
                    "left".to_string(),
                    output_from("left", WorkflowValueType::Json),
                ),
                (
                    "right".to_string(),
                    output_from("right", WorkflowValueType::Json),
                ),
            ]),
        ),
    }
}

fn approval_tool_fixture() -> WorkflowCoreFixture {
    let approval = fixture_node(
        "approve",
        WorkflowNodeKind::HumanApproval {
            approval_policy_id: "fixture-tool".to_string(),
        },
        BTreeMap::from([(
            "summary".to_string(),
            InputBinding::Literal {
                value: WorkflowValue::String("Allow fixture mutation?".to_string()),
            },
        )]),
    );
    let mut tool = fixture_node(
        "tool",
        WorkflowNodeKind::Tool {
            tool_id: "fixture_write".to_string(),
            effect: EffectClass::LocalMutation,
        },
        BTreeMap::from([
            (
                "arguments".to_string(),
                InputBinding::Literal {
                    value: WorkflowValue::Json(serde_json::json!({"value": 1})),
                },
            ),
            (
                "approval".to_string(),
                InputBinding::NodeOutput {
                    node_id: "approve".to_string(),
                    port: "out".to_string(),
                },
            ),
        ]),
    );
    tool.permission_policy = NodePermissionPolicy {
        permission_ids: ["fixture-write".to_string()].into_iter().collect(),
        approval_node_id: Some("approve".to_string()),
    };
    tool.idempotency = IdempotencyPolicy::Keyed {
        key_template: "fixture:{run_id}".to_string(),
    };
    tool.replay = ReplayPolicy::RequiresApproval;
    WorkflowCoreFixture {
        fixture_id: "approval-tool".to_string(),
        workflow: fixture_workflow(
            "approval-tool",
            vec![approval, tool],
            BTreeMap::from([(
                "result".to_string(),
                output_from("tool", WorkflowValueType::Json),
            )]),
        ),
    }
}

fn bounded_loop_fixture() -> WorkflowCoreFixture {
    let loop_node = fixture_node(
        "loop",
        WorkflowNodeKind::BoundedLoop {
            maximum_iterations: 3,
        },
        BTreeMap::from([(
            "input".to_string(),
            InputBinding::Literal {
                value: WorkflowValue::Json(serde_json::json!([1, 2, 3])),
            },
        )]),
    );
    WorkflowCoreFixture {
        fixture_id: "bounded-loop".to_string(),
        workflow: fixture_workflow(
            "bounded-loop",
            vec![loop_node],
            BTreeMap::from([(
                "results".to_string(),
                output_from("loop", WorkflowValueType::Json),
            )]),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    #[derive(Default)]
    struct TestClock {
        now: AtomicU64,
    }

    impl TestClock {
        fn at(unix_ms: u64) -> Self {
            Self {
                now: AtomicU64::new(unix_ms),
            }
        }
    }

    impl WorkflowClock for TestClock {
        fn now_unix_ms(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }

        fn sleep_ms(&self, duration_ms: u64, cancel: &CancellationToken) -> Result<(), String> {
            if cancel.is_cancelled() {
                return Err("cancelled".to_string());
            }
            self.now.fetch_add(duration_ms, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestBehavior {
        Normal,
        FailFirst,
        Ambiguous,
        ExceedEstimate,
        DenyApproval,
    }

    struct FixtureExecutor {
        behavior: TestBehavior,
        target_node: Option<String>,
        calls: Mutex<BTreeMap<String, u32>>,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
    }

    impl FixtureExecutor {
        fn new(behavior: TestBehavior, target_node: Option<&str>) -> Self {
            Self {
                behavior,
                target_node: target_node.map(str::to_string),
                calls: Mutex::new(BTreeMap::new()),
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
            }
        }

        fn call_count(&self, node_id: &str) -> u32 {
            self.calls
                .lock()
                .unwrap()
                .get(node_id)
                .copied()
                .unwrap_or_default()
        }

        fn targeted(&self, node_id: &str) -> bool {
            self.target_node.as_deref() == Some(node_id)
        }

        fn normal_result(&self, request: &NodeExecutionRequest) -> NodeAdapterResult {
            let (output, mut usage) = match &request.node.kind {
                WorkflowNodeKind::PromptModel { .. }
                | WorkflowNodeKind::Agent { .. }
                | WorkflowNodeKind::Subagent { .. } => (
                    WorkflowValue::String("fixture response".to_string()),
                    ResourceUsage {
                        model_calls: 1,
                        input_tokens: 10,
                        output_tokens: 10,
                        cost_microunits: 10,
                        loop_iterations: 0,
                    },
                ),
                WorkflowNodeKind::LegacyRecipe { recipe } => (
                    WorkflowValue::String(format!("legacy:{}", recipe.name)),
                    ResourceUsage {
                        model_calls: 1,
                        input_tokens: 10,
                        output_tokens: 10,
                        cost_microunits: 10,
                        loop_iterations: 0,
                    },
                ),
                WorkflowNodeKind::Condition => (
                    request.inputs["condition"].clone(),
                    ResourceUsage::default(),
                ),
                WorkflowNodeKind::HumanApproval { .. } => (
                    WorkflowValue::Boolean(self.behavior != TestBehavior::DenyApproval),
                    ResourceUsage::default(),
                ),
                WorkflowNodeKind::Transform { .. }
                | WorkflowNodeKind::Verify { .. }
                | WorkflowNodeKind::BoundedLoop { .. } => {
                    let input = request.inputs["input"].clone();
                    let iterations = match &request.node.kind {
                        WorkflowNodeKind::BoundedLoop { maximum_iterations } => {
                            u64::from(*maximum_iterations)
                        }
                        _ => 0,
                    };
                    (
                        input,
                        ResourceUsage {
                            loop_iterations: iterations,
                            ..ResourceUsage::default()
                        },
                    )
                }
                WorkflowNodeKind::Tool { .. }
                | WorkflowNodeKind::Mcp { .. }
                | WorkflowNodeKind::Browser { .. }
                | WorkflowNodeKind::Git { .. }
                | WorkflowNodeKind::PullRequest { .. } => (
                    request.inputs["arguments"].clone(),
                    ResourceUsage::default(),
                ),
                WorkflowNodeKind::Shell { .. } => (
                    WorkflowValue::Json(serde_json::json!({"exit_code": 0})),
                    ResourceUsage::default(),
                ),
                WorkflowNodeKind::Artifact { media_type } => {
                    let bytes = serde_json::to_vec(&request.inputs["content"]).unwrap();
                    (
                        WorkflowValue::Artifact(ArtifactReference {
                            artifact_id: "fixture-artifact".to_string(),
                            sha256: sha256(&bytes),
                            media_type: media_type.clone(),
                        }),
                        ResourceUsage::default(),
                    )
                }
                WorkflowNodeKind::Output => {
                    (request.inputs["value"].clone(), ResourceUsage::default())
                }
            };
            if self.behavior == TestBehavior::ExceedEstimate && self.targeted(&request.node.node_id)
            {
                usage.model_calls = request.node.estimate.model_calls.saturating_add(1);
            }
            NodeAdapterResult::Succeeded {
                outputs: BTreeMap::from([("out".to_string(), output)]),
                usage,
            }
        }
    }

    impl WorkflowNodeExecutor for FixtureExecutor {
        fn execute(
            &self,
            request: NodeExecutionRequest,
            _cancel: &CancellationToken,
        ) -> Result<NodeAdapterResult, String> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum_active.fetch_max(active, Ordering::SeqCst);
            if matches!(request.node.kind, WorkflowNodeKind::Transform { .. }) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let call = {
                let mut calls = self.calls.lock().unwrap();
                let entry = calls.entry(request.node.node_id.clone()).or_default();
                *entry += 1;
                *entry
            };
            let result = if self.targeted(&request.node.node_id)
                && self.behavior == TestBehavior::FailFirst
                && call == 1
            {
                NodeAdapterResult::Failed {
                    class: FailureClass::Transient,
                    message: "retry me".to_string(),
                    retryable: true,
                    usage: ResourceUsage::default(),
                }
            } else if self.targeted(&request.node.node_id)
                && self.behavior == TestBehavior::Ambiguous
            {
                NodeAdapterResult::AmbiguousExternalEffect {
                    receipt: "external-receipt-1".to_string(),
                    pending_outputs: BTreeMap::from([(
                        "out".to_string(),
                        request.inputs["arguments"].clone(),
                    )]),
                    usage: ResourceUsage::default(),
                }
            } else {
                self.normal_result(&request)
            };
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(result)
        }
    }

    fn run_request(run_id: &str) -> WorkflowRunRequest {
        WorkflowRunRequest {
            run_id: run_id.to_string(),
            inputs: BTreeMap::new(),
            secret_bindings: BTreeMap::new(),
            trigger: WorkflowTrigger::Manual,
        }
    }

    fn compile_fixture(fixture: &WorkflowCoreFixture) -> WorkflowIr {
        compile_workflow(
            &fixture.workflow,
            &BTreeSet::new(),
            &workflow_core_fixture_capabilities(),
        )
        .unwrap()
    }

    #[test]
    fn five_editor_fixtures_compile_round_trip_and_execute_headlessly() {
        let fixtures = workflow_core_fixtures();
        assert_eq!(fixtures.len(), 5);
        for fixture in fixtures {
            let ir = compile_fixture(&fixture);
            let serialized = serde_json::to_vec(&ir).unwrap();
            let round_trip: WorkflowIr = serde_json::from_slice(&serialized).unwrap();
            assert_eq!(round_trip, ir);
            let adapter = FixtureExecutor::new(TestBehavior::Normal, None);
            let clock = TestClock::at(1_000);
            let history = HeadlessWorkflowExecutor::new(&adapter, &clock)
                .run(
                    &ir,
                    run_request(&format!("run-{}", fixture.fixture_id)),
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(history.status, WorkflowRunStatus::Succeeded);
            assert!(!history.outputs.is_empty());
            assert_eq!(history.events.first().unwrap().sequence, 1);
            assert!(history
                .events
                .windows(2)
                .all(|events| events[0].sequence + 1 == events[1].sequence));
        }
    }

    #[test]
    fn legacy_recipe_adapter_is_strict_and_preserves_one_node_execution() {
        let recipe = LegacyRecipeV1 {
            version: LEGACY_RECIPE_VERSION,
            name: "summarize".to_string(),
            target: LegacyRecipeTargetV1 {
                provider: Some("ollama-compatible".to_string()),
                model: Some("local-model".to_string()),
                ollama: None,
                local_url: None,
            },
            permission_mode: "ask".to_string(),
            system: Some("Be concise".to_string()),
            prompt: "Summarize {{document}}".to_string(),
            params: BTreeMap::from([("document".to_string(), None)]),
            maximum_iterations: Some(2),
            timeout_seconds: Some(30),
        };
        let definition = adapt_legacy_recipe(recipe.clone()).unwrap();
        assert_eq!(definition.nodes.len(), 1);
        let ir = compile_workflow(
            &definition,
            &BTreeSet::new(),
            &WorkflowCapabilityCatalog::default(),
        )
        .unwrap();
        let request = WorkflowRunRequest {
            run_id: "legacy-run".to_string(),
            inputs: BTreeMap::from([(
                "document".to_string(),
                WorkflowValue::String("body".to_string()),
            )]),
            secret_bindings: BTreeMap::new(),
            trigger: WorkflowTrigger::Manual,
        };
        let adapter = FixtureExecutor::new(TestBehavior::Normal, None);
        let history = HeadlessWorkflowExecutor::new(&adapter, &TestClock::at(1_000))
            .run(&ir, request, &CancellationToken::new())
            .unwrap();
        assert_eq!(
            history.outputs["text"],
            WorkflowValue::String("legacy:summarize".to_string())
        );

        let mut bypass = recipe;
        bypass.permission_mode = "bypass".to_string();
        assert!(matches!(
            adapt_legacy_recipe(bypass),
            Err(WorkflowError::InvalidDefinition(_))
        ));
    }

    #[test]
    fn compiler_rejects_cycles_types_unbounded_loops_approvals_secrets_and_effect_lies() {
        let capabilities = workflow_core_fixture_capabilities();
        let mut cycle = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap()
            .workflow;
        cycle.nodes[0].inputs.insert(
            "input".to_string(),
            InputBinding::NodeOutput {
                node_id: "right".to_string(),
                port: "out".to_string(),
            },
        );
        cycle.nodes[1].inputs.insert(
            "input".to_string(),
            InputBinding::NodeOutput {
                node_id: "left".to_string(),
                port: "out".to_string(),
            },
        );
        assert!(matches!(
            compile_workflow(&cycle, &BTreeSet::new(), &capabilities),
            Err(WorkflowError::Cycle(_))
        ));

        let mut bad_type = prompt_fixture().workflow;
        bad_type.nodes[0].inputs.insert(
            "prompt".to_string(),
            InputBinding::Literal {
                value: WorkflowValue::Boolean(true),
            },
        );
        assert!(matches!(
            compile_workflow(&bad_type, &BTreeSet::new(), &capabilities),
            Err(WorkflowError::TypeMismatch(_))
        ));

        let mut unbounded = bounded_loop_fixture().workflow;
        unbounded.nodes[0].kind = WorkflowNodeKind::BoundedLoop {
            maximum_iterations: 0,
        };
        assert!(matches!(
            compile_workflow(&unbounded, &BTreeSet::new(), &capabilities),
            Err(WorkflowError::InvalidDefinition(_))
        ));

        let mut missing_approval = approval_tool_fixture().workflow;
        missing_approval.nodes[1].permission_policy.approval_node_id = None;
        assert!(matches!(
            compile_workflow(&missing_approval, &BTreeSet::new(), &capabilities),
            Err(WorkflowError::MissingApproval(_))
        ));

        let mut invalid_secret = prompt_fixture().workflow;
        invalid_secret.nodes[0]
            .secret_ids
            .insert("undeclared".to_string());
        assert!(matches!(
            compile_workflow(&invalid_secret, &BTreeSet::new(), &capabilities),
            Err(WorkflowError::InvalidSecret(_))
        ));

        let mut effect_lie = approval_tool_fixture().workflow;
        let lying_catalog = WorkflowCapabilityCatalog {
            tools: BTreeMap::from([("fixture_write".to_string(), EffectClass::ReadOnly)]),
            ..WorkflowCapabilityCatalog::default()
        };
        assert!(matches!(
            compile_workflow(&effect_lie, &BTreeSet::new(), &lying_catalog),
            Err(WorkflowError::InvalidDefinition(_))
        ));
        effect_lie.nodes[1].kind = WorkflowNodeKind::Tool {
            tool_id: "unknown".to_string(),
            effect: EffectClass::LocalMutation,
        };
        assert!(compile_workflow(&effect_lie, &BTreeSet::new(), &capabilities).is_err());

        let approval_ir = compile_fixture(&approval_tool_fixture());
        let denied = FixtureExecutor::new(TestBehavior::DenyApproval, None);
        assert!(matches!(
            HeadlessWorkflowExecutor::new(&denied, &TestClock::at(1_000)).run(
                &approval_ir,
                run_request("denied-run"),
                &CancellationToken::new(),
            ),
            Err(WorkflowError::MissingApproval(_))
        ));
        assert_eq!(denied.call_count("tool"), 0);
    }

    #[test]
    fn browser_git_and_pull_request_nodes_are_typed_and_capability_gated() {
        let mut git = parallel_transform_fixture().workflow;
        git.nodes.truncate(1);
        git.nodes[0].kind = WorkflowNodeKind::Git {
            action: "inspect_worktree".to_string(),
            effect: EffectClass::ReadOnly,
        };
        git.nodes[0].inputs = BTreeMap::from([(
            "arguments".to_string(),
            InputBinding::Literal {
                value: WorkflowValue::Json(serde_json::json!({"worktreeId":"owned-1"})),
            },
        )]);
        git.outputs = BTreeMap::from([(
            "result".to_string(),
            output_from(&git.nodes[0].node_id, WorkflowValueType::Json),
        )]);
        let catalog = WorkflowCapabilityCatalog {
            git_actions: BTreeMap::from([("inspect_worktree".to_string(), EffectClass::ReadOnly)]),
            ..WorkflowCapabilityCatalog::default()
        };
        assert!(compile_workflow(&git, &BTreeSet::new(), &catalog).is_ok());
        git.nodes[0].kind = WorkflowNodeKind::Git {
            action: "inspect_worktree".to_string(),
            effect: EffectClass::ExternalMutation,
        };
        assert!(matches!(
            compile_workflow(&git, &BTreeSet::new(), &catalog),
            Err(WorkflowError::InvalidDefinition(_))
        ));

        let mut mutation = approval_tool_fixture().workflow;
        mutation.nodes[1].kind = WorkflowNodeKind::Browser {
            action: "click".to_string(),
            effect: EffectClass::ExternalMutation,
        };
        let browser_catalog = WorkflowCapabilityCatalog {
            browser_actions: BTreeMap::from([("click".to_string(), EffectClass::ExternalMutation)]),
            ..WorkflowCapabilityCatalog::default()
        };
        assert!(compile_workflow(&mutation, &BTreeSet::new(), &browser_catalog).is_ok());
        mutation.nodes[1].permission_policy.approval_node_id = None;
        assert!(matches!(
            compile_workflow(&mutation, &BTreeSet::new(), &browser_catalog),
            Err(WorkflowError::MissingApproval(_))
        ));

        let mut pull_request = git;
        pull_request.nodes[0].kind = WorkflowNodeKind::PullRequest {
            action: "read_pull_request".to_string(),
            effect: EffectClass::ReadOnly,
        };
        let pr_catalog = WorkflowCapabilityCatalog {
            pull_request_actions: BTreeMap::from([(
                "read_pull_request".to_string(),
                EffectClass::ReadOnly,
            )]),
            ..WorkflowCapabilityCatalog::default()
        };
        assert!(compile_workflow(&pull_request, &BTreeSet::new(), &pr_catalog).is_ok());
    }

    #[test]
    fn executor_runs_independent_nodes_concurrently_within_declared_limit() {
        let fixture = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap();
        let ir = compile_fixture(&fixture);
        let adapter = FixtureExecutor::new(TestBehavior::Normal, None);
        let history = HeadlessWorkflowExecutor::new(&adapter, &TestClock::at(1_000))
            .run(&ir, run_request("parallel-run"), &CancellationToken::new())
            .unwrap();
        assert_eq!(history.status, WorkflowRunStatus::Succeeded);
        let maximum = adapter.maximum_active.load(Ordering::SeqCst);
        assert!(maximum > 1, "independent nodes did not overlap");
        assert!(maximum <= ir.maximum_concurrency);
    }

    #[test]
    fn retries_budget_reservations_and_per_attempt_estimates_are_enforced() {
        let mut fixture = prompt_fixture();
        fixture.workflow.nodes[0].retry = RetryPolicy {
            maximum_attempts: 2,
            initial_backoff_ms: 1,
            maximum_backoff_ms: 1,
            retry_on: BTreeSet::from([FailureClass::Transient]),
        };
        let ir = compile_fixture(&fixture);
        let adapter = FixtureExecutor::new(TestBehavior::FailFirst, Some("prompt"));
        let history = HeadlessWorkflowExecutor::new(&adapter, &TestClock::at(1_000))
            .run(&ir, run_request("retry-run"), &CancellationToken::new())
            .unwrap();
        assert_eq!(history.status, WorkflowRunStatus::Succeeded);
        assert_eq!(history.inspect_node("prompt").unwrap().attempts, 2);

        let mut budgeted = fixture.workflow.clone();
        budgeted.budgets.maximum_model_calls = 1;
        let budgeted_ir = compile_workflow(
            &budgeted,
            &BTreeSet::new(),
            &workflow_core_fixture_capabilities(),
        )
        .unwrap();
        let never_called = FixtureExecutor::new(TestBehavior::Normal, None);
        assert!(matches!(
            HeadlessWorkflowExecutor::new(&never_called, &TestClock::at(1_000)).run(
                &budgeted_ir,
                run_request("budget-run"),
                &CancellationToken::new(),
            ),
            Err(WorkflowError::BudgetExceeded(_))
        ));
        assert_eq!(never_called.call_count("prompt"), 0);

        let normal = compile_fixture(&prompt_fixture());
        let over = FixtureExecutor::new(TestBehavior::ExceedEstimate, Some("prompt"));
        assert!(matches!(
            HeadlessWorkflowExecutor::new(&over, &TestClock::at(1_000)).run(
                &normal,
                run_request("estimate-run"),
                &CancellationToken::new(),
            ),
            Err(WorkflowError::BudgetExceeded(_))
        ));
    }

    fn external_workflow() -> (WorkflowDefinition, WorkflowCapabilityCatalog) {
        let mut definition = approval_tool_fixture().workflow;
        definition.workflow_id = "fixture:external".to_string();
        definition.nodes[1].kind = WorkflowNodeKind::Tool {
            tool_id: "external_write".to_string(),
            effect: EffectClass::ExternalMutation,
        };
        definition.nodes[1].permission_policy.permission_ids =
            BTreeSet::from(["external-write".to_string()]);
        let capabilities = WorkflowCapabilityCatalog {
            tools: BTreeMap::from([("external_write".to_string(), EffectClass::ExternalMutation)]),
            ..WorkflowCapabilityCatalog::default()
        };
        (definition, capabilities)
    }

    #[test]
    fn ambiguous_external_effect_stops_without_retry_and_requires_reconciliation() {
        let (definition, capabilities) = external_workflow();
        let ir = compile_workflow(&definition, &BTreeSet::new(), &capabilities).unwrap();
        let adapter = FixtureExecutor::new(TestBehavior::Ambiguous, Some("tool"));
        let mut history = HeadlessWorkflowExecutor::new(&adapter, &TestClock::at(1_000))
            .run(&ir, run_request("external-run"), &CancellationToken::new())
            .unwrap();
        assert_eq!(history.status, WorkflowRunStatus::NeedsReconciliation);
        assert_eq!(adapter.call_count("tool"), 1);
        assert!(matches!(
            history.inspect_node("tool").unwrap().status,
            NodeRunStatus::NeedsReconciliation { .. }
        ));
        assert!(plan_replay(&ir, &history, "tool", true).is_err());
        reconcile_node(
            &mut history,
            "tool",
            ReconciliationDecision::VerifiedApplied,
            2_000,
        )
        .unwrap();
        assert_eq!(
            history.inspect_node("tool").unwrap().status,
            NodeRunStatus::Succeeded
        );
        assert_eq!(
            history.inspect_node("tool").unwrap().outputs["out"],
            WorkflowValue::Json(serde_json::json!({"value": 1}))
        );
    }

    #[test]
    fn history_inspection_and_replay_reuse_upstream_nodes_and_enforce_policy() {
        let fixture = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap();
        let ir = compile_fixture(&fixture);
        let adapter = FixtureExecutor::new(TestBehavior::Normal, None);
        let clock = TestClock::at(1_000);
        let runner = HeadlessWorkflowExecutor::new(&adapter, &clock);
        let source = runner
            .run(&ir, run_request("source-run"), &CancellationToken::new())
            .unwrap();
        assert_eq!(
            source.inspect_node("left").unwrap().outputs["out"],
            WorkflowValue::Json(serde_json::json!({"side": "left"}))
        );
        let plan = plan_replay(&ir, &source, "left", false).unwrap();
        assert_eq!(plan.reused_node_ids, BTreeSet::from(["right".to_string()]));
        let replayed = runner
            .replay(
                &ir,
                run_request("replay-run"),
                &source,
                &plan,
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(
            replayed.inspect_node("right").unwrap().status,
            NodeRunStatus::Reused { .. }
        ));
        assert_eq!(adapter.call_count("left"), 2);
        assert_eq!(adapter.call_count("right"), 1);

        let approval_fixture = approval_tool_fixture();
        let approval_ir = compile_fixture(&approval_fixture);
        let approval_source = runner
            .run(
                &approval_ir,
                run_request("approval-source"),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(plan_replay(&approval_ir, &approval_source, "tool", false).is_err());
        assert!(plan_replay(&approval_ir, &approval_source, "tool", true).is_ok());
    }

    #[test]
    fn persistent_triggers_are_rejected_without_exact_daemon_capability() {
        let triggers = vec![WorkflowTrigger::PersistentCron {
            expression: "*/5 * * * *".to_string(),
        }];
        assert!(matches!(
            validate_triggers(&triggers, &BTreeSet::new()),
            Err(WorkflowError::UnsupportedTrigger(_))
        ));
        validate_triggers(
            &triggers,
            &BTreeSet::from([DaemonCapability::PersistentCron]),
        )
        .unwrap();

        let webhook = vec![WorkflowTrigger::SignedWebhook {
            webhook_id: "release-hook".to_string(),
            secret_reference: "vault-hook-secret".to_string(),
            replay_window_ms: 60_000,
        }];
        assert!(validate_triggers(
            &webhook,
            &BTreeSet::from([DaemonCapability::FilesystemWatch]),
        )
        .is_err());
        validate_triggers(&webhook, &BTreeSet::from([DaemonCapability::SignedWebhook])).unwrap();
    }
}
