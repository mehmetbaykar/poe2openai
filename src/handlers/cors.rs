use salvo::http::{HeaderValue, Method, StatusCode, header};
use salvo::prelude::*;
use tracing::{debug, info};

/// 检查头部是否安全
fn is_safe_header(header: &str) -> bool {
    let header_lower = header.trim().to_lowercase();

    // 排除空字符串
    if header_lower.is_empty() {
        return false;
    }

    // 黑名單：明確的惡意頭部
    if matches!(header_lower.as_str(), "cookie" | "set-cookie") {
        return false;
    }

    // 白名單：允許的頭部模式
    // 1. X-開頭的自定義頭部（如X-Stainless-*）
    // 2. 標準HTTP頭部
    header_lower.starts_with("x-")
        || matches!(
            header_lower.as_str(),
            "accept"
                | "accept-encoding"
                | "accept-language"
                | "authorization"
                | "cache-control"
                | "connection"
                | "content-type"
                | "user-agent"
                | "referer"
                | "origin"
                | "pragma"
                | "sec-fetch-dest"
                | "sec-fetch-mode"
                | "sec-fetch-site"
        )
}

/// 解析客戶端請求的頭部並進行安全過濾
fn parse_requested_headers(req: &Request) -> Vec<String> {
    req.headers()
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|h| h.to_str().ok())
        .map(|headers_str| {
            headers_str
                .split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty() && is_safe_header(h))
                .collect()
        })
        .unwrap_or_default()
}

#[handler]
pub async fn cors_middleware(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    // 從請求中獲取Origin頭
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("null");

    // 記錄請求的Origin用於調試
    debug!("📡 接收到來自Origin: {} 的請求", origin);

    // 設置CORS頭部
    match HeaderValue::from_str(origin) {
        Ok(origin_value) => {
            res.headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin_value);
        }
        Err(e) => {
            debug!("⚠️ 無效的Origin頭: {}, 錯誤: {}", origin, e);
            res.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("null"),
            );
        }
    }

    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );

    // 為所有回應添加Vary頭，表明回應基於Origin頭變化
    res.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Origin"));

    // 如果是OPTIONS請求，直接處理並停止後續流程
    if req.method() == Method::OPTIONS {
        handle_preflight_request(req, res);
        ctrl.skip_rest();
    } else {
        // 非OPTIONS請求，繼續正常流程
        ctrl.call_next(req, depot, res).await;
    }
}

/// 專門處理CORS預檢請求
fn handle_preflight_request(req: &Request, res: &mut Response) {
    info!("🔍 處理OPTIONS預檢請求: {}", req.uri());

    // 設置CORS預檢回應的標準頭部
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS, PUT, DELETE, PATCH, HEAD"),
    );

    // 基礎硬編碼頭部（保持向後兼容）
    let base_headers = vec![
        "Authorization",
        "Content-Type",
        "User-Agent",
        "Accept",
        "Origin",
        "X-Requested-With",
        "Access-Control-Request-Method",
        "Access-Control-Request-Headers",
        "Accept-Encoding",
        "Accept-Language",
        "Cache-Control",
        "Connection",
        "Referer",
        "Sec-Fetch-Dest",
        "Sec-Fetch-Mode",
        "Sec-Fetch-Site",
        "Pragma",
        "X-Api-Key",
    ];

    // 解析客戶端請求的動態頭部
    let dynamic_headers = parse_requested_headers(req);

    // 合併基礎頭部和動態頭部
    let mut all_headers = base_headers.clone();
    for header in &dynamic_headers {
        if !all_headers
            .iter()
            .any(|h| h.to_lowercase() == header.to_lowercase())
        {
            all_headers.push(header);
        }
    }

    // 構建最終的頭部字符串
    let headers_str = all_headers.join(", ");

    // 記錄調試信息
    if !dynamic_headers.is_empty() {
        info!("➕ 動態添加的頭部: {:?}", dynamic_headers);
    }
    info!("📋 最終允許的頭部: {}", headers_str);

    // 設置 Access-Control-Allow-Headers
    match HeaderValue::from_str(&headers_str) {
        Ok(headers_value) => {
            res.headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_HEADERS, headers_value);
        }
        Err(e) => {
            // 降級處理：如果動態頭部有問題，使用基礎頭部
            debug!("⚠️ 動態頭部設置失敗: {}, 使用基礎頭部", e);
            res.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static(
                    "Authorization, Content-Type, User-Agent, Accept, Origin, \
                    X-Requested-With, Access-Control-Request-Method, \
                    Access-Control-Request-Headers, Accept-Encoding, Accept-Language, \
                    Cache-Control, Connection, Referer, Sec-Fetch-Dest, Sec-Fetch-Mode, \
                    Sec-Fetch-Site, Pragma, X-Api-Key",
                ),
            );
        }
    }

    res.headers_mut().insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("3600"),
    );

    // 添加Vary頭，表明回應會根據這些請求頭變化
    res.headers_mut().insert(
        header::VARY,
        HeaderValue::from_static("Access-Control-Request-Method, Access-Control-Request-Headers"),
    );

    // 設置正確的狀態碼: 204 No Content
    res.status_code(StatusCode::NO_CONTENT);
}
