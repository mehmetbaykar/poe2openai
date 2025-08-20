# 🔄 POE to OpenAI API

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Docker Version](https://img.shields.io/docker/v/jeromeleong/poe2openai?sort=semver)](https://hub.docker.com/r/jeromeleong/poe2openai)
[![Docker Size](https://img.shields.io/docker/image-size/jeromeleong/poe2openai/latest
)](https://hub.docker.com/r/jeromeleong/poe2openai)
[![Docker Pulls](https://img.shields.io/docker/pulls/jeromeleong/poe2openai)](https://hub.docker.com/r/jeromeleong/poe2openai)

[ [English](https://github.com/jeromeleong/poe2openai/blob/master/README_EN.md) | [繁體中文](https://github.com/jeromeleong/poe2openai/blob/master/README.md) | [简体中文](https://github.com/jeromeleong/poe2openai/blob/master/README_CN.md) ]

Poe2OpenAI is a proxy service that converts the POE API to OpenAI API format. It allows Poe subscribers to use various AI models on Poe through the OpenAI API format.

## 📑 Table of Contents
- [Key Features](#-key-features)
- [Installation Guide](#-installation-guide)
- [Quick Start](#-quick-start)
- [API Documentation](#-api-documentation)
- [Configuration](#️-configuration)
- [FAQ](#-faq)
- [Contributing](#-contributing)
- [License](#-license)

## ✨ Key Features
- 🌐 Support for proxied POE URLs (environment variables `POE_BASE_URL` and `POE_FILE_UPLOAD_URL`)
- 🔄 Support for OpenAI API format (`/models` and `/chat/completions`)
- 💬 Support for streaming and non-streaming modes
- 🔧 Use built-in XML prompts to increase compatibility and success rate of existing tool calls
- 🖼️ Support for file uploads in conversations (URL and Base64)
- 🌐 Complete handling of Events from the latest POE API
- 🤖 Support for Claude/Roo Code parsing, including token usage statistics
- 📊 Web admin interface (`/admin`) for model configuration (model mapping and editing models displayed in `/models`)
- 🔒 Rate limiting support to prevent excessive requests
- 📦 Built-in URL and Base64 image caching system to reduce duplicate uploads
- 🧠 Based on Deepseek OpenAI format, put the `Thinking...` reasoning content into `reasoning_content`
- 🎯 Support for advanced reasoning options (reasoning_effort, thinking, extra_body parameters)
- 🐳 Docker deployment support

## 🔧 Installation Guide
### Using Docker (Simple Deployment)
```bash
# Pull the image
docker pull jeromeleong/poe2openai:latest
# Run the container
docker run --name poe2openai -d \
  -p 8080:8080 \
  -e ADMIN_USERNAME=admin \
  -e ADMIN_PASSWORD=123456 \
  jeromeleong/poe2openai:latest
```

#### Data Persistence (Optional)
```bash
# Create local data directory
mkdir -p /path/to/data
# Run container with mounted data directory
docker run --name poe2openai -d \
  -p 8080:8080 \
  -v /path/to/data:/data \
  -e CONFIG_DIR=/data \
  -e ADMIN_USERNAME=admin \
  -e ADMIN_PASSWORD=123456 \
  jeromeleong/poe2openai:latest
### Using Docker Compose
Modify according to your personal requirements
```yaml
version: '3.8'
services:
  poe2openai:
    image: jeromeleong/poe2openai:latest
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
      - /path/to/data:/data
```

### Building from Source
```bash
# Clone the repository
git clone https://github.com/jeromeleong/poe2openai
cd poe2openai
# Build
cargo build --release
# Run
./target/release/poe2openai
```

## 🚀 Quick Start
1. Start the service using Docker:
```bash
docker run -d -p 8080:8080 jeromeleong/poe2openai:latest
```
2. The server starts by default at `http://localhost:8080`
3. Usage example:
```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer your-poe-token" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role": "user", "content": "Hello"}],
    "stream": true
  }'
```
4. You can manage models at `http://localhost:8080/admin`

## 📖 API Documentation
### Supported OpenAI API Endpoints
- `GET /v1/models` - Get list of available models
- `POST /v1/chat/completions` - Chat with POE models
- `GET /models` - Get list of available models (compatibility endpoint)
- `POST /chat/completions` - Chat with POE models (compatibility endpoint)

### Request Format
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

#### Optional Parameters
| Parameter     | Type     | Default      | Description                                          |
|---------------|----------|--------------|------------------------------------------------------|
| model         | string   | (required)   | Name of the model to request                         |
| messages      | array    | (required)   | List of chat messages, supports text or multimodal content (text+images) |
| temperature   | float    | null         | Exploration (0~2). Controls response diversity       |
| stream        | bool     | false        | Whether to stream the response (SSE)                 |
| tools         | array    | null         | Tool descriptions (Tool Calls) support               |
| logit_bias    | object   | null         | Token preference values in key-value format          |
| stop          | array    | null         | Array of sequences that stop text generation         |
| stream_options| object   | null         | Streaming options, supports include_usage (bool): whether to include usage statistics|
| reasoning_effort| string | null         | Reasoning effort level, options: low, medium, high   |
| thinking      | object   | null         | Thinking configuration, can set budget_tokens (0-30768): token budget for thinking phase|
| extra_body    | object   | null         | Additional request parameters, supports Google-specific configs like google.thinking_config.thinking_budget(0-30768)|

> Other OpenAI parameters like top_p, n, etc. are not currently supported and will be ignored if submitted.

### Response Format
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
        "content": "Response content",
        "reasoning_content": "Reasoning thought process"
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

### Multimodal Request Example
```json
{
  "model": "claude-3-opus",
  "messages": [
    {
      "role": "user",
      "content": [
        {
          "type": "text",
          "text": "What's in this image?"
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

## ⚙️ Configuration
Server configuration via environment variables:
- `PORT` - Server port (default: `8080`)
- `HOST` - Server host (default: `0.0.0.0`)
- `ADMIN_USERNAME` - Admin interface username (default: `admin`)
- `ADMIN_PASSWORD` - Admin interface password (default: `123456`)
- `MAX_REQUEST_SIZE` - Maximum request size (default: `1073741824`, 1GB)
- `LOG_LEVEL` - Log level (default: `info`, options: `debug`, `info`, `warn`, `error`)
- `CONFIG_DIR` - Configuration file directory (default in Docker: `/data`, default locally: `./`)
- `RATE_LIMIT_MS` - Global rate limit (milliseconds, default: `100`, set to `0` to disable)
- `URL_CACHE_TTL_SECONDS` - Poe CDN URL cache expiration period (seconds, default: `259200`, 3 days)
- `URL_CACHE_SIZE_MB` - Maximum Poe CDN URL cache capacity (MB, default: `100`)
- `POE_BASE_URL` - Poe API base URL (default: `https://api.poe.com`)
- `POE_FILE_UPLOAD_URL` - Poe file upload URL (default: `https://www.quora.com/poe_api/file_upload_3RD_PARTY_POST`)

## ❓ FAQ
### Q: How do I get a Poe API Token?
A: You need to subscribe to Poe first, then obtain it from the [Poe API Key](https://poe.com/api_key) page.

### Q: Why am I getting authentication errors?
A: Make sure you correctly set the `Authorization: Bearer your-poe-token` in the request headers.

### Q: Which models are supported?
A: All models available on the POE platform are supported. You can query them via the `/v1/models` endpoint.

### Q: How do I change the server port?
A: You can modify it by setting the `PORT` environment variable, for example:
```bash
docker run -d -e PORT=3000 -p 3000:3000 jeromeleong/poe2openai:latest
```

### Q: How do I configure models using models.yaml?
A: You can configure models in the admin interface at `/admin`, or manually edit the `models.yaml` file in the `CONFIG_DIR` directory.

### Q: How do I handle request rate limits?
A: You can control the request interval by setting the `RATE_LIMIT_MS` environment variable in milliseconds. Set to `0` to disable limits.

## 🤝 Contributing
All forms of contribution are welcome! If you find issues or have suggestions for improvements, please submit an Issue or Pull Request.

## 📄 License
This project is licensed under the [MIT License](LICENSE).

## 🌟 Star History
[![Star History Chart](https://api.star-history.com/svg?repos=jeromeleong/poe2openai&type=Date)](https://star-history.com/#jeromeleong/poe2openai&Date)