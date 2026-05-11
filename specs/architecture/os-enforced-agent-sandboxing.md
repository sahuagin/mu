# Architecture: OS-enforced agent sandboxing with Capsicum, Casper, jails, and brokers

| field      | value                                      |
| ---------- | ------------------------------------------ |
| doc_id     | architecture/os-enforced-agent-sandboxing  |
| status     | design note / deferred implementation      |
| created    | 2026-05-11                                 |
| updated    | 2026-05-11                                 |
| authors    | tcovert + pi                               |
| related    | architecture/capability-delegation         |

## Summary

mu should eventually treat shell access and other high-authority tools as a
capability boundary, not merely a prompt/policy convention. Tool policy and
Biscuit-style delegation answer "what is this session authorized to ask for?";
FreeBSD mechanisms such as jails, Capsicum, Casper, and brokered services answer
"what is physically possible if the model, prompt, or tool policy fails?"

This is explicitly deferred until the core mu framework is more complete. The
purpose of this document is to preserve the target architecture and the design
constraints while the implementation surface is still moving.

## Motivation

Agent risk compounds with runtime, tool count, and multi-agent scale. Bash is
the largest universal escape hatch: once an agent can run an arbitrary shell
command, it may be able to reach every CLI, package script, credential, network
endpoint, interpreter, and local service visible to the process.

Prompt-only controls, system prompts, and command blacklists are useful but not
sufficient. Whitelists are better, but commands such as `python`, `node`,
`npm test`, `make`, or a seemingly harmless project script can reintroduce
arbitrary code execution. Production-adjacent agents need defense in depth.

The desired posture is:

1. minimize or remove bash from normal agent profiles;
2. expose narrow explicit tools instead;
3. enforce those tools through runtime policy and attenuable capabilities;
4. run agent-controlled code inside OS-enforced boundaries;
5. route exceptional authority through audited brokers and human approval.

## Key Capsicum model

Capsicum is runtime-configurable, not compile-time static. The important property
is that authority is one-way: after `cap_enter()`, the process and its descendants
cannot regain ambient authority.

The safe pattern is therefore:

```text
trusted supervisor/launcher starts with ambient authority
read static/signed config and session capability policy
open only the files, directories, sockets, and broker channels that are allowed
narrow those descriptors with cap_rights_limit()
enter capability mode with cap_enter()
start agent/model/tool-controlled behavior
```

This is not "start unrestricted, let the agent decide, then sandbox itself." The
pre-`cap_enter()` phase must be tiny, trusted, boring bootstrap code.

After entering capability mode, the process is effectively deny-all except for
the capabilities it already holds. It cannot perform global filesystem lookup or
invent new ambient authority. Additional authority must come from another process
that still holds it.

## Config vs compile time

Config can drive the sandbox, but only trusted launcher/supervisor code should
interpret that config before dropping authority.

Example policy intent:

```toml
[agent.workspace]
path = "/zfs/mu/workspaces/session-123"
rights = ["read", "write", "lookup"]

[agent.network]
mode = "none"

[agent.tools]
allow = ["read_file", "write_file", "jj_diff"]
deny = ["bash"]
```

The supervisor/launcher compiles this into concrete OS capabilities:

- a pre-opened workspace directory fd with limited rights;
- no network sockets or DNS service unless policy grants them;
- an RPC channel to the mu broker;
- selected tool endpoints only;
- jail/rctl/devfs constraints as the coarse outer boundary.

Capsicum itself does not know about "allow git status" or "deny rm". It knows
about descriptors and rights. mu policy must translate agent/tool semantics into
held descriptors, broker permissions, and service availability.

## Relationship to Biscuits / capability delegation

Biscuits remain useful as application-level attenuable capabilities:

```text
session may call read_file under workspace
session may call write_file under workspace/src
session may call jj_diff
session may not call network or bash
```

But a Biscuit alone does not constrain the operating system. The intended stack
is:

| Layer | Role |
| ----- | ---- |
| Biscuit / session capability | app-level authorization and delegation proof |
| mu tool policy | runtime dispatch decision and side-effect classification |
| broker services | audited authority delegation and human approval surface |
| Capsicum | process-level removal of ambient authority |
| jail / pot / container | coarse filesystem, process, hostname, network, and credential boundary |
| ZFS snapshot/clone | reversible workspace state and cheap cleanup |

A parent session may attenuate a child Biscuit, but cannot widen it. The OS
sandbox should also ensure the child process cannot exceed the environment that
the launcher constructed for it.

## Proposed mu architecture

