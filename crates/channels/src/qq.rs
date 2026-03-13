use crate::account::qq_account_id;
use blockcell_core::{Config, Error, InboundMessage, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

const QQ_API_BASE: &str = "https://api.sgroup.qq.com";
const QQ_SANDBOX_API_BASE: &str = "https://sandbox.api.sgroup.qq.com";
const QQ_AUTH_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

#[derive(Debug, Clone, PartialEq, Eq)]
enum QQEnvironment {
    Production,
    Sandbox,
}

impl QQEnvironment {
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "sandbox" => QQEnvironment::Sandbox,
            _ => QQEnvironment::Production,
        }
    }

    fn api_base(&self) -> &'static str {
        match self {
            QQEnvironment::Production => QQ_API_BASE,
            QQEnvironment::Sandbox => QQ_SANDBOX_API_BASE,
        }
    }
}

#[derive(Debug, Deserialize)]
struct QQResponse<T> {
    retcode: i32,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<T>,
}

#[derive(Debug, Deserialize, Default)]
struct AccessTokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

pub struct QQChannel {
    config: Config,
    client: Client,
    inbound_tx: mpsc::Sender<InboundMessage>,
    environment: QQEnvironment,
    token_cache: Arc<RwLock<Option<(String, u64)>>>,
    dedup: Arc<RwLock<HashSet<String>>>,
}

impl QQChannel {
    pub fn new(config: Config, inbound_tx: mpsc::Sender<InboundMessage>) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to create HTTP client");

        let environment = QQEnvironment::from_str(&config.channels.qq.environment);

        Self {
            config,
            client,
            inbound_tx,
            environment,
            token_cache: Arc::new(RwLock::new(None)),
            dedup: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    #[allow(dead_code)]
    fn api_base(&self) -> &'static str {
        self.environment.api_base()
    }

    fn is_allowed(&self, user_id: &str) -> bool {
        let allow_from = &self.config.channels.qq.allow_from;

        if allow_from.is_empty() {
            return true;
        }

        allow_from.iter().any(|allowed| allowed == "*" || allowed == user_id)
    }

    #[allow(dead_code)]
    async fn get_access_token(&self) -> Result<String> {
        let mut cache = self.token_cache.write().await;

        if let Some((token, expiry)) = cache.as_ref() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            if now < *expiry {
                return Ok(token.clone());
            }
        }

        let response = self
            .client
            .post(QQ_AUTH_URL)
            .json(&json!({
                "appId": self.config.channels.qq.app_id,
                "clientSecret": self.config.channels.qq.app_secret,
            }))
            .send()
            .await
            .map_err(|e| Error::Channel(format!("QQ auth request failed: {}", e)))?;

        let qq_response: QQResponse<AccessTokenResponse> = response
            .json()
            .await
            .map_err(|e| Error::Channel(format!("Failed to parse QQ auth response: {}", e)))?;

        if qq_response.retcode != 0 {
            return Err(Error::Channel(format!(
                "QQ auth failed: {}",
                qq_response.message
            )));
        }

        let token_data = qq_response.data.ok_or_else(|| {
            Error::Channel("QQ auth response missing token data".to_string())
        })?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let expiry = now + token_data.expires_in - 300; // Refresh 5 minutes before expiry

        *cache = Some((token_data.access_token.clone(), expiry));

