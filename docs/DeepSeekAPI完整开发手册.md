# DeepSeek API 完整开发手册

> 本文档基于 DeepSeek 官方文档整理，资料日期为 2026-07-13。模型、价格、限额和 Beta(测试版) 能力可能调整，生产使用前应复核文末官方链接。

## 1. 快速索引

| 需求 | 入口 |
| --- | --- |
| 普通对话与流式输出 | `POST /chat/completions` |
| 多轮对话 | 完整回传 `messages` |
| 思维链 | `thinking`、`reasoning_effort`、`reasoning_content` |
| 工具调用 | `tools`、`tool_choice`、`tool_calls` |
| JSON(数据交换格式) 输出 | `response_format={"type":"json_object"}` |
| 对话前缀续写 | Beta 地址 + `prefix=true` |
| FIM(中间填充) 补全 | `POST /beta/completions` |
| 模型列表 | `GET /models` |
| 余额查询 | `GET /user/balance` |
| Anthropic 兼容接口 | `https://api.deepseek.com/anthropic` |

## 2. 基础配置

### 2.1 地址与认证

| 项目 | 值 |
| --- | --- |
| OpenAI 兼容基础地址 | `https://api.deepseek.com` |
| OpenAI Beta 基础地址 | `https://api.deepseek.com/beta` |
| Anthropic 兼容基础地址 | `https://api.deepseek.com/anthropic` |
| 认证方式 | HTTP Bearer Token(持有者令牌) |
| 请求头 | `Authorization: Bearer ${DEEPSEEK_API_KEY}` |

API Key(接口密钥) 应从环境变量读取，不要写入代码或提交到仓库。

PowerShell：

```powershell
$env:DEEPSEEK_API_KEY = "你的 API Key"
```

Linux/macOS：

```bash
export DEEPSEEK_API_KEY="你的 API Key"
```

### 2.2 当前模型

| 项目 | `deepseek-v4-flash` | `deepseek-v4-pro` |
| --- | --- | --- |
| 模型版本 | DeepSeek-V4-Flash | DeepSeek-V4-Pro |
| 思考模式 | 支持非思考与思考模式，默认启用思考 |
| 上下文长度 | 1M Token(令牌) |
| 最大输出长度 | 384K Token |
| JSON Output | 支持 |
| Tool Calls(工具调用) | 支持 |
| 对话前缀续写 | 支持 |
| FIM 补全 | 仅非思考模式 |
| 账号并发限制 | 2500 | 500 |

`deepseek-chat` 和 `deepseek-reasoner` 将于北京时间 2026-07-24 23:59 弃用。兼容期内，二者分别对应 `deepseek-v4-flash` 的非思考模式和思考模式。

### 2.3 价格快照

以下价格单位为人民币元/百万 Token，仅表示 2026-07-13 官方页面数据：

| 计费项 | `deepseek-v4-flash` | `deepseek-v4-pro` |
| --- | ---: | ---: |
| 输入，缓存命中 | 0.02 | 0.025 |
| 输入，缓存未命中 | 1 | 3 |
| 输出 | 2 | 6 |

扣费公式：`费用 = Token 消耗量 × 模型单价`。同时存在赠送余额和充值余额时，优先扣减赠送余额。

## 3. 首次调用

### 3.1 cURL

```bash
curl https://api.deepseek.com/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer ${DEEPSEEK_API_KEY}" \
  -d '{
    "model": "deepseek-v4-pro",
    "messages": [
      {"role": "system", "content": "You are a helpful assistant."},
      {"role": "user", "content": "Hello!"}
    ],
    "thinking": {"type": "enabled"},
    "reasoning_effort": "high",
    "stream": false
  }'
```

### 3.2 Python

安装 SDK(软件开发工具包)：

```bash
pip install openai
```

```python
import os
from openai import OpenAI

client = OpenAI(
    api_key=os.environ["DEEPSEEK_API_KEY"],
    base_url="https://api.deepseek.com",
)

response = client.chat.completions.create(
    model="deepseek-v4-pro",
    messages=[
        {"role": "system", "content": "You are a helpful assistant."},
        {"role": "user", "content": "Hello!"},
    ],
    reasoning_effort="high",
    extra_body={"thinking": {"type": "enabled"}},
    stream=False,
)

print(response.choices[0].message.content)
```

