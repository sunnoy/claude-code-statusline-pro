#!/usr/bin/env bash
# bailian-cost-refresh.sh — 把「某把百炼 API key 的当月花费」刷进一个小 JSON 缓存，
# 供 ccsp 的 `file` 状态栏 widget 读取。
#
# 为什么要 sidecar（而不是状态栏直接查）：
#   * ccsp 每次状态栏刷新都跑一次，手里只有模型 key(DASHSCOPE_API_KEY)，没有阿里云
#     账单凭证；账单是慢的、签名的 BSS 查询(ProductCode=sfm)，且 ~1 天才更新一次。
#   * 所以让 cron 定期跑本脚本写 JSON，ccsp 的 file widget 只读文件，零上游零延迟。
#   * 账单事实见 skill `aliyun-bailian-bill`：ProductCode=sfm、端点 business.aliyuncs.com、
#     账单只给钱不给 token 数、sk-xxx↔数字 ApiKeyID 无 API 映射(去控制台 API-KEY 页看)。
#
# 依赖：aliyun CLI(已配好有 BSS 只读权限的 profile) + jq。
#
# 用法：
#   BAILIAN_KEY_ID=2076695 ./bailian-cost-refresh.sh
#   BAILIAN_PROFILE=mm BAILIAN_COST_CACHE=~/.claude/statusline-pro/cache/bailian-cost.json ./bailian-cost-refresh.sh
set -euo pipefail

KEY="${BAILIAN_KEY_ID:-2076695}"            # 数字 ApiKeyID（控制台 API-KEY 页；默认=李睿测试ai）
PROFILE="${BAILIAN_PROFILE:-mm}"            # aliyun CLI profile（需 BSS 费用只读权限）
OUT="${BAILIAN_COST_CACHE:-$HOME/.claude/statusline-pro/cache/bailian-cost.json}"
ENDPOINT=business.aliyuncs.com
PRODUCT=sfm                                  # 百炼的 ProductCode（不是 bailian/dashscope）
CYCLE="${BAILIAN_CYCLE:-$(date +%Y-%m)}"    # 账期 YYYY-MM，默认本月

command -v aliyun >/dev/null || { echo "bailian-cost-refresh: 缺 aliyun CLI" >&2; exit 1; }
command -v jq >/dev/null     || { echo "bailian-cost-refresh: 缺 jq" >&2; exit 1; }

# 逐页累加当月 InstanceID 首段(ApiKeyID) == $KEY 的 PretaxAmount(元)
sum=0; next=""
while :; do
  if [ -z "$next" ]; then
    resp=$(aliyun bssopenapi DescribeInstanceBill --BillingCycle "$CYCLE" --Granularity MONTHLY \
           --ProductCode "$PRODUCT" --MaxResults 300 --endpoint "$ENDPOINT" --profile "$PROFILE" 2>/dev/null || true)
  else
    resp=$(aliyun bssopenapi DescribeInstanceBill --BillingCycle "$CYCLE" --Granularity MONTHLY \
           --ProductCode "$PRODUCT" --MaxResults 300 --NextToken "$next" --endpoint "$ENDPOINT" --profile "$PROFILE" 2>/dev/null || true)
  fi
  [ -z "$resp" ] && break
  part=$(printf '%s' "$resp" | jq -r --arg k "$KEY" \
    '[ .Data.Items[]? | select((.InstanceID|split(";")[0]) == $k) | .PretaxAmount ] | add // 0')
  sum=$(awk -v a="$sum" -v b="${part:-0}" 'BEGIN{printf "%.4f", a+b}')
  next=$(printf '%s' "$resp" | jq -r '.Data.NextToken // ""')
  [ -z "$next" ] && break
done

# 原子写出 JSON 缓存
mkdir -p "$(dirname "$OUT")"
tmp=$(mktemp)
jq -n --argjson cny "$sum" --arg month "$CYCLE" --arg key "$KEY" \
      --arg updated "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
   '{cny: $cny, month: $month, key_id: $key, updated_at: $updated}' > "$tmp"
mv -f "$tmp" "$OUT"
echo "bailian-cost-refresh: wrote $OUT  →  ¥$sum ($CYCLE, key $KEY)"
