#!/usr/bin/env bash
# End-to-end test for crab-auth running in Docker.
#
# Prerequisites:
#   docker compose up --build -d
#
# This script:
#   1. Gets a signed token from the mock IdP
#   2. Calls the crab-auth endpoint with various scenarios
#   3. Verifies expected responses (200, 403, 401)

set -euo pipefail

AUTH_URL="${CRAB_AUTH_URL:-http://localhost:8080}"
IDP_URL="${MOCK_IDP_URL:-http://localhost:9090}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

pass=0
fail=0

check() {
    local desc="$1"
    local expected_status="$2"
    local actual_status="$3"
    local body="$4"

    if [ "$actual_status" = "$expected_status" ]; then
        echo -e "${GREEN}✓${NC} $desc (HTTP $actual_status)"
        pass=$((pass + 1))
    else
        echo -e "${RED}✗${NC} $desc — expected $expected_status, got $actual_status"
        echo "  Response: $body"
        fail=$((fail + 1))
    fi
}

echo "=== Crab Auth E2E Tests ==="
echo "Auth endpoint: $AUTH_URL"
echo "Mock IdP:      $IDP_URL"
echo ""

# --- Health check ---
echo "--- Health Checks ---"
status=$(curl -s -o /dev/null -w "%{http_code}" "$AUTH_URL/health")
check "Auth health check" "200" "$status" ""

status=$(curl -s -o /dev/null -w "%{http_code}" "$IDP_URL/health")
check "IdP health check" "200" "$status" ""
echo ""

# --- Get tokens from mock IdP ---
echo "--- Issuing Tokens ---"
ALICE_TOKEN=$(curl -s "$IDP_URL/token?email=alice@corp.example.com&groups=platform-admins" | python3 -c "import sys,json; print(json.load(sys.stdin)['id_token'])")
echo "  Alice token: ${ALICE_TOKEN:0:20}..."

BOB_TOKEN=$(curl -s "$IDP_URL/token?email=bob@corp.example.com&groups=ml-team" | python3 -c "import sys,json; print(json.load(sys.stdin)['id_token'])")
echo "  Bob token:   ${BOB_TOKEN:0:20}..."

STRANGER_TOKEN=$(curl -s "$IDP_URL/token?email=stranger@external.com" | python3 -c "import sys,json; print(json.load(sys.stdin)['id_token'])")
echo "  Stranger token: ${STRANGER_TOKEN:0:20}..."

BANNED_TOKEN=$(curl -s "$IDP_URL/token?email=banned@corp.example.com&groups=platform-admins" | python3 -c "import sys,json; print(json.load(sys.stdin)['id_token'])")
echo "  Banned token: ${BANNED_TOKEN:0:20}..."
echo ""

# --- Test: /v1/credentials rejects push hard-cutoff ---
echo "--- Authorization Tests ---"
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/credentials" \
    -H "Content-Type: application/json" \
    -d "{\"id_token\":\"$ALICE_TOKEN\",\"repo_url\":\"crab://bucket/any/repo\",\"operation\":\"push\",\"client_version\":\"0.1.0\"}")
status=$(echo "$resp" | tail -1)
body=$(echo "$resp" | sed '$d')
check "Legacy push credentials are rejected" "400" "$status" "$body"

# --- Test: Alice can fetch from any repo ---
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/credentials" \
    -H "Content-Type: application/json" \
    -d "{\"id_token\":\"$ALICE_TOKEN\",\"repo_url\":\"crab://bucket/private/secret\",\"operation\":\"fetch\",\"client_version\":\"0.1.0\"}")
body=$(echo "$resp" | sed '$d')
status=$(echo "$resp" | tail -1)
check "Alice can fetch from private repo" "200" "$status" "$body"

# --- Test: Bob (ml-team) cannot use direct push credentials ---
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/credentials" \
    -H "Content-Type: application/json" \
    -d "{\"id_token\":\"$BOB_TOKEN\",\"repo_url\":\"crab://bucket/ml-models/gpt4\",\"operation\":\"push\",\"client_version\":\"0.1.0\"}")
body=$(echo "$resp" | sed '$d')
status=$(echo "$resp" | tail -1)
check "Bob cannot use direct push credentials" "400" "$status" "$body"