### 3.3 Node.js

安装 SDK：

```bash
npm install openai
```

```javascript
import OpenAI from "openai";

const client = new OpenAI({
  apiKey: process.env.DEEPSEEK_API_KEY,
  baseURL: "https://api.deepseek.com",
});

const response = await client.chat.completions.create({
  model: "deepseek-v4-pro",
  messages: [{ role: "user", content: "Hello!" }],
  thinking: { type: "enabled" },
  reasoning_effort: "high",
  stream: false,
});

console.log(response.choices[0].message.content);
```

## 4. API 接口总览

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| `POST` | `/chat/completions` | 对话、思考、流式、JSON 输出和工具调用 |
| `POST` | `/beta/completions` | FIM 补全 |
| `GET` | `/models` | 获取当前可用模型 |
| `GET` | `/user/balance` | 查询账户余额 |

所有接口均使用 Bearer 认证。

### 4.1 聊天补全请求参数

| 参数 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `model` | 字符串 | 是 | `deepseek-v4-flash` 或 `deepseek-v4-pro` |
| `messages` | 对象数组 | 是 | 按时间顺序排列的对话消息 |
| `thinking` | 对象或空 | 否 | `{"type":"enabled"}` 或 `{"type":"disabled"}` |
| `reasoning_effort` | 字符串 | 否 | `high` 或 `max` |
| `max_tokens` | 整数或空 | 否 | 最大生成 Token 数，输入与输出总量不能超过上下文长度 |
| `response_format` | 对象或空 | 否 | 文本或 JSON 输出格式 |
| `stop` | 字符串、数组或空 | 否 | 停止生成条件 |
| `stream` | 布尔值 | 否 | 是否使用 SSE(服务器发送事件) 流式返回 |
| `stream_options` | 对象或空 | 否 | 流式输出选项 |
| `temperature` | 数字或空 | 否 | 0 至 2，默认 1；思考模式下无效 |
| `top_p` | 数字或空 | 否 | 0 至 1，默认 1；思考模式下无效 |
| `tools` | 对象数组或空 | 否 | 可供模型调用的函数定义 |
| `tool_choice` | 字符串或对象 | 否 | 控制模型是否以及如何选择工具 |
| `logprobs` | 布尔值或空 | 否 | 是否返回输出 Token 的对数概率 |
| `top_logprobs` | 整数或空 | 否 | 0 至 20，使用时 `logprobs` 必须为 `true` |
| `user_id` | 字符串或空 | 否 | 业务用户标识，用于安全、缓存和调度隔离 |

`frequency_penalty` 和 `presence_penalty` 已不再支持；即使传入也不会生效。采样参数通常只调整 `temperature` 或 `top_p` 中的一个。

### 4.2 消息角色

| `role` | 用途 |
| --- | --- |
| `system` | 系统指令 |
| `user` | 用户输入 |
| `assistant` | 模型回复，可能包含 `content`、`reasoning_content`、`tool_calls` |
| `tool` | 工具执行结果，必须携带 `tool_call_id` |

### 4.3 非流式响应

主要字段：

- `id`：本次补全的唯一标识。
- `choices`：候选结果数组。
- `choices[].message.content`：最终回答。
- `choices[].message.reasoning_content`：思考模式下的思维链。
- `choices[].message.tool_calls`：模型请求执行的工具。
- `created`：Unix 时间戳。
- `model`：实际使用的模型。
- `system_fingerprint`：后端配置指纹。
- `usage`：输入、输出、推理和缓存 Token 统计。

### 4.4 流式响应

`stream=true` 时，接口持续返回 SSE 数据，增量内容位于：

- `choices[].delta.reasoning_content`：思维链增量。
- `choices[].delta.content`：最终回答增量。

流以 `data: [DONE]` 结束。

