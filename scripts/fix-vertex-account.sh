#!/usr/bin/env bash
# fix-vertex-account.sh — 修 SciSpace Vertex 账号的出网配置（链式：Clash → Vault）
#
# ── 背景（2026-10-18 实测）──────────────────────────────────────
# 报错 `401 Request is missing required authentication credential` 时，请求其实
# 已经发到 Google，只是**没经过注入代理**。两种成因都会长成这个样子：
#   1. proxy 指向代理池的 Clash 节点（source=pool）→ 从节点 IP 直连 Google；
#   2. 面板新建时填的代理被旧版本后端丢弃 → 账号根本没有 proxy。
# 正确形态是**链式**：
#   frontProxy = Clash 本地监听器（选地区）
#   proxy      = Agent Vault 的 14322（注入 x-goog-api-key）
# 少任何一跳都不成立：只有 proxy 会从本机 IP 出去；只有 frontProxy 则拿不到注入。
#
# ── 用法 ──────────────────────────────────────────────────────
#   export AGENT_VAULT_TOKEN=av_agt_...
#   ./fix-vertex-account.sh <面板用户名> <面板密码>
#
# 可选环境变量：
#   AGENT_VAULT_VAULT   vault 名（默认 prod-sandbox）
#   AGENT_VAULT_HOST    vault NLB 主机（必填，不内置环境地址）
#   FRONT_PORT          指定 Clash 监听器端口；不填则自动探测
#   AGENT2API_BASE      面板地址（默认 http://127.0.0.1:3065）
#
# 幂等：重复执行只会写回同样值。

set -euo pipefail
BASE="${AGENT2API_BASE:-http://127.0.0.1:3065}"
USER_NAME="${1:?用法: $0 <面板用户名> <面板密码>}"
PASS="${2:?用法: $0 <面板用户名> <面板密码>}"

export VAULT_HOST="${AGENT_VAULT_HOST:?请先设置 AGENT_VAULT_HOST}"
VAULT_PORT="14322"
VAULT_USER="${AGENT_VAULT_TOKEN:?请先 export AGENT_VAULT_TOKEN=av_agt_...}"
VAULT_PASS="${AGENT_VAULT_VAULT:-prod-sandbox}"
FRONT_PORT="${FRONT_PORT:-}"

CK=$(mktemp)
trap 'rm -f "$CK"' EXIT

echo "==> 登录面板"
curl -sS -m 20 -X POST "$BASE/api/panel/login" \
  -H 'Content-Type: application/json' -c "$CK" \
  -d "{\"username\":\"$USER_NAME\",\"password\":\"$PASS\"}" \
  | head -c 160; echo

echo "==> 找 vertex 提供商"
PROVIDER_ID=$(curl -sS -m 20 -b "$CK" "$BASE/api/custom-providers" | python -c '
import sys, json
d = json.load(sys.stdin)
for it in (d.get("data") or {}).get("providers") or []:
    if it.get("protocol") == "vertex":
        print(it["id"]); break
')
if [ -z "$PROVIDER_ID" ]; then
  echo "未找到 protocol=vertex 的提供商；请先在面板创建（协议选 Google Vertex - Gemini）" >&2
  exit 1
fi
echo "    $PROVIDER_ID"

echo "==> 开启 insecureTls（上游经 MITM 代理，其证书不被系统信任）"
curl -sS -m 20 -b "$CK" -X POST "$BASE/api/custom-providers/update" \
  -H 'Content-Type: application/json' \
  -d "{\"id\":\"$PROVIDER_ID\",\"insecureTls\":true}" \
  | python -c 'import sys,json; d=json.load(sys.stdin); print("    insecureTls =", (d.get("data") or {}).get("provider",{}).get("insecureTls"))'

echo "==> 列出该提供商的账号"
ACCOUNTS_JSON=$(curl -sS -m 20 -b "$CK" "$BASE/api/accounts")
ACCOUNT_IDS=$(echo "$ACCOUNTS_JSON" | PROVIDER="$PROVIDER_ID" python -c '
import sys, json, os
d = json.load(sys.stdin)
pid = os.environ["PROVIDER"]
print(" ".join(a["id"] for a in (d.get("data") or {}).get("accounts") or []
              if a.get("provider") == pid))
')
if [ -z "$ACCOUNT_IDS" ]; then
  echo "该提供商下没有账号；请先在面板添加" >&2
  exit 1
fi
echo "    $ACCOUNT_IDS"

# ── 选前置跳（Clash 本地监听器）────────────────────────────────
#
# 判据只有一条：**能连通 vault**。节点本身可用不代表能出网到 NLB
# （实测有节点回 502），所以逐个探，取第一个 200。
if [ -z "$FRONT_PORT" ]; then
  echo "==> 探测可用的 Clash 监听器（要求能连通 vault）"
  FRONT_PORT=$(python - "$BASE" <<'PY'
import json, socket, sys, urllib.request

base = sys.argv[1]

# 1) 从代理池读候选（只读池的持久化形态，不需要池 API）
try:
    req = urllib.request.Request(f"{base}/api/proxies/pool")
    # 面板端点需要 cookie；这里改走本地端口探测，池读不到就退回端口扫描
    pool = None
except Exception:
    pool = None

# 2) 端口扫描：池条目常落在 191xx 段（Clash 为每条订阅建一个监听器）
candidates = list(range(19100, 19120)) + list(range(19000, 19010))
import os
vault_host = os.environ["VAULT_HOST"]

for port in candidates:
    # 先看端口是否有人监听
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.6):
            pass
    except OSError:
        continue
    # 再看它能不能到 vault:14321（/v1/status 免认证）
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=3)
        target = f"{vault_host}:14321"
        s.sendall(f"CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n".encode())
        resp = b""
        while b"\r\n\r\n" not in resp:
            chunk = s.recv(1024)
            if not chunk:
                break
            resp += chunk
        s.close()
        if b" 200" in resp.split(b"\r\n", 1)[0]:
            print(port)
            sys.exit(0)
    except OSError:
        continue

sys.exit(1)
PY
)
  if [ -z "$FRONT_PORT" ]; then
    echo "    ⚠️ 没有可用的 Clash 监听器；将以单跳（仅 vault）写入" >&2
  else
    echo "    选中端口 $FRONT_PORT"
  fi
