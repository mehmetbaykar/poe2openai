use crate::utils::{pretty_json_truncated, redact_headers, redact_json_fields};
use crate::{cache::get_cached_config, poe_client::PoeClientWrapper, types::*};
use chrono::Utc;
use poe_api_process::{ModelInfo, get_model_list};
use salvo::prelude::*;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

// Note: This cache does not apply to /api/models path
static API_MODELS_CACHE: RwLock<Option<Arc<Vec<ModelInfo>>>> = RwLock::const_new(None);

/// Get model list based on configuration
async fn get_models_from_api(config: &Config) -> Result<Vec<ModelInfo>, String> {
    let use_v1_api = config.use_v1_api.unwrap_or(false);

    if use_v1_api {
        // Use v1/models API
        if let Some(api_token) = &config.api_token {
            info!("🔄 Using v1/models API to get model list");
            let client = PoeClientWrapper::new("dummy", api_token);
            match client.get_v1_model_list().await {
                Ok(model_response) => {
                    let models = model_response
                        .data
                        .into_iter()
                        .map(|model| ModelInfo {
                            id: model.id.to_lowercase(),
                            object: model.object,
                            created: model.created,
                            owned_by: model.owned_by,
                        })
                        .collect();
                    Ok(models)
                }
                Err(e) => {
                    error!("❌ v1/models API request failed: {}", e);
                    Err(format!("v1/models API request failed: {}", e))
                }
            }
        } else {
            error!("❌ v1/models API configured but no api_token provided");
            Err("v1/models API configured but no api_token provided".to_string())
        }
    } else {
        // Use traditional get_model_list API
        info!("🔄 Using traditional get_model_list API to get model list");
        match get_model_list(Some("zh-Hant")).await {
            Ok(model_list) => {
                let models = model_list
                    .data
                    .into_iter()
                    .map(|mut model| {
                        model.id = model.id.to_lowercase();
                        model
                    })
                    .collect();
                Ok(models)
            }
            Err(e) => {
                error!("❌ get_model_list API request failed: {}", e);
                Err(format!("get_model_list API request failed: {}", e))
            }
        }
    }
}