```python
response = client.chat.completions.create(
    model="deepseek-v4-pro",
    messages=[{"role": "user", "content": "9.11 和 9.8 哪个更大？"}],
    stream=True,
    reasoning_effort="high",
    extra_body={"thinking": {"type": "enabled"}},
)

reasoning_content = ""
content = ""

for chunk in response:
    delta = chunk.choices[0].delta
    if delta.reasoning_content:
        reasoning_content += delta.reasoning_content
    if delta.content:
        content += delta.content

print("思维链：", reasoning_content)
print("最终回答：", content)
```

## 5. 多轮对话

`/chat/completions` 是无状态接口，服务端不保存前一轮上下文。每次请求都必须传入当前会话的完整 `messages`。

```python
messages = [{"role": "user", "content": "世界最高峰是什么？"}]

response = client.chat.completions.create(
    model="deepseek-v4-pro",
    messages=messages,
)
messages.append(response.choices[0].message)

messages.append({"role": "user", "content": "第二高峰呢？"})
response = client.chat.completions.create(
    model="deepseek-v4-pro",
    messages=messages,
)
messages.append(response.choices[0].message)
```

第二轮请求实际携带：用户第一问、模型第一答、用户第二问。后续轮次重复“追加模型回复、追加用户问题、重新发送完整历史”。

## 6. 思考模式与思维链

### 6.1 开关与强度

DeepSeek 当前模型默认开启思考模式。

| 功能 | OpenAI 格式 | Anthropic 格式 |
| --- | --- | --- |
| 开关 | `{"thinking":{"type":"enabled/disabled"}}` | `thinking` |
| 强度 | `{"reasoning_effort":"high/max"}` | `{"output_config":{"effort":"high/max"}}` |

普通请求默认强度为 `high`；部分复杂 Agent 请求会自动使用 `max`。兼容参数 `low`、`medium` 会映射到 `high`，`xhigh` 会映射到 `max`。

Python OpenAI SDK 中，`thinking` 通过 `extra_body` 传递：

```python
response = client.chat.completions.create(
    model="deepseek-v4-pro",
    messages=[{"role": "user", "content": "证明勾股定理"}],
    reasoning_effort="high",
    extra_body={"thinking": {"type": "enabled"}},
)

print(response.choices[0].message.reasoning_content)
print(response.choices[0].message.content)
```

### 6.2 参数限制

思考模式下，以下参数即使传入也不会生效：

- `temperature`
- `top_p`
- `presence_penalty`
- `frequency_penalty`

### 6.3 多轮拼接规则

- 没有工具调用时，上一轮 `reasoning_content` 无需加入下一轮上下文；即使传入也会被忽略。
- 存在工具调用时，当前用户轮次产生的 `reasoning_content` 必须随 `assistant` 消息完整回传。
- 思考模式的工具链中漏传 `reasoning_content` 会返回 HTTP 400。

最稳妥的方式是直接追加 SDK 返回的完整消息对象：

```python
messages.append(response.choices[0].message)
```

## 7. Tool Calls 工具调用

### 7.1 执行流程

1. 调用方在 `tools` 中声明可用函数和参数 Schema(模式)。
2. 模型通过 `message.tool_calls` 返回函数名和参数。
3. 调用方校验参数并执行本地函数。
4. 调用方以 `role="tool"` 和对应 `tool_call_id` 回传结果。
5. 模型基于工具结果继续回答，直到不再返回工具调用。

模型只生成调用请求，不会替调用方执行函数。工具参数可能不是合法 JSON，或包含未声明参数，执行前必须验证。

### 7.2 完整示例

```python
import json

tools = [
    {
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "查询指定城市的天气",
            "parameters": {
                "type": "object",
                "properties": {
                    "location": {"type": "string", "description": "城市名"}
                },
                "required": ["location"],
            },
        },
    }
]

messages = [{"role": "user", "content": "杭州天气怎么样？"}]

response = client.chat.completions.create(
    model="deepseek-v4-pro",
    messages=messages,
    tools=tools,
)
assistant_message = response.choices[0].message
messages.append(assistant_message)

tool_call = assistant_message.tool_calls[0]
arguments = json.loads(tool_call.function.arguments)
if tool_call.function.name != "get_weather" or "location" not in arguments:
    raise ValueError("非法工具调用")

tool_result = "24℃"
messages.append(
    {
        "role": "tool",
        "tool_call_id": tool_call.id,
        "content": tool_result,
    }
)

response = client.chat.completions.create(
    model="deepseek-v4-pro",
    messages=messages,
    tools=tools,
)
print(response.choices[0].message.content)
```

