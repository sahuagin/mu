#!/usr/local/bin/zsh
# Test mu-slat: spawn claude-code with MCP mailbox, give it a task,
# verify it posts results back.
#
# This uses -p (headless, credit pool) for testing. Production would
# use pty-bridge for subscription billing.

SOCK="${MU_MCP_SOCKET:-$HOME/.local/share/mu/mcp.sock}"
MCP_CONFIG="$HOME/.local/share/mu/mcp-config.json"

# Ensure mu serve is running
if ! echo '{"jsonrpc":"2.0","id":0,"method":"ping","params":{}}' | socat -T1 - UNIX-CONNECT:"$SOCK" >/dev/null 2>&1; then
    echo "ERROR: mu serve not running (no socket at $SOCK)" >&2
    echo "Start it with: ~/.local/bin/mu-mcp-bridge (or manually)" >&2
    exit 1
fi

echo "=== mu serve is running ==="

# Get the daemon_id by peeking at session-1's event log via mailbox_list
# (any successful call proves connectivity; we need daemon_id for posting)
# We can't get daemon_id via MCP tools — use the peer.hello response which
# doesn't need it. But mailbox.post DOES need the real daemon_id.
# Workaround: peer.hello accepts any from_daemon_id, and the handle it
# returns is bound to from_session_id only. The daemon_id check is on
# mailbox.post. We need to discover the real daemon_id.
#
# Since we can't call daemon.stats via MCP, read it from the serve log.
# Discover daemon_id via the mu_daemon_info MCP tool.
DAEMON_ID=$(echo '{"jsonrpc":"2.0","id":0,"method":"tools/call","params":{"name":"mu_daemon_info","arguments":{}}}' \
    | socat -T2 - UNIX-CONNECT:"$SOCK" 2>/dev/null \
    | python3 -c "import sys,json; r=json.loads(json.loads(sys.stdin.read())['result']['content'][0]['text']); print(r['daemon_id'])" 2>/dev/null)
if [[ -z "$DAEMON_ID" ]]; then
    echo "ERROR: could not discover daemon_id via mu_daemon_info" >&2
    exit 1
fi
echo "  daemon_id: $DAEMON_ID"

# Post a task to session-1's mailbox for claude-code to find
echo "=== Posting task to session-1 mailbox ==="

# Get a handle
HANDLE_RESP=$(echo "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"mu_peer_hello\",\"arguments\":{\"to_session_id\":\"session-1\",\"from_daemon_id\":\"$DAEMON_ID\",\"from_session_id\":\"test-script\"}}}" \
    | socat -T2 - UNIX-CONNECT:"$SOCK" 2>/dev/null)

HANDLE=$(echo "$HANDLE_RESP" | python3 -c "import sys,json; r=json.loads(json.loads(sys.stdin.read())['result']['content'][0]['text']); print(r.get('peer_handle','FAILED'))" 2>/dev/null)

echo "  peer_handle: $HANDLE"

if [[ "$HANDLE" == "FAILED" || -z "$HANDLE" ]]; then
    echo "ERROR: peer.hello failed" >&2
    echo "  response: $HANDLE_RESP" >&2
    exit 1
fi

# Post the task
POST_RESP=$(echo "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"mu_mailbox_post\",\"arguments\":{\"to_session_id\":\"session-1\",\"peer_handle\":\"$HANDLE\",\"from_daemon_id\":\"$DAEMON_ID\",\"from_session_id\":\"test-script\",\"kind\":\"task\",\"subject\":\"test task from orchestrator\",\"body\":{\"instruction\":\"List the files in the current directory and post the result back to session-1's mailbox using mu_mailbox_post. Use from_session_id 'claude-worker' and from_daemon_id '$DAEMON_ID'.\"}}}}" \
    | socat -T2 - UNIX-CONNECT:"$SOCK" 2>/dev/null)

echo "  post response: $POST_RESP"

# Verify the message landed
echo "=== Verifying task is in mailbox ==="
echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"mu_mailbox_list","arguments":{"session_id":"session-1"}}}' \
    | socat -T2 - UNIX-CONNECT:"$SOCK" 2>/dev/null \
    | python3 -c "
import sys, json
resp = json.loads(sys.stdin.read())
msgs = json.loads(resp['result']['content'][0]['text'])['messages']
if not msgs:
    print('  (empty — post may have failed)')
for m in msgs:
    print(f'  seq={m[\"seq\"]} kind={m[\"kind\"]} subject={m[\"subject\"]}')
"

# Now spawn claude-code with MCP config
echo "=== Spawning claude-code with MCP config ==="
echo "(using -p mode for testing — credit pool billing)"

claude -p \
    --bare \
    --dangerously-skip-permissions \
    --mcp-config "$MCP_CONFIG" \
    --append-system-prompt "You have access to mu-mailbox MCP tools for coordinating with the mu orchestrator.

Your workflow:
1. Call mu_mailbox_list with session_id 'session-1' to check for tasks.
2. Read any messages with mu_mailbox_read using the seq number.
3. Execute the task described in the message body.
4. Get a peer handle: call mu_peer_hello with to_session_id 'session-1', from_daemon_id '$DAEMON_ID', from_session_id 'claude-worker'.
5. Post your result: call mu_mailbox_post with the handle, kind 'task_result', and the result in the body.
6. Consume the original task: call mu_mailbox_consume with the task's seq.

Always use from_daemon_id '$DAEMON_ID' and from_session_id 'claude-worker' for your identity." \
    "Check session-1's mailbox for a task, execute it, and post the result back."

echo ""
echo "=== Claude-code finished. Checking mailbox for results ==="

# Check mailbox
echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"mu_mailbox_list","arguments":{"session_id":"session-1","include_consumed":true}}}' \
    | socat -T2 - UNIX-CONNECT:"$SOCK" 2>/dev/null \
    | python3 -c "
import sys, json
resp = json.loads(sys.stdin.read())
msgs = json.loads(resp['result']['content'][0]['text'])['messages']
if not msgs:
    print('  (no messages)')
for m in msgs:
    print(f'  seq={m[\"seq\"]} from={m[\"from_session_id\"]} kind={m[\"kind\"]} subject={m[\"subject\"]} consumed={m[\"consumed\"]}')
"