# --- Test: Bob (ml-team) can prepare push to ml-models ---
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/push/prepare" \
    -H "Content-Type: application/json" \
    -d "{\"id_token\":\"$BOB_TOKEN\",\"repo_url\":\"crab://bucket/ml-models/gpt4\",\"ref_updates\":[{\"ref_name\":\"refs/heads/main\",\"old_oid\":\"0000000000000000000000000000000000000000\",\"new_oid\":\"1111111111111111111111111111111111111111\"}],\"client_version\":\"0.1.0\"}")
body=$(echo "$resp" | sed '$d')
status=$(echo "$resp" | tail -1)
check "Bob can prepare push to ml-models" "200" "$status" "$body"

if echo "$body" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d['permissions']==['immutable-write']; assert d['upload_prefix'].startswith('ml-models/gpt4/staging/')" 2>/dev/null; then
    echo -e "  ${GREEN}✓${NC} Prepare response has immutable upload permissions"
    pass=$((pass + 1))
else
    echo -e "  ${RED}✗${NC} Prepare response structure invalid"
    fail=$((fail + 1))
fi

# --- Test: Bob cannot push to unrelated repo ---
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/credentials" \
    -H "Content-Type: application/json" \
    -d "{\"id_token\":\"$BOB_TOKEN\",\"repo_url\":\"crab://bucket/infrastructure/terraform\",\"operation\":\"push\",\"client_version\":\"0.1.0\"}")
body=$(echo "$resp" | sed '$d')
status=$(echo "$resp" | tail -1)
check "Bob cannot use direct push credentials for unrelated repo" "400" "$status" "$body"

# --- Test: Stranger can fetch from public repos ---
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/credentials" \
    -H "Content-Type: application/json" \
    -d "{\"id_token\":\"$STRANGER_TOKEN\",\"repo_url\":\"crab://bucket/public/datasets\",\"operation\":\"fetch\",\"client_version\":\"0.1.0\"}")
body=$(echo "$resp" | sed '$d')
status=$(echo "$resp" | tail -1)
check "Stranger can fetch from public repo" "200" "$status" "$body"

# --- Test: Stranger cannot push to public repos ---
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/credentials" \
    -H "Content-Type: application/json" \
    -d "{\"id_token\":\"$STRANGER_TOKEN\",\"repo_url\":\"crab://bucket/public/datasets\",\"operation\":\"push\",\"client_version\":\"0.1.0\"}")
body=$(echo "$resp" | sed '$d')
status=$(echo "$resp" | tail -1)
check "Stranger cannot use direct push credentials" "400" "$status" "$body"

# --- Test: Stranger cannot access private repos ---
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/credentials" \
    -H "Content-Type: application/json" \
    -d "{\"id_token\":\"$STRANGER_TOKEN\",\"repo_url\":\"crab://bucket/private/secret\",\"operation\":\"fetch\",\"client_version\":\"0.1.0\"}")
body=$(echo "$resp" | sed '$d')
status=$(echo "$resp" | tail -1)
check "Stranger cannot access private repo" "403" "$status" "$body"

# --- Test: Banned user is denied even with admin group ---
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/credentials" \
    -H "Content-Type: application/json" \
    -d "{\"id_token\":\"$BANNED_TOKEN\",\"repo_url\":\"crab://bucket/public/data\",\"operation\":\"fetch\",\"client_version\":\"0.1.0\"}")
body=$(echo "$resp" | sed '$d')
status=$(echo "$resp" | tail -1)
check "Banned user is denied" "403" "$status" "$body"

# --- Test: Invalid token is rejected ---
echo ""
echo "--- Security Tests ---"
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/credentials" \
    -H "Content-Type: application/json" \
    -d '{"id_token":"eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJmYWtlIn0.invalid","repo_url":"crab://bucket/repo","operation":"fetch","client_version":"0.1.0"}')
body=$(echo "$resp" | sed '$d')
status=$(echo "$resp" | tail -1)
check "Invalid token signature rejected" "401" "$status" "$body"

# --- Test: Malformed request ---
resp=$(curl -s -w "\n%{http_code}" -X POST "$AUTH_URL/v1/credentials" \
    -H "Content-Type: application/json" \
    -d '{"not_valid": true}')
body=$(echo "$resp" | sed '$d')
status=$(echo "$resp" | tail -1)
check "Malformed request rejected" "422" "$status" "$body"

# --- Summary ---
echo ""
echo "=== Results ==="
total=$((pass + fail))
echo -e "  ${GREEN}$pass passed${NC}, ${RED}$fail failed${NC} (out of $total)"

if [ "$fail" -gt 0 ]; then
    exit 1
fi