### 7.3 思考模式工具调用

思考模式支持多次“思考 → 调用工具 → 回传结果 → 继续思考”。每次将完整 `response.choices[0].message` 加入 `messages`，即可保留 `content`、`reasoning_content` 和 `tool_calls`。

### 7.4 strict 严格模式

strict(严格) 模式属于 Beta 功能：

- 使用 `base_url="https://api.deepseek.com/beta"`。
- 每个函数都设置 `"strict": true`。
- 每个对象的所有属性都列入 `required`。
- 每个对象都设置 `"additionalProperties": false`。
- 服务端会校验传入的 JSON Schema，不支持的定义会直接报错。

```json
{
  "type": "function",
  "function": {
    "name": "get_weather",
    "strict": true,
    "description": "查询天气",
    "parameters": {
      "type": "object",
      "properties": {
        "location": {"type": "string"}
      },
      "required": ["location"],
      "additionalProperties": false
    }
  }
}
```

支持的主要 Schema 类型：`object`、`string`、`number`、`integer`、`boolean`、`array`、`enum`、`anyOf`、`$ref` 和 `$def`。

- 字符串支持 `pattern`，以及 `email`、`hostname`、`ipv4`、`ipv6`、`uuid` 格式；不支持 `minLength`、`maxLength`。
- 数字支持 `const`、`default`、`minimum`、`maximum`、`exclusiveMinimum`、`exclusiveMaximum`、`multipleOf`。
- 数组不支持 `minItems`、`maxItems`。

## 8. JSON Output

使用条件：

1. 设置 `response_format={"type":"json_object"}`。
2. `system` 或 `user` 提示中必须包含 `json` 字样。
3. 提示中提供期望的 JSON 格式示例。
4. 合理设置 `max_tokens`，避免 JSON 被截断。

```python
import json

messages = [
    {
        "role": "system",
        "content": (
            "请将用户输入解析为 json。"
            "输出示例：{\"question\": \"...\", \"answer\": \"...\"}"
        ),
    },
    {
        "role": "user",
        "content": "世界最长的河流是什么？尼罗河。",
    },
]

response = client.chat.completions.create(
    model="deepseek-v4-pro",
    messages=messages,
    response_format={"type": "json_object"},
)
data = json.loads(response.choices[0].message.content)
```

官方提示：JSON Output 偶尔可能返回空 `content`，可尝试调整提示后重试。

## 9. 对话前缀续写

对话前缀续写沿用聊天补全接口，让模型从给定的 `assistant` 开头继续生成。

要求：

- 使用 Beta 基础地址。
- `messages` 最后一条消息的 `role` 必须为 `assistant`。
- 最后一条消息设置 `prefix=True`。

```python
beta_client = OpenAI(
    api_key=os.environ["DEEPSEEK_API_KEY"],
    base_url="https://api.deepseek.com/beta",
)

response = beta_client.chat.completions.create(
    model="deepseek-v4-pro",
    messages=[
        {"role": "user", "content": "请编写快速排序"},
        {"role": "assistant", "content": "```python\n", "prefix": True},
    ],
    stop=["```"],
)
print(response.choices[0].message.content)
```

## 10. FIM 补全

FIM 用于根据前缀 `prompt` 和可选后缀 `suffix` 补全中间内容，常用于代码补全。

限制：

- 使用 Beta 基础地址和 `/completions` 接口。
- 当前接口模型为 `deepseek-v4-pro`。
- 仅非思考模式支持。
- 最大补全长度为 4K Token。

```python
beta_client = OpenAI(
    api_key=os.environ["DEEPSEEK_API_KEY"],
    base_url="https://api.deepseek.com/beta",
)

