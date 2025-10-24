use salvo::prelude::*;
use salvo::http::StatusCode;
use serde_json::Value;
use std::time::Instant;
use tracing::{Level, debug};

/// Request/Response logging middleware that logs all HTTP requests and responses
/// when LOG_LEVEL=debug is set
#[handler]
pub async fn request_response_logging(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    // Only log if debug level is enabled
    if !tracing::enabled!(Level::DEBUG) {
        ctrl.call_next(req, depot, res).await;
        return;
    }

    let start_time = Instant::now();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();
    let full_path = format!("{}{}", path, query);

    // Log incoming request with body
    log_incoming_request(req, &method, &full_path).await;

    // Continue with the request
    ctrl.call_next(req, depot, res).await;

    // Log outgoing response
    let duration = start_time.elapsed();
    log_outgoing_response(res, &method, &full_path, duration);
}

/// Log incoming request details including body
async fn log_incoming_request(req: &mut Request, method: &str, path: &str) {
    let mut log_lines = Vec::new();
    log_lines.push("╔════════════════ Incoming Request ════════════════╗".to_string());
    log_lines.push(format!("║ Method: {}", method));
    log_lines.push(format!("║ Path: {}", path));
    
    // Log headers (sanitized)
    log_lines.push("║ Headers:".to_string());
    for (name, value) in req.headers() {
        let header_name = name.as_str();
        let header_value = if is_sensitive_header(header_name) {
            "****** (hidden)".to_string()
        } else {
            value.to_str().unwrap_or("[invalid utf8]").to_string()
        };
        log_lines.push(format!("║   {}: {}", header_name, header_value));
    }

    // Log request body for POST/PUT/PATCH requests
    if method == "POST" || method == "PUT" || method == "PATCH" {
        if let Some(content_type) = req.headers().get("content-type") {
            let content_type_str = content_type.to_str().unwrap_or("");
            if content_type_str.contains("application/json") {
                // Read and log JSON body
                match req.parse_body::<String>().await {
                    Ok(body_str) => {
                        if !body_str.is_empty() {
                            log_lines.push("║ Body:".to_string());
                            let formatted_body = format_json_for_logging(&body_str, 5000);
                            let masked_body = mask_sensitive_json_values(&formatted_body);
                            for line in masked_body.lines() {
                                log_lines.push(format!("║   {}", line));
                            }
                        } else {
                            log_lines.push("║ Body: [Empty]".to_string());
                        }
                    }
                    Err(e) => {
                        log_lines.push(format!("║ Body: [Error reading body: {}]", e));
                    }
                }
            } else if content_type_str.contains("multipart/form-data") {
                log_lines.push("║ Body: [Multipart form data - not logged]".to_string());
            } else {
                log_lines.push(format!(
                    "║ Body: [Non-JSON content type: {}]",
                    content_type_str
                ));
            }
        } else {
            log_lines.push("║ Body: [No content-type header]".to_string());
        }
    }

    log_lines.push("╚═══════════════════════════════════════════════════╝".to_string());
    
    debug!("\n{}", log_lines.join("\n"));
}

/// Log outgoing response details
fn log_outgoing_response(res: &Response, method: &str, path: &str, duration: std::time::Duration) {
    let mut log_lines = Vec::new();
    log_lines.push("╔════════════════ Outgoing Response ═══════════════╗".to_string());
    log_lines.push(format!("║ Method: {}", method));
    log_lines.push(format!("║ Path: {}", path));
    let status = res.status_code.unwrap_or(StatusCode::OK);
    log_lines.push(format!("║ Status: {} {}", status.as_u16(), status.canonical_reason().unwrap_or("Unknown")));
    log_lines.push(format!("║ Duration: {:.3}s", duration.as_secs_f64()));
    
    // Log headers
    log_lines.push("║ Headers:".to_string());
    for (name, value) in res.headers() {
        let header_name = name.as_str();
        let header_value = if is_sensitive_header(header_name) {
            "****** (hidden)".to_string()
        } else {
            value.to_str().unwrap_or("[invalid utf8]").to_string()
        };
        log_lines.push(format!("║   {}: {}", header_name, header_value));
    }

    // Note: Response body logging is complex due to streaming and ownership
    // For now, we log at the handler level for detailed body inspection
    log_lines.push("║ Body: [See individual handler logs for response body details]".to_string());
    
    log_lines.push("╚═══════════════════════════════════════════════════╝".to_string());
    
    debug!("\n{}", log_lines.join("\n"));
}

/// Check if a header contains sensitive information that should be hidden
fn is_sensitive_header(header_name: &str) -> bool {
    let sensitive_headers = [
        "authorization",
        "cookie",
        "x-api-key",
        "x-auth-token",
        "x-access-token",
        "x-session-token",
        "admin-password",
        "admin-username",
        "set-cookie",
    ];
    
    sensitive_headers.contains(&header_name.to_lowercase().as_str())
}

/// Helper function to format JSON for logging (with truncation for large bodies)
pub fn format_json_for_logging(json_str: &str, max_length: usize) -> String {
    if json_str.len() <= max_length {
        // Try to pretty-print if it's valid JSON
        if let Ok(value) = serde_json::from_str::<Value>(json_str) {
            if let Ok(pretty) = serde_json::to_string_pretty(&value) {
                return pretty;
            }
        }
        return json_str.to_string();
    }

    // Truncate if too long
    let truncated = &json_str[..max_length.min(json_str.len())];
    format!(
        "{}... [truncated, {} total chars]",
        truncated,
        json_str.len()
    )
}

/// Helper function to mask sensitive values in JSON
pub fn mask_sensitive_json_values(json_str: &str) -> String {
    let sensitive_keys = [
        "password",
        "token",
        "key",
        "secret",
        "auth",
        "authorization",
        "api_key",
        "access_token",
        "refresh_token",
        "session_id",
        "cookie",
    ];

    match serde_json::from_str::<Value>(json_str) {
        Ok(mut value) => {
            mask_sensitive_values_recursive(&mut value, &sensitive_keys);
            serde_json::to_string_pretty(&value).unwrap_or(json_str.to_string())
        }
        Err(_) => json_str.to_string(),
    }
}

/// Recursively mask sensitive values in JSON
fn mask_sensitive_values_recursive(value: &mut Value, sensitive_keys: &[&str]) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                let key_lower = key.to_lowercase();
                if sensitive_keys
                    .iter()
                    .any(|&sensitive| key_lower.contains(sensitive))
                {
                    *val = Value::String("****** (hidden)".to_string());
                } else {
                    mask_sensitive_values_recursive(val, sensitive_keys);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                mask_sensitive_values_recursive(item, sensitive_keys);
            }
        }
        _ => {}
    }
}
