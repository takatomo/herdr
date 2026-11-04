//! herdr-core (`hc`) terminals, as the connected Herdr server sees them.
//!
//! Names stay neutral: a "core terminal" is a terminal of the core Herdr is
//! connected to (or of a machine that core added), addressed by its path as
//! that core names it (`t7`, `rohan/t7`) and identified by the core id that
//! runs it plus its address there.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::common::SplitDirection;
use super::panes::PaneInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoreViewMode {
    Control,
    Observe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CorePlacementParam {
    /// Focus the pane already showing it, else a new tab.
    Existing,
    Tab,
    Split,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreStatusParams {}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreTerminalListParams {
    /// A core word (`working`, `blocked`, ...), `attention`, `finished` or `no_status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// `local` or a core machine name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Folder prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Case-insensitive text in the path, name, title, template or message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
    /// Only terminals with (true) or without (false) a pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreTerminalOpenParams {
    /// The core id the caller read the terminal from; refused when Herdr is
    /// connected to another core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_core_id: Option<String>,
    /// Path (`t7`, `rohan/t7`) or name.
    pub terminal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CoreViewMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<CorePlacementParam>,
    /// Pane to split (default: the focused pane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<SplitDirection>,
    /// Default: true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
    /// Close the popup this request came from, after the pane is placed.
    #[serde(default)]
    pub close_popup: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreTerminalRunParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_core_id: Option<String>,
    /// Program and arguments, or a template name and its task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    /// A shell line instead of argv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Unique among live terminals; the core refuses a second live one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default)]
    pub outlive_parent: bool,
    #[serde(default)]
    pub keep: bool,
    /// A core machine to run it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    #[serde(default)]
    pub no_notify: bool,
    /// Open it in a pane after it starts (omit to only start it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<CoreRunOpen>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreRunOpen {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CoreViewMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<CorePlacementParam>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
    #[serde(default)]
    pub close_popup: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoreAttentionAction {
    /// How many core terminals need a person.
    Count,
    /// Go to the one waiting longest.
    Next,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreAttentionParams {
    pub action: CoreAttentionAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<CorePlacementParam>,
    #[serde(default)]
    pub close_popup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreIdentityInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreMachineInfo {
    /// The core's name for it (`rohan` in `rohan/t7`); not a Herdr machine.
    pub name: String,
    pub id: String,
    pub host: String,
    pub state: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreStatusInfo {
    pub enabled: bool,
    /// disabled, unsupported, connecting, connected or disconnected.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The socket Herdr connects to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<CoreIdentityInfo>,
    /// The running core's version (from hello).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub machines: Vec<CoreMachineInfo>,
    pub live: usize,
    pub finished: usize,
    /// Live terminals by the core's status word (`no_status` when unreported).
    pub by_state: BTreeMap<String, usize>,
    pub attention: usize,
    /// When the longest-waiting terminal needing a person reported, ms since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_attention_ms: Option<u64>,
    /// Core panes in this Herdr server.
    pub panes: usize,
    pub herdr_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CorePaneRef {
    pub pane_id: String,
    pub mode: CoreViewMode,
    /// pending, attached, closing, lost, interrupted, mismatch, not_found or ended.
    pub phase: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreTerminalInfo {
    pub path: String,
    pub origin_core_id: String,
    pub origin_address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// live or finished.
    pub state: String,
    /// The program's own word (OSC 7501), if it reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// How Herdr shows it (`working`, `blocked: permission`, `error`, `turn done`, `exited 0`, ...).
    pub label: String,
    pub attention: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub turn: u64,
    pub title: String,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub panes: Vec<CorePaneRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreOpenedInfo {
    pub pane: PaneInfo,
    /// False when an existing pane was focused instead.
    pub created: bool,
}

/// Whether a template run's first message went in, as the core answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreRunFirstMessage {
    pub sent: bool,
    /// Whether the agent started working on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<bool>,
    /// Why it was not sent (a trust or sign-in screen, not ready in time, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The terminal's turn when it went in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoreRunTerminal {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub endpoint_core_id: String,
}