```text
mu supervisor
  owns ambient authority
  owns config/policy
  owns TUI approval prompts
  owns audit log
  owns jail/ZFS setup

mu sandbox-launcher
  tiny trusted bootstrap binary
  interprets resolved policy
  opens allowed resources
  applies fd rights limits
  enters Capsicum capability mode
  starts the worker runtime

mu worker / agent runtime
  already constrained before model-controlled behavior begins
  has no default bash
  has only pre-opened resources and broker channel
  proposes tool calls; does not grant itself authority

mu broker services
  hold selected authority outside the sandbox
  verify Biscuit + policy + call arguments
  perform operations or return narrowed descriptors/tokens
  ask the human when policy says Ask / AskOnce
  append audit events
```

The dangerous anti-pattern is:

```text
launch arbitrary delegate permissive
let model/tool runtime read config
hope it calls cap_enter()
```

The safer pattern is:

```text
trusted launcher reads resolved policy
trusted launcher constrains process
agent-controlled runtime starts inside the box
```

## Human approval / controlled escalation

A sandboxed worker cannot escalate itself after `cap_enter()`. If a task needs
something outside the current capability set, it asks a broker:

```text
request: fetch https://github.com/owner/repo/issues/123
reason: inspect issue metadata for current task
session_biscuit: ...
```

The broker may:

1. deny immediately from static policy;
2. approve from static policy;
3. ask the human in the TUI;
4. perform the operation on behalf of the worker;
5. return a narrowed capability, such as a read-only fd, a connected socket, or
   a short-lived task-specific token.

The worker never becomes globally more privileged. It either receives a narrow
new capability or receives the broker's result.

Casper fits this pattern for existing FreeBSD delegated services such as DNS,
syslog, and similar resources. mu will likely also need custom brokers for
workspace operations, VCS operations, network fetches, package/test execution,
agent delegation, and production-adjacent operations.

## Fork/exec behavior

Capability mode is inherited across `fork()`. A descendant cannot leave
capability mode. Executing a new binary does not magically regain ambient
authority.

Important wrinkles:

- Executing by pathname generally requires namespace lookup, which capability
  mode is designed to prevent unless the relevant path is reachable through a
  pre-opened directory capability or equivalent controlled mechanism.
- Dynamically linked binaries may require loader/library access. Static helper
  binaries or carefully prepared runtime dependencies are simpler.
- Allowing interpreters or package runners inside the sandbox still permits
  arbitrary code execution inside that sandbox. That may be acceptable for a test
  jail, but should not imply access to production credentials, broad filesystem
  paths, or uncontrolled network.

## Tool execution model

Long term, mu should prefer service-like tools over local shell affordances.

Examples:

```text
read_file(path)
  broker checks path policy and Biscuit
  broker reads via workspace capability

write_file(path, content)
  broker checks write scope
  broker writes into versioned/snapshotted workspace

run_tests(profile)
  broker starts an approved command in a separate constrained test jail
  no arbitrary command string from the model

fetch_url(url)
  broker checks allowlist or asks human
  broker performs network operation and returns content
```

Bash, if present at all, should be a special high-risk profile. Production-
adjacent sessions should use explicit tools and constrained brokers instead.

## Phasing

This should come after mu has the core framework working. Suggested sequence:

1. **Policy metadata first**: tool side-effect classes, retry posture, Ask/Deny,
   and audit events.
2. **Biscuit/session capabilities**: delegation can attenuate tools, budgets,
   depth, and side-effect caps.
3. **No-bash profiles**: normal agent sessions use explicit tools; bash becomes
   opt-in or unavailable.
4. **Broker abstraction**: route privileged operations through a single audited
   service boundary.
5. **Jail/ZFS integration**: per-session or per-task workspace clone with coarse
   process/filesystem/network isolation.
6. **Capsicum launcher**: tiny trusted pre-cap bootstrap for worker processes and
   selected tool runners.
7. **Casper/custom services**: delegate DNS/syslog/etc. via Casper where useful;
   implement mu-specific brokers for workspace, VCS, network, test, and package
   operations.

## Open questions

- Which mu components should be separate processes versus in-process services?
- Should the worker itself be Capsicum-constrained, or should only tool runners
  be constrained at first?
- How should dynamic linking/runtime dependencies be packaged for cap-mode
  helpers?
- What is the minimal useful broker API for file and VCS operations?
- Which operations should return capabilities versus brokered results?
- How should TUI approval decisions be cached, expired, and represented in the
  event log?
- How do these controls compose with pot-based ephemeral agent jails?

## Design stance

Do not trust the model to honor policy. Do not trust arbitrary tools to sandbox
themselves. Let the model propose actions; let mu policy, brokers, and the OS
decide what can actually happen.