        Ok(token_data.access_token)
    }

    async fn is_duplicate(&self, msg_id: &str) -> bool {
        let mut dedup = self.dedup.write().await;
        if dedup.contains(msg_id) {
            return true;
        }

        // Evict half if at capacity
        if dedup.len() >= 10_000 {
            let to_remove = dedup.len() / 2;
            for key in dedup.iter().take(to_remove).map(|k| k.clone()).collect::<Vec<_>>() {
                dedup.remove(&key);
            }
        }

        dedup.insert(msg_id.to_string());
        false
    }

    fn extract_message_id(payload: &Value) -> String {
        payload
            .get("id")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("msg_id").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string()
    }

    fn compose_message_content(payload: &Value) -> Option<String> {
        let text = payload
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim();

        let attachments: Vec<String> = payload
            .get("attachments")
            .and_then(|a| a.as_array())
            .map(|atts| {
                atts.iter()
                    .filter_map(|att| {
                        let url = att.get("url").and_then(|u| u.as_str())?;
                        let content_type = att
                            .get("content_type")
                            .and_then(|ct| ct.as_str())
                            .unwrap_or("");
                        let filename = att.get("filename").and_then(|f| f.as_str()).unwrap_or("");

                        if content_type.starts_with("image/")
                            || filename
                                .to_lowercase()
                                .ends_with(".png")
                            || filename.to_lowercase().ends_with(".jpg")
                            || filename.to_lowercase().ends_with(".jpeg")
                            || filename.to_lowercase().ends_with(".gif")
                            || filename.to_lowercase().ends_with(".webp")
                        {
                            Some(format!("[IMAGE:{}]", url))
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        if text.is_empty() && attachments.is_empty() {
            return None;
        }

        if text.is_empty() {
            return Some(attachments.join("\n"));
        }

        if attachments.is_empty() {
            return Some(text.to_string());
        }

        Some(format!("{}\n\n{}", text, attachments.join("\n")))
    }

    async fn handle_c2c_message(&self, payload: &Value) -> Result<()> {
        let msg_id = Self::extract_message_id(payload);

        if self.is_duplicate(&msg_id).await {
            debug!("Duplicate C2C message, ignoring: {}", msg_id);
            return Ok(());
        }

        let author_id = payload
            .get("author")
            .and_then(|a| a.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let user_openid = payload
            .get("author")
            .and_then(|a| a.get("user_openid"))
            .and_then(|v| v.as_str())
            .unwrap_or(author_id);

        if !self.is_allowed(user_openid) {
            debug!("User not in allowlist, ignoring: {}", user_openid);
            return Ok(());
        }

        let content = Self::compose_message_content(payload).unwrap_or_default();

        if content.is_empty() {
            return Ok(());
        }

        let inbound = InboundMessage {
            channel: "qq".to_string(),
            account_id: qq_account_id(&self.config),
            sender_id: user_openid.to_string(),
            chat_id: format!("user:{}", user_openid),
            content,
            media: vec![],
            metadata: json!({
                "message_id": msg_id,
                "message_type": "C2C",
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        self.inbound_tx
            .send(inbound)
            .await
            .map_err(|e| Error::Channel(e.to_string()))?;

        Ok(())
    }

    async fn handle_group_at_message(&self, payload: &Value) -> Result<()> {
        let msg_id = Self::extract_message_id(payload);

        if self.is_duplicate(&msg_id).await {
            debug!("Duplicate group AT message, ignoring: {}", msg_id);
            return Ok(());
        }

        let author_id = payload
            .get("author")
            .and_then(|a| a.get("member_openid"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if !self.is_allowed(author_id) {
            debug!("User not in allowlist, ignoring: {}", author_id);
            return Ok(());
        }

        let group_openid = payload
            .get("group_openid")
            .or_else(|| payload.get("group_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let content = Self::compose_message_content(payload).unwrap_or_default();

        if content.is_empty() {
            return Ok(());
        }

        let inbound = InboundMessage {
            channel: "qq".to_string(),
            account_id: qq_account_id(&self.config),
            sender_id: author_id.to_string(),
            chat_id: format!("group:{}", group_openid),
            content,
            media: vec![],
            metadata: json!({
                "message_id": msg_id,
                "message_type": "GROUP_AT",
                "group_id": group_openid,
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        self.inbound_tx
            .send(inbound)
            .await
            .map_err(|e| Error::Channel(e.to_string()))?;

        Ok(())
    }

    pub async fn handle_webhook_payload(&self, payload: &Value) -> Result<Value> {
        let op = payload
            .get("op")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Webhook validation (op = 13)
        if op == 13 {
            return self.handle_webhook_validation(payload).await;
        }

        // Event dispatch (op = 0)
        if op != 0 {
            return Ok(json!({"retcode": 0}));
        }

        let event_type = payload
            .get("t")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let data = payload.get("d");

        match event_type {
            "C2C_MESSAGE_CREATE" => {
                if let Some(payload) = data {
                    if let Err(e) = self.handle_c2c_message(payload).await {
                        error!("Failed to handle C2C message: {}", e);
                    }
                }
            }
            "GROUP_AT_MESSAGE_CREATE" => {
                if let Some(payload) = data {
                    if let Err(e) = self.handle_group_at_message(payload).await {
                        error!("Failed to handle group AT message: {}", e);
                    }
                }
            }
            _ => {
                debug!("Unhandled QQ event type: {}", event_type);
            }
        }

        Ok(json!({"retcode": 0}))
    }

    async fn handle_webhook_validation(&self, payload: &Value) -> Result<Value> {
        use ed25519_dalek::{SigningKey, Signer};
        use sha2::{Digest, Sha256};

        let validation = payload
            .get("d")
            .ok_or_else(|| Error::Channel("Missing validation data".to_string()))?;

        let plain_token = validation
            .get("plain_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Channel("Missing plain_token".to_string()))?;

        let event_ts = validation
            .get("event_ts")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Channel("Missing event_ts".to_string()))?;

        // Create signature using app_secret as seed
        let mut hasher = Sha256::new();
        hasher.update(self.config.channels.qq.app_secret.as_bytes());
        let seed_hash = hasher.finalize();

        let mut seed = [0u8; 32];
        seed.copy_from_slice(&seed_hash);

        let signing_key = SigningKey::from_bytes(&seed);
        let mut message = event_ts.as_bytes().to_vec();
        message.extend_from_slice(plain_token.as_bytes());

        let signature = signing_key.sign(&message);
        let signature_hex = hex::encode(signature.to_bytes());

        Ok(json!({
            "plain_token": plain_token,
            "signature": signature_hex
        }))
    }

    pub async fn run_loop(self: Arc<Self>, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
        if !self.config.channels.qq.enabled {
            info!("QQ channel disabled");
            return;
        }

        if self.config.channels.qq.app_id.is_empty()
            || self.config.channels.qq.app_secret.is_empty()
        {
            warn!("QQ app_id or app_secret not configured");
            return;
        }

        info!("QQ channel started (environment: {:?})", self.environment);

        // QQ channel is webhook-based, so we just wait for shutdown
        tokio::select! {
            _ = shutdown.recv() => {
                info!("QQ channel shutting down");
            }
        }
    }
}

pub async fn send_message(config: &Config, chat_id: &str, text: &str) -> Result<()> {
    crate::rate_limit::qq_limiter().acquire().await;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Channel(format!("Failed to create HTTP client: {}", e)))?;

    let environment = QQEnvironment::from_str(&config.channels.qq.environment);
    let api_base = environment.api_base();

    // Parse recipient
    let send_url = if let Some(group_id) = chat_id.strip_prefix("group:") {
        format!("{}/v2/groups/{}/messages", api_base, group_id)
    } else {
        let user_id = chat_id.strip_prefix("user:").unwrap_or(chat_id);
        format!("{}/v2/users/{}/messages", api_base, user_id)
    };

    // Get access token
    let token = get_access_token_internal(
        &client,
        &config.channels.qq.app_id,
        &config.channels.qq.app_secret,
    )
    .await?;

    // Send message
    let body = json!({
        "content": text,
        "msg_type": 0, // Text message
    });

    let response = client
        .post(&send_url)
        .header("Authorization", format!("QQBot {}", token))
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Channel(format!("QQ send message failed: {}", e)))?;

    let qq_response: QQResponse<Value> = response
        .json()
        .await
        .map_err(|e| Error::Channel(format!("Failed to parse QQ response: {}", e)))?;

    if qq_response.retcode != 0 {
        return Err(Error::Channel(format!(
            "QQ send message failed: {}",
            qq_response.message
        )));
    }

    Ok(())
}

async fn get_access_token_internal(
    client: &Client,
    app_id: &str,
    app_secret: &str,
) -> Result<String> {
    let response = client
        .post(QQ_AUTH_URL)
        .json(&json!({
            "appId": app_id,
            "clientSecret": app_secret,
        }))
        .send()
        .await
        .map_err(|e| Error::Channel(format!("QQ auth request failed: {}", e)))?;

    let qq_response: QQResponse<AccessTokenResponse> = response
        .json()
        .await
        .map_err(|e| Error::Channel(format!("Failed to parse QQ auth response: {}", e)))?;

    if qq_response.retcode != 0 {
        return Err(Error::Channel(format!(
            "QQ auth failed: {}",
            qq_response.message
        )));
    }

    let token_data = qq_response.data.ok_or_else(|| {
        Error::Channel("QQ auth response missing token data".to_string())
    })?;

    Ok(token_data.access_token)
}

pub async fn send_media_message(config: &Config, chat_id: &str, file_path: &str) -> Result<()> {
    crate::rate_limit::qq_limiter().acquire().await;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| Error::Channel(format!("Failed to create HTTP client: {}", e)))?;

    let environment = QQEnvironment::from_str(&config.channels.qq.environment);
    let api_base = environment.api_base();

    // Parse recipient
    let upload_url: String = if let Some(group_id) = chat_id.strip_prefix("group:") {
        format!("{}/v2/groups/{}/files", api_base, group_id)
    } else {
        let user_id = chat_id.strip_prefix("user:").unwrap_or(chat_id);
        format!("{}/v2/users/{}/files", api_base, user_id)
    };

    // Get access token
    let token = get_access_token_internal(
        &client,
        &config.channels.qq.app_id,
        &config.channels.qq.app_secret,
    )
    .await?;

    // Read file
    let file_bytes = tokio::fs::read(file_path)
        .await
        .map_err(|e| Error::Channel(format!("Failed to read file {}: {}", file_path, e)))?;

    let filename = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();

    // Determine file type
    let file_type = if filename
        .to_lowercase()
        .ends_with(".png")
        || filename.to_lowercase().ends_with(".jpg")
        || filename.to_lowercase().ends_with(".jpeg")
        || filename.to_lowercase().ends_with(".gif")
        || filename.to_lowercase().ends_with(".webp")
    {
        1 // Image
    } else {
        0 // File
    };

    // Upload file
    let part = reqwest::multipart::Part::bytes(file_bytes)
        .file_name(filename)
        .mime_str("application/octet-stream")
        .map_err(|e| Error::Channel(format!("Invalid MIME: {}", e)))?;

    let form = reqwest::multipart::Form::new()
        .text("file_type", file_type.to_string())
        .part("file", part);

    let response = client
        .post(&upload_url)
        .header("Authorization", format!("QQBot {}", token))
        .multipart(form)
        .send()
        .await
        .map_err(|e| Error::Channel(format!("QQ upload file failed: {}", e)))?;

    let qq_response: QQResponse<Value> = response
        .json()
        .await
        .map_err(|e| Error::Channel(format!("Failed to parse QQ response: {}", e)))?;

    if qq_response.retcode != 0 {
        return Err(Error::Channel(format!(
            "QQ upload file failed: {}",
            qq_response.message
        )));
    }

    // Get file_info from response
    let data = qq_response.data.ok_or_else(|| {
        Error::Channel("QQ upload response missing data".to_string())
    })?;

    let file_info = data
        .get("file_info")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Channel("QQ upload response missing file_info".to_string()))?
        .to_string();

    // Send media message
    let send_url = upload_url.replace("/files", "/messages");

    let body = json!({
        "content": " ",
        "msg_type": 7, // Media message
        "media": {
            "file_info": file_info
        }
    });

    let response = client
        .post(&send_url)
        .header("Authorization", format!("QQBot {}", token))
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Channel(format!("QQ send media failed: {}", e)))?;

    let qq_response: QQResponse<Value> = response
        .json()
        .await
        .map_err(|e| Error::Channel(format!("Failed to parse QQ response: {}", e)))?;

    if qq_response.retcode != 0 {
        return Err(Error::Channel(format!(
            "QQ send media failed: {}",
            qq_response.message
        )));
    }

    Ok(())
}