response = beta_client.completions.create(
    model="deepseek-v4-pro",
    prompt="def fib(a):",
    suffix="    return fib(a - 1) + fib(a - 2)",
    max_tokens=128,
)
print(response.choices[0].text)
```

主要参数：`model`、`prompt`、`suffix`、`echo`、`logprobs`、`max_tokens`、`stop`、`stream`、`stream_options`、`temperature`、`top_p`。

## 11. 上下文硬盘缓存

KV Cache(键值缓存) 对所有用户默认开启，无需修改代码。后续请求与历史请求拥有完整相同前缀时，重复部分可能命中缓存并按较低输入价格计费。

### 11.1 落盘与命中

缓存前缀会在以下时机建立：

- 用户输入结束和模型输出结束。
- 系统检测到多次请求的公共前缀。
- 长输入或长输出达到固定 Token 间隔。

缓存前缀是完整单元，后续请求必须完整匹配该单元才算命中。多轮对话保持前序消息不变，更容易复用缓存。

### 11.2 查看命中量

响应 `usage` 中包含：

- `prompt_cache_hit_tokens`：缓存命中的输入 Token 数。
- `prompt_cache_miss_tokens`：缓存未命中的输入 Token 数。

### 11.3 限制

- 缓存是尽力而为，不保证 100% 命中。
- 缓存构建需要秒级时间。
- 不再使用的缓存通常会在数小时到数天后清理。
- 缓存只复用输入前缀，输出仍会重新推理，随机性与不使用缓存时一致。

## 12. 限速、隔离与保活

### 12.1 并发限制

- `deepseek-v4-pro`：每账号 500 并发。
- `deepseek-v4-flash`：每账号 2500 并发。
- 请求从发出到响应完成期间计为一个并发。
- 限制按账号计算，与 API Key 数量无关。
- 超出限制返回 HTTP 429。

更高并发需通过官方工单申请，扩容本身不增加额外费用。

### 12.2 user_id

`user_id` 用于内容安全、KV Cache 隔离和调度隔离。要求：

- 仅允许 `[a-zA-Z0-9\-_]+`。
- 最大长度 512。
- 不得包含用户隐私信息。

Python OpenAI SDK：

```python
response = client.chat.completions.create(
    model="deepseek-v4-pro",
    messages=[{"role": "user", "content": "Hello!"}],
    extra_body={"user_id": "user_123"},
)
```

### 12.3 请求保活

推理开始前等待期间：

- 非流式请求持续收到空行。
- 流式请求持续收到 `: keep-alive` SSE 注释。
- 这些内容不属于 JSON 响应正文，自行解析 HTTP 时需要忽略。
- 10 分钟后仍未开始推理，服务器会关闭连接。

## 13. Token 用量与余额

官方给出的粗略换算：

- 1 个英文字符约为 0.3 Token。
- 1 个中文字符约为 0.6 Token。

不同模型的分词方式不同，实际用量以响应 `usage` 为准。

查询当前可用模型：

```bash
curl https://api.deepseek.com/models \
  -H "Authorization: Bearer ${DEEPSEEK_API_KEY}"
```

查询余额：

```bash
curl https://api.deepseek.com/user/balance \
  -H "Authorization: Bearer ${DEEPSEEK_API_KEY}"
```

余额响应主要字段：

- `is_available`：余额是否足以继续调用 API。
- `balance_infos`：各币种余额明细。

## 14. Anthropic API 兼容模式

安装：

```bash
pip install anthropic
```

配置：

```bash
export ANTHROPIC_BASE_URL="https://api.deepseek.com/anthropic"
export ANTHROPIC_API_KEY="${DEEPSEEK_API_KEY}"
```

调用：

```python
import anthropic

