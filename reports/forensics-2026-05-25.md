# Claude-Code Session Forensics Report
## Corpus: 98 sessions

## Tool Reliability
- Total tool calls: 25,082
- Total errors: 823 (3.3%)

### Errors by tool:
  Bash: 557
  Edit: 124
  Write: 50
  Read: 49
  AskUserQuestion: 22
  WebFetch: 8
  ExitPlanMode: 3
  TaskUpdate: 3
  TaskOutput: 3
  Monitor: 2
  RemoteTrigger: 1
  Skill: 1

### Top error messages:
  **Edit**:
    - <tool_use_error>File has not been read yet. Read it first before writing to it.< (x73)
    - <tool_use_error>String to replace not found in file. (x18)
    - <tool_use_error>File has been modified since read, either by the user or by a li (x12)
  **Write**:
    - EACCES: permission denied, mkdir '/tmp' (x28)
    - <tool_use_error>File has not been read yet. Read it first before writing to it.< (x21)
    - EACCES: permission denied, open '/tmp/claude-write-probe-2026-05-18.txt' (x1)
  **Bash**:
    - Exit code 1 (x315)
    - Exit code 2 (x80)
    - Exit code 4 (x12)
  **Read**:
    - This PDF has 19 pages, which is too many to read at once. Use the pages paramete (x5)
    - File does not exist. Note: your current working directory is /home/tcovert. (x5)
    - File does not exist. Note: your current working directory is /home/tcovert/src/p (x4)

## Retry Loops
- Sessions with retry loops: 55
- Total retry loops: 421

## Context Growth
- Sessions with usage data: 85
- Median peak: 394,900 tokens
- Max peak: 995,783 tokens
- Median growth rate: 3,412 tokens/turn
- Total compactions detected: 17
