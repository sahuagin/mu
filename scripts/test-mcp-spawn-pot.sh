#!/usr/local/bin/zsh
# Test mu-slat in pot: spawn claude-code inside a pot with MCP mailbox
# connected via nullfs-mounted unix socket.
#
# Prerequisites:
#   - mu serve running (socket at ~/.local/share/mu/mcp.sock)
#   - pot 'mu-slat-test' running with:
#     - ~/.local/share/mu/ mounted at /usr/home/tcovert/.local/share/mu/
#     - socat installed
#     - claude 2.1.150 available
#
# Uses -p (headless, credit pool) for testing.

SOCK="${MU_MCP_SOCKET:-$HOME/.local/share/mu/mcp.sock}"
POT_NAME="${MU_SLAT_POT:-mu-slat-test}"
POT_SOCK="/usr/home/tcovert/.local/share/mu/mcp.sock"
POT_USER="tcovert"

# Ensure mu serve is running
if ! echo '{"jsonrpc":"2.0","id":0,"method":"ping","params":{}}' | socat -T1 - UNIX-CONNECT:"$SOCK" >/dev/null 2>&1; then
    echo "ERROR: mu serve not running (no socket at $SOCK)" >&2
    exit 1
fi
echo "=== mu serve is running ==="

# Get daemon_id
DAEMON_ID=$(echo '{"jsonrpc":"2.0","id":0,"method":"tools/call","params":{"name":"mu_daemon_info","arguments":{}}}' \
    | socat -T2 - UNIX-CONNECT:"$SOCK" 2>/dev/null \
    | python3 -c "import sys,json; r=json.loads(json.loads(sys.stdin.read())['result']['content'][0]['text']); print(r['daemon_id'])" 2>/dev/null)
echo "  daemon_id: $DAEMON_ID"

# Post a task
echo "=== Posting task to session-1 mailbox ==="
HANDLE=$(echo "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"mu_peer_hello\",\"arguments\":{\"to_session_id\":\"session-1\",\"from_daemon_id\":\"$DAEMON_ID\",\"from_session_id\":\"test-script\"}}}" \
    | socat -T2 - UNIX-CONNECT:"$SOCK" 2>/dev/null \
    | python3 -c "import sys,json; r=json.loads(json.loads(sys.stdin.read())['result']['content'][0]['text']); print(r.get('peer_handle','FAILED'))" 2>/dev/null)
echo "  peer_handle: $HANDLE"

POST_RESP=$(echo "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"mu_mailbox_post\",\"arguments\":{\"to_session_id\":\"session-1\",\"peer_handle\":\"$HANDLE\",\"from_daemon_id\":\"$DAEMON_ID\",\"from_session_id\":\"test-script\",\"kind\":\"task\",\"subject\":\"test task from orchestrator (pot)\",\"body\":{\"instruction\":\"Run uname -a and post the output back to session-1's mailbox using mu_mailbox_post. Use from_session_id 'claude-worker' and from_daemon_id '$DAEMON_ID'.\"}}}}" \
    | socat -T2 - UNIX-CONNECT:"$SOCK" 2>/dev/null)
echo "  post response: $POST_RESP"

# Verify
echo "=== Verifying task is in mailbox ==="
echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"mu_mailbox_list","arguments":{"session_id":"session-1"}}}' \
    | socat -T2 - UNIX-CONNECT:"$SOCK" 2>/dev/null \
    | python3 -c "
import sys, json
resp = json.loads(sys.stdin.read())
msgs = json.loads(resp['result']['content'][0]['text'])['messages']
for m in msgs:
    print(f'  seq={m[\"seq\"]} kind={m[\"kind\"]} subject={m[\"subject\"]}')
"

# Write MCP config to /tmp (shared with pot via /compat/linux/tmp)
MCP_CONFIG_HOST="/tmp/mu-mcp-pot.json"
MCP_CONFIG_IN_POT="/compat/linux/tmp/mu-mcp-pot.json"
cat > "$MCP_CONFIG_HOST" << MCPEOF
{
  "mcpServers": {
    "mu-mailbox": {
      "command": "/usr/local/bin/socat",
      "args": ["STDIO", "UNIX-CONNECT:$POT_SOCK"]
    }
  }
}
MCPEOF

# Read the OAuth token — keep base64-encoded for safe shell transport
TOKEN_B64=$(base64 < "$HOME/.config/claude-code/headless-oauth-token.disabled" | tr -d '\n')

echo "=== Spawning claude-code INSIDE POT $POT_NAME ==="
echo "(jexec + -p mode for testing — credit pool billing)"

sudo jexec -U "$POT_USER" "$POT_NAME" /bin/sh -c "
    unset ANTHROPIC_API_KEY
    unset ANTHROPIC_BASE_URL
    export HOME=/usr/home/tcovert
    export LANG=C.UTF-8
    TOKEN_DECODED=\$(echo '$TOKEN_B64' | base64 -d)
    export CLAUDE_CODE_OAUTH_TOKEN=\"\$TOKEN_DECODED\"
    /usr/local/bin/claude -p \
        --dangerously-skip-permissions \
        --mcp-config /compat/linux/tmp/mu-mcp-pot.json \
        --append-system-prompt 'You have access to mu-mailbox MCP tools for coordinating with the mu orchestrator.

Your workflow:
1. Call mu_mailbox_list with session_id session-1 to check for tasks.
2. Read any messages with mu_mailbox_read using the seq number.
3. Execute the task described in the message body.
4. Get a peer handle: call mu_peer_hello with to_session_id session-1, from_daemon_id $DAEMON_ID, from_session_id claude-worker.
5. Post your result: call mu_mailbox_post with the handle, kind task_result, and the result in the body.
6. Consume the original task: call mu_mailbox_consume with the tasks seq.

Always use from_daemon_id $DAEMON_ID and from_session_id claude-worker for your identity.' \
        'Check session-1 mailbox for a task, execute it, and post the result back.'
" 2>&1

echo ""
echo "=== Claude-code finished. Checking mailbox for results ==="

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
