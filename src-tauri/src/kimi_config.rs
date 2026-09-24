use crate::app_config::MultiAppConfig;
use crate::error::AppError;
use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use toml_edit::{DocumentMut, Table};

pub const DEFAULT_KIMI_CONFIG_DIR: &str = ".kimi-code";
pub const KIMI_HOME_ENV: &str = "KIMI_CODE_HOME";
pub const KIMI_CONFIG_FILE: &str = "config.toml";

/// 解析 Kimi Code 根目录路径（遵循 KIMI_CODE_HOME 环境变量，默认 ~/.kimi-code）
pub fn get_kimi_config_dir() -> PathBuf {
    if let Some(override_dir) = crate::settings::get_kimi_override_dir() {
        return override_dir;
    }
    if let Some(env_val) = std::env::var_os(KIMI_HOME_ENV) {
        if !env_val.is_empty() {
            return PathBuf::from(env_val);
        }
    }
    #[cfg(test)]
    {
        // 单元测试未显式设置 KIMI_CODE_HOME 时，绝不能回退到宿主真实目录，防止测试副作用篡改真实凭据
        std::env::temp_dir().join("cc-switch-kimi-test-isolated")
    }
    #[cfg(not(test))]
    {
        dirs::home_dir()
            .map(|p| p.join(DEFAULT_KIMI_CONFIG_DIR))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_KIMI_CONFIG_DIR))
    }
}

/// 获取 Kimi Code 的主配置文件路径 (~/.kimi-code/config.toml)
pub fn get_kimi_config_path() -> PathBuf {
    get_kimi_config_dir().join(KIMI_CONFIG_FILE)
}

/// 获取 Kimi Code 的 MCP 配置文件路径 (~/.kimi-code/mcp.json)
pub fn get_kimi_mcp_path() -> PathBuf {
    get_kimi_config_dir().join("mcp.json")
}

/// 获取 Kimi Code 的 Skills 存放目录 (~/.kimi-code/skills)
pub fn get_kimi_skills_dir() -> PathBuf {
    get_kimi_config_dir().join("skills")
}

/// 获取 Kimi Code 的全局指令文件路径 (~/.kimi-code/AGENTS.md)
pub fn get_kimi_agents_md_path() -> PathBuf {
    get_kimi_config_dir().join("AGENTS.md")
}

pub fn kimi_write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn read_kimi_config_source() -> Result<Option<String>, AppError> {
    let path = get_kimi_config_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
    Ok(Some(content))
}

pub fn write_kimi_config_source(source: &str) -> Result<(), AppError> {
    let path = get_kimi_config_path();
    write_file_atomic(&path, source, 0o644).map_err(|e| AppError::Message(e.to_string()))
}

pub fn read_kimi_config_json() -> Result<Value, AppError> {
    let source = read_kimi_config_source()?.unwrap_or_default();
    if source.trim().is_empty() {
        return Ok(json!({}));
    }
    let toml_val: toml::Value = toml::from_str(&source)
        .map_err(|e| AppError::Config(format!("Failed to parse Kimi config.toml: {e}")))?;
    let json_val = serde_json::to_value(toml_val)
        .map_err(|e| AppError::Config(format!("Failed to convert Kimi config to JSON: {e}")))?;
    Ok(json_val)
}

