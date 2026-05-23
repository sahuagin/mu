# Claude Code JSONL Protocol Catalog

Mined from 300 session JSONL files (152,339 events) spanning claude-code versions
2.1.86 through 2.1.150, across three accounts (claude-personal: 261, claude-openrouter: 25,
claude: 14). Corpus covers ~2 months of use (2026-04 through 2026-05).

Companion machine-readable sidecar: `exo-protocol-catalog.json`.

## Event type inventory

| Type | Count | First version | Category |
|------|------:|---------------|----------|
| assistant | 58,806 | 2.1.108 | envelope |
| user | 35,910 | 2.1.108 | envelope |
| attachment | 13,018 | 2.1.108 | envelope |
| last-prompt | 8,772 | 2.1.108 | metadata |
| system | 8,671 | 2.1.108 | envelope |
| permission-mode | 7,709 | 2.1.108 | metadata |
| file-history-snapshot | 5,697 | 2.1.108 | metadata |
| ai-title | 4,212 | 2.1.123 | metadata |
| queue-operation | 3,419 | 2.1.108 | metadata |
| pr-link | 3,185 | 2.1.113 | metadata |
| bridge-session | 2,628 | 2.1.142 | metadata |
| agent-name | 215 | 2.1.142 | metadata |
| worktree-state | 95 | 2.1.142 | metadata |
| agent-color | 2 | 2.1.120 | metadata |

### Categories

- **Envelope events**: Carry the full message envelope (uuid chain, version, cwd,
  entrypoint, gitBranch, userType, timestamp). These form the conversation graph.
- **Metadata events**: Lightweight — typically just sessionId + type-specific payload.
  Track state transitions and bookkeeping.

## Version drift

Three waves of event type additions across 23 observed versions:

- **Core (2.1.86–2.1.108)**: user, assistant, attachment, system, last-prompt,
  permission-mode, file-history-snapshot, queue-operation — present from earliest sessions
- **Mid-lifecycle (2.1.113–2.1.123)**: pr-link (.113), agent-color (.120), ai-title (.123)
- **Recent (2.1.142)**: bridge-session, agent-name, worktree-state — all appeared together

Field-level drift is minimal. The envelope schema (uuid, parentUuid, isSidechain, cwd,
entrypoint, gitBranch, version, userType, timestamp, sessionId) is stable across all versions.
Optional fields (slug, sessionKind, agentId, attribution*) appear sparsely and version-independently.

## Per-type schemas

### assistant

Model responses. Content blocks mirror Anthropic Messages API format.

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| type | yes | `"assistant"` | |
| uuid | yes | string | Unique event ID |
| parentUuid | yes | string | Previous event in chain |
| isSidechain | yes | bool | Branched conversation |
| message | yes | object | `{role: "assistant", content: [block...]}` |
| requestId | yes | string | API request ID (`req_...`) |
| cwd | yes | string | Working directory |
| entrypoint | yes | string | `"cli"` or `"sdk-cli"` |
| gitBranch | yes | string | |
| version | yes | string | Claude-code version |
| sessionId | yes | string | Session UUID |
| timestamp | yes | string | ISO 8601 |
| userType | yes | string | `"external"` |
| slug | 26% | string | Session slug |
| sessionKind | 2% | string | `"bg"` for background |
| agentId | 4% | string | Subagent ID |
| attributionAgent | 3% | string | e.g. `"Explore"` |
| attributionSkill | 2% | string | e.g. `"goal-protocol"` |
| attributionMcpServer | <1% | string | MCP server name |
| attributionMcpTool | <1% | string | MCP tool name |
| error | <1% | string | Error description |
| isApiErrorMessage | <1% | bool | API error flag |
| apiErrorStatus | <1% | int | HTTP status |

**Content block types** (inside `message.content[]`):

| Block type | Count | Notes |
|------------|------:|-------|
| tool_use | 28,928 | `{type, id, name, input}` |
| thinking | 15,800 | `{type, thinking}` (often 0-length) |
| text | 14,109 | `{type, text}` |
| redacted_thinking | 43 | `{type, data}` |

