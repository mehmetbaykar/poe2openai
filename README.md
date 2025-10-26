# 🔄 POE to OpenAI API

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Docker Version](https://img.shields.io/docker/v/mehmetbaykar/poe2openai?sort=semver)](https://hub.docker.com/r/mehmetbaykar/poe2openai)
[![Docker Size](https://img.shields.io/docker/image-size/mehmetbaykar/poe2openai/latest
)](https://hub.docker.com/r/mehmetbaykar/poe2openai)
[![Docker Pulls](https://img.shields.io/docker/pulls/mehmetbaykar/poe2openai)](https://hub.docker.com/r/mehmetbaykar/poe2openai)

[ [English](https://github.com/jeromeleong/poe2openai/blob/master/README_EN.md) | [繁體中文](https://github.com/jeromeleong/poe2openai/blob/master/README.md) | [简体中文](https://github.com/jeromeleong/poe2openai/blob/master/README_CN.md) ]

Poe2OpenAI 是一個將 POE API 轉換為 OpenAI API 格式的代理服務。讓 Poe 訂閱者能夠通過 OpenAI API 格式使用 Poe 的各種 AI 模型。

## 📑 目錄
- [主要特點](#-主要特點)
- [安裝指南](#-安裝指南)
- [快速開始](#-快速開始)
- [API 文檔](#-api-文檔)
- [配置說明](#️-配置說明)
- [常見問題](#-常見問題)
- [貢獻指南](#-貢獻指南)
- [授權協議](#-授權協議)

## ✨ 主要特點
- 🌐 支持使用代理的 POE URL（環境變量為 `POE_BASE_URL` 和 `POE_FILE_UPLOAD_URL`）
- 🔄 支持 OpenAI API 格式（`/models` 和 `/chat/completions`）
- 💬 支持串流和非串流模式
- 🔧 使用內置的 XML 提示語增加原有工具調用 (Tool Calls) 的兼容性和成功率
- 🖼️ 支持文件上傳並加入對話 (URL 和 Base64)
- 🌐 對最新 POE API 的 Event 進行完整處理
- 🤖 支持 Claude/Roo Code 解析，包括 Token 用量統計
- 📊 Web 管理介面(`/admin`)用於配置模型（模型映射和編輯`/models`顯示的模型）
- 🔒 支持速率限制控制，防止請求過於頻繁
- 📦 內建 URL 和 Base64 圖片緩存系統，減少重複上傳
- 🧠 基於 Deepseek OpenAI 格式，把 `Thinking...` 的推理思考內容放到`reasoning_content`中
- 🎯 支持高級推理選項（reasoning_effort、thinking、extra_body 參數）
- 🐳 Docker 佈置支持

## 🔧 安裝指南

### 使用 Docker（簡單部署）
```bash
# 拉取映像
docker pull mehmetbaykar/poe2openai:latest

# 運行容器
docker run --name poe2openai -d \
  -p 8080:8080 \
  -e ADMIN_USERNAME=admin \
  -e ADMIN_PASSWORD=123456 \
  mehmetbaykar/poe2openai:latest
```

#### 數據持久化（可選）
```bash
# 創建本地數據目錄
mkdir -p /path/to/data

# 運行容器並掛載數據目錄
docker run --name poe2openai -d \
  -p 8080:8080 \
  -v /path/to/data:/data \
  -e CONFIG_DIR=/data \
  -e ADMIN_USERNAME=admin \
  -e ADMIN_PASSWORD=123456 \
  jeromeleong/poe2openai:latest
```

### 使用 Docker Compose
具體內容可根據自己個人需求來進行修改
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

### 從源碼編譯
```bash
# 克隆專案
git clone https://github.com/jeromeleong/poe2openai
cd poe2openai

# 編譯
cargo build --release

# 運行
./target/release/poe2openai
```

## 🚀 快速開始

1. 使用 Docker 啟動服務：
```bash
docker run -d -p 8080:8080 mehmetbaykar/poe2openai:latest
```

2. 服務器默認在 `http://localhost:8080` 啟動

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

4. 可以在 `http://localhost:8080/admin` 管理模型和配置 API Token

## 📖 API 文檔

### 支援的 OpenAI API 端點
- `GET /v1/models` - 獲取可用模型列表
- `POST /v1/chat/completions` - 與 POE 模型聊天
- `GET /models` - 獲取可用模型列表（相容端點）
- `POST /chat/completions` - 與 POE 模型聊天（相容端點）

### 請求格式
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

#### 可選參數說明
| 參數           | 類型     | 預設值       | 說明                                                 |
|---------------|----------|--------------|------------------------------------------------------|
| model         | string   | (必填)       | 要請求的模型名稱                                     |
| messages      | array    | (必填)       | 聊天訊息列表，支援純文字或多模態內容（文字+圖片）      |
| temperature   | float    | null         | 探索性(0~2)。控制回答的多樣性，數值越大越發散         |
| stream        | bool     | false        | 是否串流回傳（SSE），true 開啟串流                    |
| tools         | array    | null         | 工具描述 (Tool Calls) 支援（如 function calling）     |
| logit_bias    | object   | null         | 特定 token 的偏好值，格式為 key-value 對應             |
| stop          | array    | null         | 停止生成的文字序列陣列                               |
| stream_options| object   | null         | 串流細部選項，支援 include_usage (bool): 是否附帶用量統計|
| reasoning_effort| string | null         | 推理努力程度，可選值：low, medium, high               |
| thinking      | object   | null         | 思考配置，可設定 budget_tokens (0-30768): 思考階段的 token 預算|
| extra_body    | object   | null         | 額外的請求參數，支援 Google 特定配置如 google.thinking_config.thinking_budget(0-30768)|                     |

> 其他參數如 top_p、n 等 OpenAI 參數暫不支援，提交會被忽略。

### 響應格式
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
        "content": "回應內容",
        "reasoning_content": "推理思考過程"
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

### 多模態請求範例
```json
{
  "model": "claude-3-opus",
  "messages": [
    {
      "role": "user",
      "content": [
        {
          "type": "text",
          "text": "這張圖片是什麼？"
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

## ⚙️ 配置說明
服務器配置通過環境變量進行：
- `PORT` - 服務器端口（默認：`8080`）
- `HOST` - 服務器主機（默認：`0.0.0.0`）
- `ADMIN_USERNAME` - 管理介面用戶名（默認：`admin`）
- `ADMIN_PASSWORD` - 管理介面密碼（默認：`123456`）
- `MAX_REQUEST_SIZE` - 最大請求大小（默認：`1073741824`，1GB）
- `LOG_LEVEL` - 日誌級別（默認：`info`，可選：`debug`, `info`, `warn`, `error`）
- `CONFIG_DIR` - 配置文件目錄路徑（docker 環境中默認為：`/data`，本機環境中默認為：`./`）
- `RATE_LIMIT_MS` - 全局速率限制（毫秒，默認：`100`，設置為 `0` 禁用）
- `URL_CACHE_TTL_SECONDS` - Poe CDN URL緩存有效期（秒，默認：`259200`，3天）
- `URL_CACHE_SIZE_MB` - Poe CDN URL緩存最大容量（MB，默認：`100`）
- `POE_BASE_URL` - Poe API 基礎 URL（默認：`https://api.poe.com`）
- `POE_FILE_UPLOAD_URL` - Poe 文件上傳 URL（默認：`https://www.quora.com/poe_api/file_upload_3RD_PARTY_POST`）

## ❓ 常見問題

### Q: Poe API Token 如何獲取？
A: 首先要訂閱 Poe，才能從 [Poe API Key](https://poe.com/api_key) 網頁中取得。

### Q: 為什麼會收到認證錯誤？
A: 確保在請求頭中正確設置了 `Authorization: Bearer your-poe-token`。

### Q: 支援哪些模型？
A: 支援所有 POE 平台上可用的模型，可通過 `/v1/models` 端點查詢。

### Q: 如何修改服務器端口？
A: 可以通過設置環境變量 `PORT` 來修改，例如：
```bash
docker run -d -e PORT=3000 -p 3000:3000 mehmetbaykar/poe2openai:latest
```

### Q: 如何使用 models.yaml 配置模型？
A: 在管理介面 `/admin` 頁面中可以進行模型配置，也可以手動編輯 `CONFIG_DIR` 目錄下的 `models.yaml` 文件。

### Q: 如何處理請求頻率限制？
A: 可以通過設置環境變量 `RATE_LIMIT_MS` 來控制請求間隔，單位為毫秒。設置為 `0` 則禁用限制。

## 🐳 Docker Hub 自動建構

本專案使用 GitHub Actions 在每次推送到主分支時自動建構並發布 Docker 映像到 Docker Hub。

### 倉庫資訊
- **Docker Hub 倉庫**: `mehmetbaykar/poe2openai`
- **映像標籤**: `latest`
- **自動建構**: 在每次推送到主分支時觸發

### Docker 拉取命令
```bash
docker pull mehmetbaykar/poe2openai:latest
```

## 🤝 貢獻指南
歡迎所有形式的貢獻！如果您發現了問題或有改進建議，請提交 Issue 或 Pull Request。

## 📄 授權協議
本專案使用 [MIT 授權協議](LICENSE)。

## 🌟 Star 歷史
[![Star History Chart](https://api.star-history.com/svg?repos=jeromeleong/poe2openai&type=Date)](https://star-history.com/#jeromeleong/poe2openai&Date)