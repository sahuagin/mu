# Agent context taxonomy and mu config shape

This note captures the practical taxonomy from the APM discussion and maps it
onto mu. The goal is not to win terminology arguments. The goal is to give us a
shared set of buckets for deciding where agent instructions, procedures,
programmatic hooks, packaged capabilities, and delegated workers belong.

APM may or may not become the tool we use. The taxonomy is useful either way.
If APM becomes useful, this document should make it straightforward to map mu's
configuration and process materials into an `apm.yml` package.

## Core distinction

Agent setup mixes several different things that are easy to blur together:

- always-on project guidance;
- reusable task procedures;
- deterministic lifecycle programs;
- tool/resource servers;
- installable capability bundles;
- separate delegated workers;
- full agent control loops.

Those are not the same primitive. They differ along five axes:

- **when they load**: always, on demand, or at an event boundary;
- **who executes them**: the current agent, another agent, or the runtime;
- **how binding they are**: suggestion, procedure, policy, or code;
- **how they are distributed**: local file, package, plugin, or manifest;
- **what failure means**: ignored guidance, bad procedure, policy violation, or
  failed program.

Keeping those axes visible helps avoid stuffing everything into one giant
`CLAUDE.md`, and it helps avoid pretending that a prompt reminder gives the
same guarantees as a hook or policy gate.

## Taxonomy

### AGENTS.md

`AGENTS.md` is shared ambient guidance for agent runtimes.

It is the repo/project handbook: the material a competent new worker should
read before touching the code. It should cover local conventions, build and test
commands, project-specific vocabulary, operating constraints, and sharp edges.

Examples:

- use `uv run ...`, not bare `python`, in this repository;
- compatibility is not required yet, because the product only has internal
  test users;
- `prod` means the read-only log MCP, not an ssh session;
- use `jj`, not raw `git`, in this repo;
- never run unattended workers in a shared host working copy.

`AGENTS.md` should be cross-runtime. Claude Code, Codex, Cursor, Gemini,
OpenCode, Copilot, and future harnesses should all be able to read the same
base instructions.

### CLAUDE.md

`CLAUDE.md` is Claude Code's native ambient guidance file.

Long-term, this should usually be a thin Claude-specific wrapper around
`AGENTS.md`, not a second source of truth. A practical pattern is:

```md
@AGENTS.md
```

Then `AGENTS.md` remains the shared base, while `CLAUDE.md` exists only because
Claude Code's native loader expects it. Any truly Claude-specific deltas can
live there, but general repo process belongs in `AGENTS.md`.

### Skill

A skill is reusable procedural context loaded into the current agent.

It is a saved playbook: markdown that says how to do a kind of work. It is
stronger than vibe, weaker than code. It helps the current agent perform a task
without forcing the operator to paste the same procedure every time.

Examples:

- design-review taste and design-system paths;
- weekly metrics report procedure;
- production incident triage checklist;
- how to run a specific internal pipeline;
- how to inspect a particular kind of log or event stream.

A useful distinction:

```text
AGENTS.md = what the agent should know before it knows the task
Skill     = what the agent should load when it recognizes the task
```

Skills can be explicitly invoked, or selected by the harness when relevant.
They still execute inside the current agent's context window and authority.

### Hook

A hook is deterministic code run by the runtime at a lifecycle boundary.

Hooks are for behavior that should not depend on the model remembering,
inferring, or complying. If forgetting the behavior is expensive, move it from a
prompt reminder to a hook.

Examples:

- inject memory context at session start;
- notify the operator when a long task completes;
- run a formatter after file edits;
- block or audit dangerous commands before tool execution;
- scan installed context for hidden Unicode;
- record transcript metadata after a session.

A skill can say "remember to notify me." A hook makes notification a runtime
property.

### MCP server

An MCP server is a tool/resource/prompt provider exposed over a standard
protocol.

It is not itself a skill or a plugin, though a plugin may install one and a
skill may explain how to use one. MCP is the interface by which an agent gains
access to external capabilities: tools, resources, prompts, and subscriptions.

The important security distinction is that installing MCP configuration can add
real authority. A transitive package that adds an MCP server is closer to adding
an executable dependency than adding documentation.

### Plugin or package

A plugin/package is a distributable capability bundle.

It may contain skills, prompts, MCP config, hooks, binaries, scripts, agent
files, target-specific metadata, or any subset of those. The key property is
packaging: "install this capability" rather than "copy this one markdown file."

Examples:

- a browser automation package containing a Playwright MCP server plus usage
  skills;
- an org review package containing review skills, prompts, and policy hooks;
- a deployment package containing runbooks, MCP server config, and CLI scripts.

For APM, this is the natural unit of dependency management: declare it in a
manifest, resolve transitive dependencies, lock hashes, and deploy the right
files for each agent harness.

### Subagent

A subagent is delegated execution in a separate context.

It is not a skill. A skill changes how the current agent works. A subagent is
another worker: it receives a task, runs with its own context/model/tools, and
returns a result.

Useful properties:

- separate context window;
- possibly different model;
- narrowed tool authority;
- parallel execution;
- bounded task contract;
- summarized result returned to the parent;
- parent does not need to ingest every intermediate detail.