/// 读取 live config.toml 中配置的所有 providers，转化为统一的 Provider settings_config 格式
pub fn get_providers() -> Result<IndexMap<String, Value>, AppError> {
    let path = get_kimi_config_path();
    if !path.exists() {
        return Ok(IndexMap::new());
    }
    let content = fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
    if content.trim().is_empty() {
        return Ok(IndexMap::new());
    }
    let doc = content
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Config(format!("Failed to parse Kimi config.toml: {e}")))?;

    let Some(providers_tbl) = doc.get("providers").and_then(|v| v.as_table_like()) else {
        return Ok(IndexMap::new());
    };

    let default_model_opt = doc.get("default_model").and_then(|v| v.as_str());

    let mut result = IndexMap::new();

    for (p_id, p_item) in providers_tbl.iter() {
        let Some(p_tbl) = p_item.as_table_like() else {
            continue;
        };

        let p_type = p_tbl
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("openai");
        let base_url = p_tbl
            .get("base_url")
            .or_else(|| p_tbl.get("baseUrl"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let api_key = p_tbl
            .get("api_key")
            .or_else(|| p_tbl.get("apiKey"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Find models pointing to this provider
        let mut models = Vec::new();
        if let Some(models_tbl) = doc.get("models").and_then(|v| v.as_table_like()) {
            for (m_name, m_item) in models_tbl.iter() {
                if let Some(m_tbl) = m_item.as_table_like() {
                    if m_tbl.get("provider").and_then(|p| p.as_str()) == Some(p_id) {
                        let display_name = m_tbl
                            .get("display_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(m_name);
                        models.push(json!({
                            "id": m_name,
                            "name": display_name,
                        }));
                    }
                }
            }
        }

        // Determine primary model
        let primary_model = if let Some(def_m) = default_model_opt {
            if models.iter().any(|m| m.get("id").and_then(Value::as_str) == Some(def_m)) {
                Some(def_m.to_string())
            } else {
                models
                    .first()
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            }
        } else {
            models
                .first()
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        };

        let mut obj = serde_json::Map::new();
        obj.insert("name".to_string(), json!(p_id));
        obj.insert("type".to_string(), json!(p_type));
        if !base_url.is_empty() {
            obj.insert("baseUrl".to_string(), json!(base_url));
            obj.insert("base_url".to_string(), json!(base_url));
        }
        if !api_key.is_empty() {
            obj.insert("apiKey".to_string(), json!(api_key));
            obj.insert("api_key".to_string(), json!(api_key));
        }
        if let Some(model) = primary_model {
            obj.insert("model".to_string(), json!(model));
        }
        if !models.is_empty() {
            obj.insert("models".to_string(), Value::Array(models));
        }

        result.insert(p_id.to_string(), Value::Object(obj));
    }

    Ok(result)
}

pub fn get_provider(id: &str) -> Result<Option<Value>, AppError> {
    Ok(get_providers()?.get(id).cloned())
}

/// 准备将 provider 变更写入 Kimi 的 config.toml，返回更新后的 TOML 文本
pub fn prepare_provider(id: &str, provider_config: Value) -> Result<String, AppError> {
    let path = get_kimi_config_path();
    let content = if path.exists() {
        fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?
    } else {
        String::new()
    };
    let mut doc = if content.trim().is_empty() {
        DocumentMut::new()
    } else {
        content
            .parse::<DocumentMut>()
            .map_err(|e| AppError::Config(format!("Failed to parse Kimi config.toml: {e}")))?
    };

    if doc.get("providers").is_none() {
        doc["providers"] = toml_edit::Item::Table(Table::new());
    }
    let providers = doc["providers"].as_table_like_mut().ok_or_else(|| {
        AppError::Config("Kimi config.toml [providers] is not a table".into())
    })?;

    if providers.get(id).is_none() {
        providers.insert(id, toml_edit::Item::Table(Table::new()));
    }
    let provider_tbl = providers
        .get_mut(id)
        .and_then(|v| v.as_table_like_mut())
        .ok_or_else(|| AppError::Config(format!("Kimi provider table '{id}' is invalid")))?;

    let p_type = provider_config
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("openai");
    provider_tbl.insert("type", toml_edit::value(p_type));

    let base_url = provider_config
        .get("baseUrl")
        .or_else(|| provider_config.get("base_url"))
        .or_else(|| provider_config.get("endpoint"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(url) = base_url {
        provider_tbl.insert("base_url", toml_edit::value(url));
    }

    let api_key = provider_config
        .get("apiKey")
        .or_else(|| provider_config.get("api_key"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(key) = api_key {
        provider_tbl.insert("api_key", toml_edit::value(key));
    } else if provider_config.get("api_key").is_some() || provider_config.get("apiKey").is_some() {
        provider_tbl.remove("api_key");
    }

    // Handle models
    let mut configured_models = Vec::new();
    if let Some(model_str) = provider_config.get("model").and_then(|v| v.as_str()) {
        let m = model_str.trim();
        if !m.is_empty() {
            configured_models.push(m.to_string());
        }
    }

    if let Some(models_arr) = provider_config.get("models").and_then(|v| v.as_array()) {
        for item in models_arr {
            if let Some(m_id) = item
                .get("id")
                .and_then(|v| v.as_str())
                .or_else(|| item.as_str())
            {
                let m = m_id.trim();
                if !m.is_empty() && !configured_models.contains(&m.to_string()) {
                    configured_models.push(m.to_string());
                }
            }
        }
    }

    if !configured_models.is_empty() {
        if doc.get("models").is_none() {
            doc["models"] = toml_edit::Item::Table(Table::new());
        }
        if let Some(models) = doc["models"].as_table_like_mut() {
            for m in &configured_models {
                if models.get(m).is_none() {
                    let mut tbl = Table::new();
                    tbl.insert("provider", toml_edit::value(id));
                    tbl.insert("model", toml_edit::value(m.as_str()));
                    models.insert(m, toml_edit::Item::Table(tbl));
                } else if let Some(tbl) = models.get_mut(m).and_then(|v| v.as_table_like_mut()) {
                    tbl.insert("provider", toml_edit::value(id));
                    tbl.insert("model", toml_edit::value(m.as_str()));
                }
            }
        }

        if doc.get("default_model").is_none() {
            doc["default_model"] = toml_edit::value(&configured_models[0]);
        }
    }

    Ok(doc.to_string())
}

/// 将准备好的配置内容原子写入 Kimi 的 config.toml
pub fn write_prepared_config(content: &str) -> Result<(), AppError> {
    let _guard = kimi_write_lock()
        .lock()
        .map_err(|_| AppError::Message("Kimi write lock poisoned".into()))?;
    let path = get_kimi_config_path();
    write_file_atomic(&path, content, 0o644).map_err(|e| AppError::Message(e.to_string()))
}

/// 从 config.toml 中移除指定 provider 及其关联的 models
pub fn remove_provider(id: &str) -> Result<(), AppError> {
    let _guard = kimi_write_lock()
        .lock()
        .map_err(|_| AppError::Message("Kimi write lock poisoned".into()))?;
    let path = get_kimi_config_path();
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
    let mut doc = content
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Config(format!("Failed to parse Kimi config.toml: {e}")))?;

    if let Some(providers) = doc.get_mut("providers").and_then(|v| v.as_table_like_mut()) {
        providers.remove(id);
    }

    let mut removed_models = HashSet::new();
    if let Some(models) = doc.get_mut("models").and_then(|v| v.as_table_like_mut()) {
        let to_remove: Vec<String> = models
            .iter()
            .filter_map(|(m_name, m_item)| {
                if let Some(tbl) = m_item.as_table_like() {
                    if tbl.get("provider").and_then(|p| p.as_str()) == Some(id) {
                        return Some(m_name.to_string());
                    }
                }
                None
            })
            .collect();
        for m in to_remove {
            removed_models.insert(m.clone());
            models.remove(&m);
        }
    }

    if let Some(def_m) = doc.get("default_model").and_then(|v| v.as_str()) {
        if removed_models.contains(def_m) {
            let next_model = doc
                .get("models")
                .and_then(|m| m.as_table_like())
                .and_then(|m| m.iter().next().map(|(k, _)| k.to_string()));
            if let Some(next) = next_model {
                doc["default_model"] = toml_edit::value(next);
            } else {
                doc.as_table_mut().remove("default_model");
            }
        }
    }

    write_file_atomic(&path, &doc.to_string(), 0o644).map_err(|e| AppError::Message(e.to_string()))
}

/// 获取当前激活的 provider ID（根据 default_model 追溯）
pub fn get_current_provider_id() -> Result<Option<String>, AppError> {
    let path = get_kimi_config_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
    let doc = content
        .parse::<DocumentMut>()
        .map_err(|e| AppError::Config(format!("Failed to parse Kimi config.toml: {e}")))?;
    let Some(default_model) = doc.get("default_model").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    if let Some(models) = doc.get("models").and_then(|m| m.as_table_like()) {
        if let Some(target) = models.get(default_model).and_then(|m| m.as_table_like()) {
            if let Some(provider_id) = target.get("provider").and_then(|p| p.as_str()) {
                return Ok(Some(provider_id.to_string()));
            }
        }
    }
    Ok(None)
}

/// 切换当前激活的 provider，并将其设为 default_model
pub fn set_current_provider(id: &str, provider_config: &Value) -> Result<(), AppError> {
    let _guard = kimi_write_lock()
        .lock()
        .map_err(|_| AppError::Message("Kimi write lock poisoned".into()))?;
    let path = get_kimi_config_path();
    let content = if path.exists() {
        fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?
    } else {
        String::new()
    };
    let mut doc = if content.trim().is_empty() {
        DocumentMut::new()
    } else {
        content
            .parse::<DocumentMut>()
            .map_err(|e| AppError::Config(format!("Failed to parse Kimi config.toml: {e}")))?
    };

    let target_model = provider_config
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            doc.get("models")
                .and_then(|m| m.as_table_like())
                .and_then(|models| {
                    models.iter().find_map(|(m_name, m_item)| {
                        if let Some(tbl) = m_item.as_table_like() {
                            if tbl.get("provider").and_then(|p| p.as_str()) == Some(id) {
                                return Some(m_name.to_string());
                            }
                        }
                        None
                    })
                })
        })
        .unwrap_or_else(|| id.to_string());

    if doc.get("models").is_none() {
        doc["models"] = toml_edit::Item::Table(Table::new());
    }
    if let Some(models) = doc["models"].as_table_like_mut() {
        if models.get(&target_model).is_none() {
            let mut tbl = Table::new();
            tbl.insert("provider", toml_edit::value(id));
            tbl.insert("model", toml_edit::value(&target_model));
            models.insert(&target_model, toml_edit::Item::Table(tbl));
        }
    }

    doc["default_model"] = toml_edit::value(&target_model);
    write_file_atomic(&path, &doc.to_string(), 0o644).map_err(|e| AppError::Message(e.to_string()))
}

/// 设置默认模型
pub fn set_default_model(model_name: &str) -> Result<String, AppError> {
    let _guard = kimi_write_lock()
        .lock()
        .map_err(|_| AppError::Message("Kimi write lock poisoned".into()))?;
    let path = get_kimi_config_path();
    let content = if path.exists() {
        fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?
    } else {
        String::new()
    };
    let mut doc = if content.trim().is_empty() {
        DocumentMut::new()
    } else {
        content
            .parse::<DocumentMut>()
            .map_err(|e| AppError::Config(format!("Failed to parse Kimi config.toml: {e}")))?
    };
    doc["default_model"] = toml_edit::value(model_name);
    write_file_atomic(&path, &doc.to_string(), 0o644).map_err(|e| AppError::Message(e.to_string()))?;
    Ok(model_name.to_string())
}

/// 读取 Kimi MCP 服务器字典 (~/.kimi-code/mcp.json)
pub fn read_kimi_mcp_servers_map() -> Result<HashMap<String, Value>, AppError> {
    let path = get_kimi_mcp_path();
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
    let val: Value = serde_json::from_str(&content).map_err(|e| AppError::json(&path, e))?;
    let servers = val
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    Ok(servers)
}

/// 写入 Kimi MCP 服务器字典
pub fn set_kimi_mcp_servers_map(servers: &HashMap<String, Value>) -> Result<(), AppError> {
    let path = get_kimi_mcp_path();
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Message("Invalid mcp path".into()))?;
    fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;

    let mut root_obj = if path.exists() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    let servers_val = serde_json::to_value(servers)
        .map_err(|e| AppError::Message(format!("Failed to serialize MCP servers: {e}")))?;
    root_obj.insert("mcpServers".to_string(), servers_val);

    let formatted = serde_json::to_string_pretty(&Value::Object(root_obj))
        .map_err(|e| AppError::Message(format!("Failed to format MCP JSON: {e}")))?;
    write_file_atomic(&path, &formatted, 0o644).map_err(|e| AppError::Message(e.to_string()))
}

/// 同步单个 MCP 服务器到 Kimi live 配置
pub fn sync_single_server_to_kimi(
    _config: &MultiAppConfig,
    id: &str,
    server_spec: &Value,
) -> Result<(), AppError> {
    if !crate::sync_policy::should_sync_live(&crate::app_config::AppType::Kimi) {
        return Ok(());
    }
    let mut servers = read_kimi_mcp_servers_map()?;
    servers.insert(id.to_string(), server_spec.clone());
    set_kimi_mcp_servers_map(&servers)
}

/// 从 Kimi live 配置中移除单个 MCP 服务器
pub fn remove_server_from_kimi(id: &str) -> Result<(), AppError> {
    if !crate::sync_policy::should_sync_live(&crate::app_config::AppType::Kimi) {
        return Ok(());
    }
    let mut servers = read_kimi_mcp_servers_map()?;
    servers.remove(id);
    set_kimi_mcp_servers_map(&servers)
}

fn write_file_atomic(path: &Path, content: &str, #[allow(unused_variables)] mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无效的路径: {}", path.display()))?;

    fs::create_dir_all(parent)
        .with_context(|| format!("创建目录失败: {}", parent.display()))?;

    let filename = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("无效的文件名: {}", path.display()))?
        .to_string_lossy();

    let temp_path = parent.join(format!(
        ".{filename}.tmp.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp_path)
        .with_context(|| format!("创建临时文件失败: {}", temp_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }

    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);

    fs::rename(&temp_path, path).with_context(|| {
        let _ = fs::remove_file(&temp_path);
        format!("重命名临时文件到目标文件失败: {}", path.display())
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kimi_provider_crud_and_mcp() {
        let _lock = crate::test_support::lock_test_home_and_settings();
        let temp_home = tempfile::tempdir().unwrap();
        let old_home_env = std::env::var_os(KIMI_HOME_ENV);
        std::env::set_var(KIMI_HOME_ENV, temp_home.path());

        // 1. Initial get_providers on empty directory
        let providers = get_providers().unwrap();
        assert!(providers.is_empty());

        // 2. Prepare and write cortex provider
        let provider_config = json!({
            "type": "openai",
            "baseUrl": "https://cortex.botiverse.dev/v1",
            "apiKey": "sk-cortex-secret",
            "model": "devin/swe-2",
            "models": [
                { "id": "devin/swe-2", "name": "Devin SWE-2" },
                { "id": "k3", "name": "K3" }
            ]
        });
        let prepared = prepare_provider("cortex", provider_config).unwrap();
        write_prepared_config(&prepared).unwrap();

        // Verify providers read back
        let providers = get_providers().unwrap();
        assert_eq!(providers.len(), 1);
        let cortex = providers.get("cortex").unwrap();
        assert_eq!(cortex["name"], "cortex");
        assert_eq!(cortex["type"], "openai");
        assert_eq!(cortex["baseUrl"], "https://cortex.botiverse.dev/v1");
        assert_eq!(cortex["apiKey"], "sk-cortex-secret");
        assert_eq!(cortex["model"], "devin/swe-2");

        // Verify active provider
        let current_id = get_current_provider_id().unwrap();
        assert_eq!(current_id, Some("cortex".to_string()));

        // 3. Add second provider (openrouter)
        let or_config = json!({
            "type": "openai",
            "baseUrl": "https://openrouter.ai/api/v1",
            "apiKey": "sk-or-test",
            "model": "anthropic/claude-3.5-sonnet"
        });
        let prepared2 = prepare_provider("openrouter", or_config.clone()).unwrap();
        write_prepared_config(&prepared2).unwrap();

        let providers = get_providers().unwrap();
        assert_eq!(providers.len(), 2);

        // Switch to openrouter
        set_current_provider("openrouter", &or_config).unwrap();
        assert_eq!(get_current_provider_id().unwrap(), Some("openrouter".to_string()));

        // 4. Remove cortex
        remove_provider("cortex").unwrap();
        let providers = get_providers().unwrap();
        assert_eq!(providers.len(), 1);
        assert!(!providers.contains_key("cortex"));
        assert!(providers.contains_key("openrouter"));
        assert_eq!(get_current_provider_id().unwrap(), Some("openrouter".to_string()));

        // 5. MCP sync
        let dummy_cfg = MultiAppConfig::default();
        let server_spec = json!({
            "command": "npx",
            "args": ["-y", "@modelcontextprotocol/server-filesystem"]
        });
        sync_single_server_to_kimi(&dummy_cfg, "filesystem", &server_spec).unwrap();
        let mcp_map = read_kimi_mcp_servers_map().unwrap();
        assert_eq!(mcp_map.len(), 1);
        assert!(mcp_map.contains_key("filesystem"));

        remove_server_from_kimi("filesystem").unwrap();
        let mcp_map = read_kimi_mcp_servers_map().unwrap();
        assert!(mcp_map.is_empty());

        if let Some(val) = old_home_env {
            std::env::set_var(KIMI_HOME_ENV, val);
        } else {
            std::env::remove_var(KIMI_HOME_ENV);
        }
    }

    #[test]
    fn test_kimi_set_default_model() {
        let _lock = crate::test_support::lock_test_home_and_settings();
        let temp_home = tempfile::tempdir().unwrap();
        let old_home_env = std::env::var_os(KIMI_HOME_ENV);
        std::env::set_var(KIMI_HOME_ENV, temp_home.path());

        let provider_config = json!({
            "type": "openai",
            "baseUrl": "https://api.moonshot.cn/v1",
            "apiKey": "sk-test",
            "model": "moonshot-v1-8k",
            "models": [
                { "id": "moonshot-v1-8k", "name": "Moonshot 8k" },
                { "id": "moonshot-v1-32k", "name": "Moonshot 32k" }
            ]
        });
        let prepared = prepare_provider("moonshot", provider_config).unwrap();
        write_prepared_config(&prepared).unwrap();

        assert_eq!(get_current_provider_id().unwrap(), Some("moonshot".to_string()));

        set_default_model("moonshot-v1-32k").unwrap();
        assert_eq!(get_current_provider_id().unwrap(), Some("moonshot".to_string()));

        let json = read_kimi_config_json().unwrap();
        assert_eq!(json["default_model"], "moonshot-v1-32k");

        if let Some(val) = old_home_env {
            std::env::set_var(KIMI_HOME_ENV, val);
        } else {
            std::env::remove_var(KIMI_HOME_ENV);
        }
    }
}