### user

User messages and tool results. When `sourceToolAssistantUUID` is present, this is a
tool result being returned to the model.

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| type | yes | `"user"` | |
| uuid | yes | string | |
| parentUuid | yes | string/null | null for conversation root |
| isSidechain | yes | bool | |
| message | yes | object | `{role: "user", content: ...}` |
| promptId | yes | string | |
| cwd | yes | string | |
| entrypoint | yes | string | |
| gitBranch | yes | string | |
| version | yes | string | |
| sessionId | yes | string | |
| timestamp | yes | string | |
| userType | yes | string | |
| sourceToolAssistantUUID | 81% | string | Links tool result to assistant turn |
| toolUseResult | 77% | object/string/array | Tool execution result |
| isMeta | 6% | bool | System-injected message |
| permissionMode | 13% | string | Permission state at time of event |
| isCompactSummary | <1% | bool | Compaction summary message |
| isVisibleInTranscriptOnly | <1% | bool | |
| imagePasteIds | <1% | array | Pasted image IDs |
| origin | 1% | object | `{kind: "task-notification"}` etc. |
| slug | 27% | string | |
| sessionKind | 2% | string | |
| agentId | 4% | string | |

### attachment

Context injections — hooks, skills, system reminders, task lists.

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| type | yes | `"attachment"` | |
| uuid | yes | string | |
| parentUuid | yes | string/null | |
| isSidechain | yes | bool | |
| attachment | yes | object | Payload (see subtypes below) |
| cwd | yes | string | |
| entrypoint | yes | string | |
| gitBranch | yes | string | |
| version | yes | string | |
| sessionId | yes | string | |
| timestamp | yes | string | |
| userType | yes | string | |
| slug | 22% | string | |

**Attachment payload fields** (inside `attachment`):

| Field | Count | Notes |
|-------|------:|-------|
| type | 13,018 | Attachment subtype discriminator |
| content | 9,252 | Text content |
| hookName | 5,666 | Hook that produced this |
| hookEvent | 5,666 | Event that triggered hook |
| toolUseID | 5,666 | Tool use that triggered hook |
| durationMs | 5,024 | Hook execution time |
| command | 5,002 | Hook command |
| stdout | 5,002 | Hook stdout |
| stderr | 5,002 | Hook stderr |
| exitCode | 5,002 | Hook exit code |
| itemCount | 3,283 | Task/item count |
| used/total/remaining | 1,691 | Context window usage |
| prompt | 808 | |
| commandMode | 808 | |
| files | 376 | |
| addedNames/removedNames | 341 | |
| filename | 268 | |
| skillCount | 238 | |
| addedBlocks | 195 | |
| met/condition | 177 | Stop condition evaluation |

### system

System-level events with `subtype` discriminator.

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| type | yes | `"system"` | |
| subtype | yes | string | See subtypes below |
| uuid | yes | string | |
| parentUuid | yes | string/null | |
| isSidechain | yes | bool | |
| cwd | yes | string | |
| entrypoint | yes | string | |
| gitBranch | yes | string | |
| version | yes | string | |
| sessionId | yes | string | |
| timestamp | yes | string | |
| userType | yes | string | |

**System subtypes:**

| Subtype | Count | Additional fields |
|---------|------:|-------------------|
| stop_hook_summary | 4,337 | hookCount, hookInfos, hookErrors, preventedContinuation, stopReason, hasOutput, toolUseID |
| turn_duration | 3,725 | durationMs, messageCount |
| away_summary | 260 | content |
| api_error | 172 | level, retryInMs, retryAttempt, maxRetries, error |
| bridge_status | 94 | url |
| local_command | 52 | content |
| compact_boundary | 18 | compactMetadata, logicalParentUuid, isVisibleInTranscriptOnly, isCompactSummary |
| scheduled_task_fire | 7 | |
| informational | 6 | level |

### Lightweight metadata types