Subagents may be launched with skills, packages, hooks, and MCP servers already
installed. Those are their environment. The subagent is the actor.

### Agent

An agent is an LLM-containing control loop with tools and an objective.

The useful discriminator is the action loop:

1. observe state;
2. choose a next action;
3. call tools or mutate the world;
4. observe the result;
5. continue until done, blocked, or stopped.

Claude Code is an agent. A mailbox responder is an agent. A plain chat
completion is not, unless wrapped in a loop that lets it act.

## Summary table

| Primitive | Loaded when | Runs where | Binding strength | Typical content |
| --- | --- | --- | --- | --- |
| `AGENTS.md` | Session/repo start | Current agent | Ambient guidance | Repo process, conventions, hazards |
| `CLAUDE.md` | Claude session start | Claude Code | Ambient guidance / wrapper | Claude-specific overlay, `@AGENTS.md` |
| Skill | On demand | Current agent | Procedure | Task playbooks, reusable prompts |
| Hook | Lifecycle event | Runtime process | Deterministic code | Notifications, gates, scanners, injectors |
| MCP server | Tool/resource use | External server | Capability boundary | Tools, resources, prompts, subscriptions |
| Plugin/package | Install time | Deployed to targets | Dependency | Bundles of skills, MCP, hooks, scripts |
| Subagent | Delegation time | Separate agent context | Delegated execution | Bounded task with returned result |
| Agent | Runtime | Control loop | Actor | Observe/act/continue loop |

## Proposed mu config shape

mu should keep the same taxonomy visible in its config directory. A strawman
layout:

```text
$XDG_CONFIG_HOME/mu/
  mu.toml                 # global mu defaults shared by frontends
  solo.toml               # mu-solo local UI/session defaults

  agents/
    AGENTS.md             # user-level shared ambient guidance
    CLAUDE.md             # optional Claude wrapper or overlay

  skills/
    <skill-name>/
      SKILL.md
      assets/

  hooks/
    session-start.d/
    pre-tool.d/
    post-tool.d/
    session-done.d/

  mcp/
    servers.toml          # local MCP declarations and trust choices

  packages/
    apm.yml               # optional: exported/managed context manifest
    apm.lock.yaml         # optional: resolved package lockfile

  subagents/
    profiles.toml         # named worker profiles: model, tools, budget, cwd

  policy/
    policy.toml           # source allowlists, hook policy, package policy
```

This does not need to land all at once. The value is the separation of
concerns:

- ambient guidance is not mixed with task procedures;
- task procedures are not confused with deterministic hooks;
- capability servers are declared separately from instructions about how to use
  them;
- packages can install material into the right buckets;
- subagent profiles describe workers, not prompts;
- policy can reason about all of the above.

## APM / APU question

The repository we looked at is APM, the Agent Package Manager. If `apu` was a
name slip, read this section as APM. If `apu` becomes a separate tool later, the
same shape should apply.

APM's useful promise is not merely "install prompts." The useful promise is:

> an agent environment should be reproducible from a manifest, not reconstructed
> from tribal shell history.

For mu, that could mean a fresh worker can:

1. clone the repo;
2. run the package manager;
3. receive the repo's shared guidance, skills, hooks, MCP config, and policies;
4. start work in a known environment.

That is especially relevant for isolated pots and clean jj workspaces. The
workspace clone gives filesystem isolation; the context manifest gives process
isolation and reproducibility.

The caution is that context dependencies are executable in effect. A malicious
skill or prompt can steer a future agent with real tool access. A package that
adds an MCP server or hook is even more obviously executable. Any package path
we adopt should therefore require:

- lockfile hashes;
- source provenance;
- transitive dependency visibility;
- explicit consent for authority-adding MCP servers and hooks;
- org/repo policy gates;
- drift detection between generated files and manifest state.

APM appears to understand this direction: manifest, lockfile, integrity hashes,
policy, hidden-Unicode scanning, drift detection, and trust prompts for
transitive MCP servers are all in its pitch. We should still treat it as a tool
to evaluate, not a dependency to assume.

## How this should guide mu work

Near-term:

1. Keep repo-level process in `AGENTS.md`.
2. Use `CLAUDE.md` only as a Claude-specific wrapper/overlay when possible.
3. Put repeated procedures into skills instead of expanding ambient context.
4. Put non-negotiable lifecycle behavior into hooks, not reminders.
5. Treat MCP config and package installs as authority changes.
6. Model subagents as separate workers with explicit profiles and bounded
   authority.

Future mu features can map directly onto this taxonomy:

- context loading: `AGENTS.md`, `CLAUDE.md`, path-scoped overlays;
- skill discovery and invocation: `skills/`;
- hook engine: `hooks/<event>.d/`;
- MCP registry/trust: `mcp/`;
- package import/export: `packages/` and possibly APM integration;
- worker delegation: `subagents/profiles.toml` plus pot/jj workspace isolation;
- policy gates: `policy/`.

The main design rule: choose the weakest primitive that gives the needed
semantics, but no weaker. If a reminder is enough, use guidance. If a repeatable
procedure is enough, use a skill. If forgetting is costly, use a hook. If a
capability changes authority, require package/policy treatment. If work should
not pollute the parent context, delegate to a subagent.
