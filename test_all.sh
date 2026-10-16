#!/bin/sh
# integration test: core(HTTP) + gateway(hook/api) + MCP stdio adapter, all against mock OpenAI.
# fresh-review: the old script only printed output — nothing asserted anything, so wrong
# status codes or corrupt JSON would still print "green". Every probe now asserts.
set -u
cd "$(dirname "$0")"
FAILED=0
ok() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1 ($2)"; FAILED=$((FAILED+1)); }
assert_contain() { # label haystack needle
  case "$2" in *"$3"*) ok "$1";; *) fail "$1" "missing '$3' in: $2";; esac
}

python3 mock_openai.py >/dev/null 2>&1 &
MOCK=$!
sleep 1
NANOBOT_SERVE=8090 NANOBOT_API_KEY=x NANOBOT_API_BASE=http://127.0.0.1:18891 NANOBOT_WORKSPACE="$PWD" ./target2/release/nanobot-rs > /tmp/core.log 2>&1 &
CORE=$!
sleep 1
(cd gateway && exec ./gateway-bin > /tmp/gw.log 2>&1) &
GW=$!
sleep 1
cleanup() { kill $CORE $GW $MOCK 2>/dev/null; }
trap cleanup INT TERM
trap 'cleanup; exit 143' TERM

OUT=$(curl -s http://localhost:8090/health)
assert_contain "core /health ok" "$OUT" '{"ok":true}'

OUT=$(curl -s -XPOST localhost:8081/api/health)
assert_contain "gateway /api/health proxies core" "$OUT" '{"ok":true}'

OUT=$(curl -s -XPOST localhost:8081/hook -H 'Content-Type: application/json' -d '{"text":"ping","session_id":"hook-1"}')
assert_contain "gateway /hook runs agent" "$OUT" 'tool said'

OUT=$(curl -s -XPOST localhost:8081/api/run -H 'Content-Type: application/json' -d '{"prompt":"ping"}')
assert_contain "gateway /api/run (WebUI path)" "$OUT" 'tool said'

OUT=$(curl -s -XPOST localhost:8081/api/run -d '{"prompt":"ping"}')
CODE=$(curl -s -o /dev/null -w '%{http_code}' -XPOST localhost:8081/api/run -d '{"prompt":"ping"}')
[ "$CODE" = "415" ] && ok "/api/run rejects non-JSON Content-Type (415)" || fail "/api/run non-JSON gate" "got $CODE"

CODE=$(curl -s -o /dev/null -w '%{http_code}' -XPOST localhost:8081/hook -H 'Content-Type: text/plain' -d '{}')
[ "$CODE" = "415" ] && ok "/hook rejects non-JSON (415)" || fail "/hook non-JSON" "got $CODE"

CODE=$(curl -s -o /dev/null -w '%{http_code}' http://localhost:8081/api/healthFuzz)
[ "$CODE" = "404" ] && ok "core rejects fuzzed routes" || fail "route fuzz" "core gave $CODE for /api/healthFuzz"
CODE=$(curl -s -o /dev/null -w '%{http_code}' http://localhost:8090/healthFuzz)
[ "$CODE" = "404" ] && ok "core rejects fuzzed /healthFuzz" || fail "route fuzz core" "got $CODE"

OUT=$(curl -s -o /dev/null -w '%{http_code}' -XPOST localhost:8081/api/run -H 'Content-Type: application/json' -d 'not-json')
[ "$OUT" = "400" ] && ok "gateway 400 on bad JSON" || fail "gateway bad JSON" "got $OUT"

# MCP over stdio
printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n{"jsonrpc":"2.0","method":"notifications/initialized"}\n{"jsonrpc":"2.0","id":2,"method":"tools/list"}\n{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"exec","arguments":{"command":"echo mcp-ok"}}}\n{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}\n{"jsonrpc":"2.0","id":5,"method":"what"}\n{"not json"}\n' | node mcp/nanobot-mcp.js > /tmp/mcp.out 2>&1
MCP_FULL=$(cat /tmp/mcp.out)
assert_contain "MCP initialize" "$MCP_FULL" '"id":1'
assert_contain "MCP tools/call" "$MCP_FULL" 'mcp-ok'
assert_contain "MCP unknown tool isError" "$MCP_FULL" '"isError":true'
case "$MCP_FULL" in *32601*) ok "MCP -32601 present";; *) fail "MCP -32601" "not in output";; esac
case "$MCP_FULL" in *32700*) ok "MCP parse error -32700 on garbage line";; *) fail "MCP -32700" "not in output";; esac

cleanup
if [ "$FAILED" = "0" ]; then echo "ALL CHECKS PASSED"; else echo "$FAILED CHECK(S) FAILED"; exit 1; fi
echo ALL-DONE
