# 百炼当月花费 → ccsp 状态栏

在状态栏显示某把百炼 API key 的**当月花费**（如 `本月百炼 ¥12.34`）。

## 为什么是 sidecar 架构

状态栏（ccsp）每次刷新都会跑，进程里只有模型 key（`DASHSCOPE_API_KEY`），**没有阿里云账单凭证**；
而账单是一次**签名的、慢的 BSS 查询**（`DescribeInstanceBill`, ProductCode=`sfm`），且当月数据
**~1 天才更新一次**。所以：

```
cron ──每小时/每天──▶ bailian-cost-refresh.sh ──▶ 写 JSON 缓存
                                                      │
ccsp 每次刷新 ──▶ file widget 只读该 JSON（零上游、零延迟、零凭证）
```

> ⚠️ 账单只有「钱」没有「token 数」，`sk-xxx` 到数字 ApiKeyID 也无 API 映射——所以这里只显示
> **花费**，且 key 用**数字 ApiKeyID**（去[百炼控制台 API-KEY 页](https://bailian.console.aliyun.com/)看）。
> 详见 skill `aliyun-bailian-bill`。

## 前置

- `aliyun` CLI，且配好一个**有 BSS 费用只读权限**的 profile（默认 `mm`；`aliyun configure list | grep mm`）
- `jq`
- 知道要统计的**数字 ApiKeyID**（本机 `DASHSCOPE_API_KEY` = 「李睿测试ai」= `2076695`）

## 装配

1) 部署刷新脚本 + 建 cron（示例每小时第 17 分钟跑一次；账期数据一天才变，别太频繁）：

```bash
install -m755 bailian-cost-refresh.sh ~/.claude/statusline-pro/bailian-cost-refresh.sh
# 先手动跑一次，确认能出数：
BAILIAN_KEY_ID=2076695 BAILIAN_PROFILE=mm ~/.claude/statusline-pro/bailian-cost-refresh.sh
# 挂 cron：
( crontab -l 2>/dev/null; \
  echo '17 * * * * BAILIAN_KEY_ID=2076695 BAILIAN_PROFILE=mm $HOME/.claude/statusline-pro/bailian-cost-refresh.sh >/dev/null 2>&1' \
) | crontab -
```

2) 接线 file widget：把 `usage.toml` 里的 `[widgets.bailian_cost]` 段合并进
   `~/.claude/statusline-pro/components/usage.toml`（或任何已启用组件的 TOML）。前提：
   主配置 `[multiline].enabled = true`，且该组件在 `[components].order` / preset 里（`U` 在 `PMBTURS`）。

3) 验证：

```bash
# 缓存文件有内容：
cat ~/.claude/statusline-pro/cache/bailian-cost.json
# 状态栏渲染出这一行（真二进制）：
echo '{"model":{"id":"claude-sonnet-4"},"cwd":"/tmp","session_id":"t"}' | ccsp
# 期望在主行下面看到：💰 本月百炼 ¥xx.xx
```

## 可调项（环境变量，传给刷新脚本）

| 变量 | 默认 | 说明 |
|---|---|---|
| `BAILIAN_KEY_ID` | `2076695` | 数字 ApiKeyID |
| `BAILIAN_PROFILE` | `mm` | 有 BSS 权限的 aliyun profile |
| `BAILIAN_COST_CACHE` | `~/.claude/statusline-pro/cache/bailian-cost.json` | 缓存输出路径（须与 widget `file.path` 一致） |
| `BAILIAN_CYCLE` | 本月 `YYYY-MM` | 账期 |

## 注意

- 缓存文件不存在（cron 还没跑）时，widget **自动隐藏**，不报错。
- 只覆盖「模型走百炼」的花费；GLM/Kimi 等走厂商官方/中转的不在阿里云账单里。
- 当月数字有 ~1 天延迟、次月 3 号才定稿，别当对账依据。