#[handler]
pub async fn get_models(req: &mut Request, res: &mut Response) {
    let path = req.uri().path();

    // Structure request/response logging with separator
    debug!("------ Incoming Request [GET] {} ------", req.uri());

    // Log inbound request metadata with redacted headers
    let method = req.method().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let redacted_headers = redact_headers(req.headers());

    debug!(
        "📋 Received model list request | Method: {} | Path: {} | Query: {} | Headers: {:?}",
        method, path, query, redacted_headers
    );

    let start_time = Instant::now();

    // Handle /api/models special path (no cache) ---
    if path == "/api/models" {
        info!("⚡️ api/models path: Direct from Poe (no cache)");

        let config = get_cached_config().await;
        match get_models_from_api(&config).await {
            Ok(models) => {
                let models_arc = Arc::new(models);

                {
                    let mut cache_guard = API_MODELS_CACHE.write().await;
                    *cache_guard = Some(models_arc.clone());
                    info!("🔄 Updated API_MODELS_CACHE after /api/models request.");
                }

                let response = json!({
                    "object": "list",
                    "data": &*models_arc
                });

                // Log the response before rendering
                let response_value =
                    serde_json::to_value(&*models_arc).unwrap_or_else(|_| json!(null));
                let redacted_response = redact_json_fields(&response_value);
                let pretty_response = pretty_json_truncated(&redacted_response, 64 * 1024);
                debug!(
                    "📤 Response body (sanitized, truncated):\n{}",
                    pretty_response
                );

                debug!("------ Outgoing Response [200] /api/models ------");

                let duration = start_time.elapsed();
                info!(
                    "✅ [/api/models] Successfully retrieved unfiltered model list and updated cache | Model count: {} | Processing time: {}",
                    models_arc.len(),
                    crate::utils::format_duration(duration)
                );
                res.render(Json(response));
            }
            Err(e) => {
                let duration = start_time.elapsed();
                error!(
                    "❌ [/api/models] Failed to get model list | Error: {} | Duration: {}",
                    e,
                    crate::utils::format_duration(duration)
                );
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(Json(json!({ "error": e })));
            }
        }
        return;
    }

    let config = get_cached_config().await;

    let is_enabled = config.enable.unwrap_or(false);
    debug!(
        "🔍 Configuration enable status (from cache): {}",
        is_enabled
    );

    let yaml_config_map: std::collections::HashMap<String, ModelConfig> = config
        .models
        .clone() // Clone HashMap from Arc<Config>
        .into_iter()
        .map(|(k, v)| (k.to_lowercase(), v))
        .collect();

    if is_enabled {
        info!("⚙️ Merging cached Poe API list with models.yaml (enabled)");

        let api_models_data_arc: Arc<Vec<ModelInfo>>;

        let read_guard = API_MODELS_CACHE.read().await;
        if let Some(cached_data) = &*read_guard {
            // Cache hit
            debug!("✅ Model cache hit.");
            api_models_data_arc = cached_data.clone();
            drop(read_guard);
        } else {
            // Cache miss
            debug!("❌ Model cache miss. Attempting to populate...");
            drop(read_guard);

            let mut write_guard = API_MODELS_CACHE.write().await;
            // Check again to prevent another thread from filling cache during write lock acquisition
            if let Some(cached_data) = &*write_guard {
                debug!(
                    "✅ API model cache populated by another thread while waiting for write lock."
                );
                api_models_data_arc = cached_data.clone();
            } else {
                // Cache is indeed empty, get data from API
                info!("⏳ Getting models from API to populate cache...");
                match get_models_from_api(&config).await {
                    Ok(models) => {
                        let new_data = Arc::new(models);
                        *write_guard = Some(new_data.clone());
                        api_models_data_arc = new_data;
                        info!("✅ API models cache populated successfully.");
                    }
                    Err(e) => {
                        // If cache population fails, return error
                        let duration = start_time.elapsed(); // Calculate duration
                        error!(
                            "❌ Failed to populate API models cache: {} | Duration: {}.",
                            e,
                            crate::utils::format_duration(duration) // Use duration in log
                        );
                        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                        res.render(Json(
                            json!({ "error": format!("Failed to retrieve model list to populate cache: {}", e) }),
                        ));
                        drop(write_guard);
                        return;
                    }
                }
            }
            drop(write_guard);
        }

        let mut api_model_ids: HashSet<String> = HashSet::new();
        for model_ref in api_models_data_arc.iter() {
            api_model_ids.insert(model_ref.id.to_lowercase());
        }

        let mut processed_models_enabled: Vec<ModelInfo> = Vec::new();

        for api_model_ref in api_models_data_arc.iter() {
            let api_model_id_lower = api_model_ref.id.to_lowercase();
            match yaml_config_map.get(&api_model_id_lower) {
                Some(yaml_config) => {
                    // Found in YAML: check if enabled, if enabled apply mapping
                    if yaml_config.enable.unwrap_or(true) {
                        let final_id = if let Some(mapping) = &yaml_config.mapping {
                            let new_id = mapping.to_lowercase();
                            debug!(
                                "🔄 API model renamed (YAML enabled): {} -> {}",
                                api_model_id_lower, new_id
                            );
                            new_id
                        } else {
                            debug!(
                                "✅ Keep API model (YAML enabled, no mapping): {}",
                                api_model_id_lower
                            );
                            api_model_id_lower.clone()
                        };
                        processed_models_enabled.push(ModelInfo {
                            id: final_id,
                            object: api_model_ref.object.clone(),
                            created: api_model_ref.created,
                            owned_by: api_model_ref.owned_by.clone(),
                        });
                    } else {
                        debug!(
                            "❌ Exclude API model (YAML disabled): {}",
                            api_model_id_lower
                        );
                    }
                }
                None => {
                    debug!("✅ Keep API model (not in YAML): {}", api_model_id_lower);
                    processed_models_enabled.push(ModelInfo {
                        id: api_model_id_lower.clone(),
                        object: api_model_ref.object.clone(),
                        created: api_model_ref.created,
                        owned_by: api_model_ref.owned_by.clone(),
                    });
                }
            }
        }

        // Process custom models, adding them to the processed model list
        if let Some(custom_models) = &config.custom_models {
            if !custom_models.is_empty() {
                info!(
                    "📋 Processing custom models | Count: {}",
                    custom_models.len()
                );
                for custom_model in custom_models {
                    let model_id = custom_model.id.to_lowercase();
                    // Check if this ID already exists in processed models
                    if !processed_models_enabled.iter().any(|m| m.id == model_id) {
                        // Check if configured with enable: false in yaml_config_map
                        if let Some(yaml_config) = yaml_config_map.get(&model_id) {
                            if yaml_config.enable == Some(false) {
                                debug!("❌ Exclude custom model (YAML disabled): {}", model_id);
                                continue;
                            }
                        }

                        debug!("➕ Add custom model: {}", model_id);
                        processed_models_enabled.push(ModelInfo {
                            id: model_id,
                            object: "model".to_string(),
                            created: custom_model
                                .created
                                .unwrap_or_else(|| Utc::now().timestamp()),
                            owned_by: custom_model
                                .owned_by
                                .clone()
                                .unwrap_or_else(|| "poe".to_string()),
                        });
                    }
                }
            }
        }

        let response = json!({
            "object": "list",
            "data": processed_models_enabled
        });

        // Log the response before rendering
        let response_value = serde_json::to_value(&response).unwrap_or_else(|_| json!(null));
        let redacted_response = redact_json_fields(&response_value);
        let pretty_response = pretty_json_truncated(&redacted_response, 64 * 1024);
        debug!(
            "📤 Response body (sanitized, truncated):\n{}",
            pretty_response
        );

        debug!("------ Outgoing Response [200] /models ------");

        let duration = start_time.elapsed();
        info!(
            "✅ Successfully retrieved processed model list | Source: {} | Model count: {} | Processing time: {}",
            "YAML + Cached API",
            processed_models_enabled.len(),
            crate::utils::format_duration(duration)
        );

        res.render(Json(response));
    } else {
        info!(
            "🔌 YAML disabled, directly get model list from Poe API (no cache, no YAML rules)..."
        );

        match get_models_from_api(&config).await {
            Ok(models) => {
                let response = json!({
                    "object": "list",
                    "data": models
                });

                // Log the response before rendering
                let response_value =
                    serde_json::to_value(&response).unwrap_or_else(|_| json!(null));
                let redacted_response = redact_json_fields(&response_value);
                let pretty_response = pretty_json_truncated(&redacted_response, 64 * 1024);
                debug!(
                    "📤 Response body (sanitized, truncated):\n{}",
                    pretty_response
                );

                debug!("------ Outgoing Response [200] /models ------");

                let duration = start_time.elapsed();
                info!(
                    "✅ [Direct Poe] Successfully directly retrieved model list | Model count: {} | Processing time: {}",
                    models.len(),
                    crate::utils::format_duration(duration)
                );
                res.render(Json(response));
            }
            Err(e) => {
                let duration = start_time.elapsed();
                error!(
                    "❌ [Direct Poe] Directly get model list failed | Error: {} | Duration: {}",
                    e,
                    crate::utils::format_duration(duration)
                );
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(Json(
                    json!({ "error": format!("Failed to directly get models from API: {}", e) }),
                ));
            }
        }
    }
}
