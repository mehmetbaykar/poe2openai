use crate::cache::{remove_config_sled, save_config_sled};
use crate::types::Config;
use crate::utils::{get_config_path, redact_headers, redact_json_fields, pretty_json_truncated};
use askama::Template;
use salvo::basic_auth::{BasicAuth, BasicAuthValidator};
use salvo::prelude::*;
use serde_json::json;
use std::fs;
use tracing::{info, debug, error};

#[derive(Template)]
#[template(path = "admin.html")]
struct AdminTemplate;

#[handler]
async fn admin_page(res: &mut Response) {
    let template = AdminTemplate;
    res.render(Text::Html(template.render().unwrap()));
}

#[handler]
async fn get_config(req: &mut Request, res: &mut Response) {
    // Structure request/response logging with separator
    debug!("------ Incoming Request [GET] {} ------", req.uri());
    
    // Log inbound request metadata with redacted headers
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let redacted_headers = redact_headers(req.headers());
    
    debug!("📋 Received config request | Method: {} | Path: {} | Headers: {:?}", 
        method, path, redacted_headers);
    
    invalidate_config_cache();
    let config = load_config().unwrap_or_default();
    
    // Log the response before rendering
    let response_value = serde_json::to_value(&config).unwrap_or_else(|_| json!(null));
    let redacted_response = redact_json_fields(&response_value);
    let pretty_response = pretty_json_truncated(&redacted_response, 64 * 1024);
    debug!("📤 Response body (sanitized, truncated):\n{}", pretty_response);
    
    debug!("------ Outgoing Response [200] /api/admin/config ------");
    
    res.render(Json(config));
}

#[handler]
async fn save_config(req: &mut Request, res: &mut Response) {
    // Structure request/response logging with separator
    debug!("------ Incoming Request [POST] {} ------", req.uri());
    
    // Log inbound request metadata with redacted headers
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let redacted_headers = redact_headers(req.headers());
    
    debug!("📋 Received config save request | Method: {} | Path: {} | Headers: {:?}", 
        method, path, redacted_headers);
    
    match req.parse_json::<Config>().await {
        Ok(config) => {
            // Log the incoming config (sanitized)
            let config_value = serde_json::to_value(&config).unwrap_or_else(|_| json!(null));
            let redacted_config = redact_json_fields(&config_value);
            let pretty_config = pretty_json_truncated(&redacted_config, 64 * 1024);
            debug!("📋 Incoming config (sanitized, truncated):\n{}", pretty_config);
            
            if let Err(e) = save_config_to_file(&config) {
                error!("❌ Failed to save config file: {}", e);
                
                // Log error response
                debug!("------ Outgoing Response [500] /api/admin/config ------");
                
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(Json(json!({ "error": e.to_string() })));
            } else {
                info!("✅ models.yaml saved successfully.");
                // Sync write to sled cache
                let _ = save_config_sled("models.yaml", &config);
                invalidate_config_cache();
                
                // Log success response
                debug!("------ Outgoing Response [200] /api/admin/config ------");
                
                res.render(Json(json!({ "status": "success" })));
            }
        }
        Err(e) => {
            error!("❌ Failed to parse config: {}", e);
            
            // Log error response
            debug!("------ Outgoing Response [400] /api/admin/config ------");
            
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(json!({ "error": e.to_string() })));
        }
    }
}

fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let config_path = get_config_path("models.yaml");
    if config_path.exists() {
        let contents = fs::read_to_string(config_path)?;
        match serde_yaml::from_str::<Config>(&contents) {
            Ok(mut config) => {
                // Ensure custom_models field exists
                if config.custom_models.is_none() {
                    config.custom_models = Some(Vec::new());
                }
                Ok(config)
            }
            Err(e) => Err(Box::new(e)),
        }
    } else {
        Ok(Config {
            enable: Some(false),
            models: std::collections::HashMap::new(),
            custom_models: Some(Vec::new()),
            api_token: None,
            use_v1_api: None,
        })
    }
}

fn save_config_to_file(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let yaml = serde_yaml::to_string(config)?;
    let config_path = get_config_path("models.yaml");
    fs::write(config_path, yaml)?;
    Ok(())
}

fn invalidate_config_cache() {
    info!("🗑️  Clearing models.yaml configuration cache...");
    remove_config_sled("models.yaml");
}

pub struct AdminAuthValidator;

impl BasicAuthValidator for AdminAuthValidator {
    async fn validate(&self, username: &str, password: &str, _depot: &mut Depot) -> bool {
        let valid_username =
            std::env::var("ADMIN_USERNAME").unwrap_or_else(|_| "admin".to_string());
        let valid_password =
            std::env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "123456".to_string());
        username == valid_username && password == valid_password
    }
}

pub fn admin_routes() -> Router {
    let auth_handler = BasicAuth::new(AdminAuthValidator);
    Router::new()
        .hoop(auth_handler) // Add authentication middleware
        .push(Router::with_path("admin").get(admin_page))
        .push(
            Router::with_path("api/admin/config")
                .get(get_config)
                .post(save_config),
        )
}