#### last-prompt
| Field | Required | Type |
|-------|----------|------|
| type | yes | `"last-prompt"` |
| sessionId | yes | string |
| leafUuid | 60% | string |
| lastPrompt | 94% | string |

#### permission-mode
| Field | Required | Type |
|-------|----------|------|
| type | yes | `"permission-mode"` |
| sessionId | yes | string |
| permissionMode | yes | string |

Observed modes: `bypassPermissions` (7,605), `default` (50), `plan` (33), `acceptEdits` (21).

#### file-history-snapshot
| Field | Required | Type |
|-------|----------|------|
| type | yes | `"file-history-snapshot"` |
| messageId | yes | string |
| snapshot | yes | object |
| isSnapshotUpdate | yes | bool |

Note: Does NOT carry sessionId or the standard envelope fields.

#### ai-title
| Field | Required | Type |
|-------|----------|------|
| type | yes | `"ai-title"` |
| sessionId | yes | string |
| aiTitle | yes | string |

#### queue-operation
| Field | Required | Type |
|-------|----------|------|
| type | yes | `"queue-operation"` |
| sessionId | yes | string |
| operation | yes | string |
| timestamp | yes | string |
| content | 51% | string |

#### bridge-session
| Field | Required | Type | Notes |
|-------|----------|------|-------|
| type | yes | `"bridge-session"` | Since 2.1.142 |
| sessionId | yes | string | |
| bridgeSessionId | yes | string | `cse_...` format |
| lastSequenceNum | yes | int | |

#### pr-link
| Field | Required | Type |
|-------|----------|------|
| type | yes | `"pr-link"` |
| sessionId | yes | string |
| timestamp | yes | string |
| prNumber | yes | int |
| prUrl | yes | string |
| prRepository | yes | string |

#### worktree-state
| Field | Required | Type | Notes |
|-------|----------|------|-------|
| type | yes | `"worktree-state"` | Since 2.1.142 |
| sessionId | yes | string | |
| worktreeSession | yes | object/null | |

#### agent-name
| Field | Required | Type | Notes |
|-------|----------|------|-------|
| type | yes | `"agent-name"` | Since 2.1.142 |
| sessionId | yes | string | |
| agentName | yes | string | |

#### agent-color
| Field | Required | Type | Notes |
|-------|----------|------|-------|
| type | yes | `"agent-color"` | Rare (2 events) |
| sessionId | yes | string | |
| agentColor | yes | string | |

## Tool usage summary

Top tools across corpus (28,928 total tool_use blocks):

| Tool | Count | % |
|------|------:|---|
| Bash | 17,753 | 61.4% |
| Read | 3,454 | 11.9% |
| Edit | 3,389 | 11.7% |
| TaskUpdate | 1,242 | 4.3% |
| Write | 1,123 | 3.9% |
| TaskCreate | 658 | 2.3% |
| Grep | 301 | 1.0% |
| WebFetch | 173 | 0.6% |
| ToolSearch | 165 | 0.6% |
| AskUserQuestion | 136 | 0.5% |
| Agent | 57 | 0.2% |
| Skill | 39 | 0.1% |

## Adapter translation notes

For mu's exo-adapter (mu-p49v), the mapping strategy is:

1. **Envelope events** (user, assistant, attachment, system) → mu EventPayload variants.
   The uuid/parentUuid chain maps to mu's event log ordering. Content blocks
   map directly to mu's existing Anthropic message types.

2. **Metadata events** → mu session-level state updates. These don't need
   conversation-graph representation; they're projections.

3. **Unknown event types**: Log and continue. The corpus has been stable (no type
   removals observed), so new types in future versions are additive. Strict mode
   can opt into fail-closed on unknown types.

4. **Version keying**: Map CC version → adapter version. When a new CC version
   appears, auto-generate the mapping, diff against prior, dedup if identical.

## Corpus statistics

- Files: 300 (261 claude-personal, 25 claude-openrouter, 14 claude)
- Sessions: 242 unique
- Total events: 152,339
- Parse errors: 1
- Versions observed: 23 (2.1.86 through 2.1.150)
- Date range: 2026-04 through 2026-05