client = anthropic.Anthropic()
message = client.messages.create(
    model="deepseek-v4-pro",
    max_tokens=1000,
    system="You are a helpful assistant.",
    messages=[{"role": "user", "content": "Hi, how are you?"}],
)
print(message.content)
```

模型映射：

- `claude-opus*` → `deepseek-v4-pro`
- `claude-haiku*`、`claude-sonnet*` → `deepseek-v4-flash`
- 其他不支持的模型名会自动映射到 `deepseek-v4-flash`

主要兼容性：

| 类别 | 支持情况 |
| --- | --- |
| `x-api-key`、`max_tokens`、`stream`、`system`、`stop_sequences` | 完全支持 |
| `temperature`、`top_p` | 支持 |
| `thinking` | 支持，忽略 `budget_tokens` |
| `output_config` | 仅支持 `effort` |
| `metadata` | 仅支持 `user_id` |
| 文本、工具调用、工具结果、思考内容 | 支持 |
| 图片、文档、代码执行结果、MCP 工具 | 不支持 |
| `anthropic-beta`、`anthropic-version`、`top_k` | 忽略 |

## 15. 错误码与处理

| 状态码 | 原因 | 处理方式 |
| --- | --- | --- |
| 400 | 请求体格式错误，或思考工具链缺少必要字段 | 修正格式并完整回传消息 |
| 401 | API Key 错误 | 检查密钥和认证头 |
| 402 | 余额不足 | 查询余额并充值 |
| 422 | 参数错误 | 根据错误信息修正参数 |
| 429 | 并发或速率达到上限 | 控制并发、退避重试或申请扩容 |
| 500 | 服务端内部故障 | 延迟后重试，持续失败时联系官方 |
| 503 | 服务端繁忙 | 延迟后重试 |

重试仅适用于临时错误。对 400、401、402、422 不应盲目重试；对 429、500、503 可使用带随机抖动的指数退避。

## 16. 生产接入检查清单

- API Key 仅从密钥管理系统或环境变量读取。
- 对每个工具调用校验函数名、JSON 参数和权限。
- 为请求设置连接、读取和总执行超时。
- 正确处理流式空行、SSE keep-alive 和 `[DONE]`。
- 限制会话历史长度，并保留多轮消息顺序。
- 思考工具链完整回传 `reasoning_content`。
- 记录 `usage`、缓存命中、延迟、状态码和模型名。
- 对 429、500、503 做有限次数的退避重试。
- 价格、模型名、上下文长度和并发值从配置读取并定期复核。
- Beta 功能上线前验证兼容性和回退方案。

## 17. 官方资料

- [首次调用 API](https://api-docs.deepseek.com/zh-cn/)
- [模型与价格](https://api-docs.deepseek.com/zh-cn/quick_start/pricing)
- [Token 用量计算](https://api-docs.deepseek.com/zh-cn/quick_start/token_usage)
- [限速与隔离](https://api-docs.deepseek.com/zh-cn/quick_start/rate_limit)
- [错误码](https://api-docs.deepseek.com/zh-cn/quick_start/error_codes)
- [思考模式](https://api-docs.deepseek.com/zh-cn/guides/thinking_mode)
- [多轮对话](https://api-docs.deepseek.com/zh-cn/guides/multi_round_chat)
- [对话前缀续写](https://api-docs.deepseek.com/zh-cn/guides/chat_prefix_completion)
- [FIM 补全](https://api-docs.deepseek.com/zh-cn/guides/fim_completion)
- [JSON Output](https://api-docs.deepseek.com/zh-cn/guides/json_mode)
- [Tool Calls](https://api-docs.deepseek.com/zh-cn/guides/tool_calls)
- [上下文硬盘缓存](https://api-docs.deepseek.com/zh-cn/guides/kv_cache)
- [Anthropic API](https://api-docs.deepseek.com/zh-cn/guides/anthropic_api)
- [聊天补全 API 参考](https://api-docs.deepseek.com/api/create-chat-completion)
- [FIM 补全 API 参考](https://api-docs.deepseek.com/api/create-completion)
- [模型列表 API 参考](https://api-docs.deepseek.com/api/list-models)
- [余额查询 API 参考](https://api-docs.deepseek.com/api/get-user-balance)
- [Agent 工具接入指南](https://api-docs.deepseek.com/zh-cn/quick_start/agent_integrations/claude_code)
- [更新日志](https://api-docs.deepseek.com/zh-cn/updates)