fi

echo "==> 写入链式出口"
for AID in $ACCOUNT_IDS; do
  BODY=$(python - "$AID" "$VAULT_HOST" "$VAULT_PORT" "$VAULT_USER" "$VAULT_PASS" "$FRONT_PORT" <<'PY'
import json, sys
aid, host, port, user, pwd, front = sys.argv[1:7]
body = {
    "ids": [aid], "action": "proxy",
    "proxy": {"source": "custom", "protocol": "http", "host": host,
              "port": int(port), "username": user, "password": pwd},
}
if front.strip():
    body["frontProxy"] = {"source": "custom", "protocol": "http",
                          "host": "127.0.0.1", "port": int(front)}
else:
    body["frontProxy"] = None
print(json.dumps(body))
PY
)
  curl -sS -m 25 -b "$CK" -X POST "$BASE/api/accounts/batch" \
    -H 'Content-Type: application/json' -d "$BODY" \
    | python -c 'import sys,json; d=json.load(sys.stdin); ok=d.get("success", bool(d.get("data"))); print("    ok:", ok, "" if ok else str(d)[:200])'
done

echo "==> 复查"
curl -sS -m 20 -b "$CK" "$BASE/api/accounts" | PROVIDER="$PROVIDER_ID" python -c '
import sys, json, os
d = json.load(sys.stdin)
pid = os.environ["PROVIDER"]
for a in (d.get("data") or {}).get("accounts") or []:
    if a.get("provider") != pid:
        continue
    p = a.get("proxy") or {}
    f = a.get("frontProxy") or {}
    cfg = p.get("config") or p
    fcfg = f.get("config") or f
    print(f"    {a[\"id\"]}")
    print(f"      proxy      = {cfg.get(\"host\")}:{cfg.get(\"port\")}  ({cfg.get(\"source\", \"-\")})")
    print(f"      frontProxy = {fcfg.get(\"host\") or \"无\"}:{fcfg.get(\"port\") or \"\"}")
'
curl -sS -m 20 -b "$CK" "$BASE/api/custom-providers" | PROVIDER="$PROVIDER_ID" python -c '
import sys, json, os
pid = os.environ["PROVIDER"]
for it in (json.load(sys.stdin).get("data") or {}).get("providers") or []:
    if it["id"] == pid:
        print(f"    provider insecureTls={it.get(\"insecureTls\")} models={[m[\"id\"] for m in it.get(\"models\", [])]}")
'

echo
echo "完成。用网关 Key 验证："
echo "  curl $BASE/v1/chat/completions -H 'Authorization: Bearer sk-a2a-...' \\"
echo "    -H 'Content-Type: application/json' \\"
echo "    -d '{\"model\":\"gemini-4-argon\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}'"
echo
echo "验证链式是否真的生效（关键对照）：把 frontProxy 写成死端口应**失败**："
echo "  -d '{\"ids\":[\"<账号id>\"],\"action\":\"proxy\",\"frontProxy\":{\"source\":\"custom\",\"protocol\":\"http\",\"host\":\"127.0.0.1\",\"port\":19999},...}'"
