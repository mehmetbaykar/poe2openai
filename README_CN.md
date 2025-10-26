# 🔄 POE to OpenAI API

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Docker Version](https://img.shields.io/docker/v/mehmetbaykar/poe2openai?sort=semver)](https://hub.docker.com/r/mehmetbaykar/poe2openai)
[![Docker Size](https://img.shields.io/docker/image-size/mehmetbaykar/poe2openai/latest
)](https://hub.docker.com/r/mehmetbaykar/poe2openai)
[![Docker Pulls](https://img.shields.io/docker/pulls/mehmetbaykar/poe2openai)](https://hub.docker.com/r/mehmetbaykar/poe2openai)

[ [English](https://github.com/jeromeleong/poe2openai/blob/master/README_EN.md) | [繁體中文](https://github.com/jeromeleong/poe2openai/blob/master/README.md) | [简体中文](https://github.com/jeromeleong/poe2openai/blob/master/README_CN.md) ]

Poe2OpenAI 是一个将 POE API 转换为 OpenAI API 格式的代理服务。让 Poe 订阅者能够通过 OpenAI API 格式使用 Poe 的各种 AI 模型。

## 📑 目录
- [主要特点](#-主要特点)
- [安装指南](#-安装指南)
- [快速开始](#-快速开始)
- [API 文档](#-api-文档)
- [配置说明](#️-配置说明)
- [常见问题](#-常见问题)
- [贡献指南](#-贡献指南)
- [授权协议](#-授权协议)

## ✨ 主要特点
- 🌐 支持使用代理的 POE URL（环境变量为 `POE_BASE_URL` 和 `POE_FILE_UPLOAD_URL`）
- 🔄 支持 OpenAI API 格式（`/models` 和 `/chat/completions`）
- 💬 支持流式和非流式模式
- 🔧 使用內置的 XML 提示語增加原有工具調用 (Tool Calls) 的兼容性和成功率
- 🖼️ 支持文件上传并加入对话 (URL 和 Base64)
- 🌐 对最新 POE API 的 Event 进行完整处理
- 🤖 支持 Claude/Roo Code 解析，包括 Token 用量统计
- 📊 Web 管理界面(`/admin`)用于配置模型（模型映射和编辑`/models`显示的模型）
- 🔒 支持速率限制控制，防止请求过于频繁
- 📦 内置 URL 和 Base64 图片缓存系统，减少重复上传
- 🧠 基于 Deepseek OpenAI 格式，把 `Thinking...` 的推理思考内容放到`reasoning_content`中
- 🎯 支持高级推理选项（reasoning_effort、thinking、extra_body 参数）
- 🐳 Docker 部署支持

## 🔧 安装指南
### 使用 Docker（简单部署）
```bash
# 拉取镜像
docker pull mehmetbaykar/poe2openai:latest
# 运行容器
docker run --name poe2openai -d \
  -p 8080:8080 \
  -e ADMIN_USERNAME=admin \
  -e ADMIN_PASSWORD=123456 \
  mehmetbaykar/poe2openai:latest
```

#### 数据持久化（可选）
```bash
# 创建本地数据目录
mkdir -p /path/to/data
# 运行容器并挂载数据目录
docker run --name poe2openai -d \
  -p 8080:8080 \
  -v /path/to/data:/data \
  -e CONFIG_DIR=/data \
  -e ADMIN_USERNAME=admin \
  -e ADMIN_PASSWORD=123456 \
  mehmetbaykar/poe2openai:latest
```

### 使用 Docker Compose
具体内容可根据自己个人需求来进行修改
```yaml
version: '3.8'
services:
  poe2openai:
    image: mehmetbaykar/poe2openai:latest
    ports:
      - "8080:8080"
    environment:
      - PORT=8080
      - LOG_LEVEL=info
      - ADMIN_USERNAME=admin
      - ADMIN_PASSWORD=123456
      - MAX_REQUEST_SIZE=1073741824
      - CONFIG_DIR=/data
      - RATE_LIMIT_MS=100
      - URL_CACHE_TTL_SECONDS=259200
      - URL_CACHE_SIZE_MB=100
      - POE_BASE_URL=https://api.poe.com
      - POE_FILE_UPLOAD_URL=https://www.quora.com/poe_api/file_upload_3RD_PARTY_POST
    volumes:
      - /path/to/data:/data
```

### 从源码编译
```bash
# 克隆项目
git clone https://github.com/jeromeleong/poe2openai
cd poe2openai
# 编译
cargo build --release
# 运行
./target/release/poe2openai
```

## 🚀 快速开始
1. 使用 Docker 启动服务：
```bash
docker run -d -p 8080:8080 mehmetbaykar/poe2openai:latest
```
2. 服务器默认在 `http://localhost:8080` 启动
3. 使用方式示例：
```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer your-poe-token" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'
```
4. 可以在 `http://localhost:8080/admin` 管理模型

## 📖 API 文档
### 支持的 OpenAI API 端点
- `GET /v1/models` - 获取可用模型列表
- `POST /v1/chat/completions` - 与 POE 模型聊天
- `GET /models` - 获取可用模型列表（兼容端点）
- `POST /chat/completions` - 与 POE 模型聊天（兼容端点）

### 请求格式
```json
{
  "model": "string",
  "messages": [
    {
      "role": "user",
      "content": "string"
    }
  ],
  "temperature": 0.7,
  "stream": false,
  "tools": [],
  "stream_options": {
    "include_usage": false
  },
  "reasoning_effort": "medium",
  "extra_body": {}
}
```

#### 可选参数说明
| 参数           | 类型     | 默认值       | 说明                                                 |
|---------------|----------|--------------|------------------------------------------------------|
| model         | string   | (必填)       | 要请求的模型名称                                     |
| messages      | array    | (必填)       | 聊天消息列表，支持纯文字或多模态内容（文字+图片）      |
| temperature   | float    | null         | 探索性(0~2)。控制回答的多样性，数值越大越发散         |
| stream        | bool     | false        | 是否流式返回（SSE），true 开启流式                    |
| tools         | array    | null         | 工具描述 (Tool Calls) 支持（如 function calling）     |
| logit_bias    | object   | null         | 特定 token 的偏好值，格式为 key-value 对应             |
| stop          | array    | null         | 停止生成的文字序列数组                               |
| stream_options| object   | null         | 流式细部选项，支持 include_usage (bool): 是否附带用量统计|
| reasoning_effort| string | null         | 推理努力程度，可选值：low, medium, high               |
| thinking      | object   | null         | 思考配置，可设定 budget_tokens (0-30768): 思考阶段的 token 预算|
| extra_body    | object   | null         | 额外的请求参数，支持 Google 特定配置如 google.thinking_config.thinking_budget(0-30768)|

> 其他参数如 top_p、n 等 OpenAI 参数暂不支持，提交会被忽略。

### 响应格式
```json
{
  "id": "chatcmpl-xxx",
  "object": "chat.completion",
  "created": 1677858242,
  "model": "gpt-4o-mini",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "响应内容",
        "reasoning_content": "推理思考过程"
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 10,
    "completion_tokens": 20,
    "total_tokens": 30,
    "prompt_tokens_details": {
      "cached_tokens": 0
    }
  }
}
```

### 多模态请求范例
```json
{
  "model": "claude-3-opus",
  "messages": [
    {
      "role": "user",
      "content": [
        {
          "type": "text",
          "text": "这张图片是什么？"
        },
        {
          "type": "image_url",
          "image_url": {
            "url": "https://example.com/image.jpg"
          }
        }
      ]
    }
  ]
}
```

## ⚙️ 配置说明
服务器配置通过环境变量进行：
- `PORT` - 服务器端口（默认：`8080`）
- `HOST` - 服务器主机（默认：`0.0.0.0`）
- `ADMIN_USERNAME` - 管理界面用户名（默认：`admin`）
- `ADMIN_PASSWORD` - 管理界面密码（默认：`123456`）
- `MAX_REQUEST_SIZE` - 最大请求大小（默认：`1073741824`，1GB）
- `LOG_LEVEL` - 日志级别（默认：`info`，可选：`debug`, `info`, `warn`, `error`）
- `CONFIG_DIR` - 配置文件目录路径（docker 环境中默认为：`/data`，本机环境中默认为：`./`）
- `RATE_LIMIT_MS` - 全局速率限制（毫秒，默认：`100`，设置为 `0` 禁用）
- `URL_CACHE_TTL_SECONDS` - Poe CDN URL缓存有效期（秒，默认：`259200`，3天）
- `URL_CACHE_SIZE_MB` - Poe CDN URL缓存最大容量（MB，默认：`100`）
- `POE_BASE_URL` - Poe API 基础 URL（默认：`https://api.poe.com`）
- `POE_FILE_UPLOAD_URL` - Poe 文件上传 URL（默认：`https://www.quora.com/poe_api/file_upload_3RD_PARTY_POST`）

## ❓ 常见问题
### Q: Poe API Token 如何获取？
A: 首先要订阅 Poe，才能从 [Poe API Key](https://poe.com/api_key) 网页中获取。

### Q: 为什么会收到认证错误？
A: 确保在请求头中正确设置了 `Authorization: Bearer your-poe-token`。

### Q: 支持哪些模型？
A: 支持所有 POE 平台上可用的模型，可通过 `/v1/models` 端点查询。

### Q: 如何修改服务器端口？
A: 可以通过设置环境变量 `PORT` 来修改，例如：
```bash
docker run -d -e PORT=3000 -p 3000:3000 mehmetbaykar/poe2openai:latest
```

### Q: 如何使用 models.yaml 配置模型？
A: 在管理界面 `/admin` 页面中可以进行模型配置，也可以手动编辑 `CONFIG_DIR` 目录下的 `models.yaml` 文件。

### Q: 如何处理请求频率限制？
A: 可以通过设置环境变量 `RATE_LIMIT_MS` 来控制请求间隔，单位为毫秒。设置为 `0` 则禁用限制。

## 🐳 Docker Hub 自动构建

本项目使用 GitHub Actions 在每次推送到主分支时自动构建并发布 Docker 镜像到 Docker Hub。

### 仓库信息
- **Docker Hub 仓库**: `mehmetbaykar/poe2openai`
- **镜像标签**: `latest`
- **自动构建**: 在每次推送到主分支时触发

### Docker 拉取命令
```bash
docker pull mehmetbaykar/poe2openai:latest
```

## 🤝 贡献指南
欢迎所有形式的贡献！如果您发现了问题或有改进建议，请提交 Issue 或 Pull Request。

## 📄 授权协议
本项目使用 [MIT 授权协议](LICENSE)。

## 🌟 Star 历史
[![Star History Chart](https://api.star-history.com/svg?repos=jeromeleong/poe2openai&type=Date)](https://star-history.com/#jeromeleong/poe2openai&Date)