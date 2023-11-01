use super::{Client, ExtraConfig, ModelInfo, OpenAIClient, PromptKind, PromptType, SendData};

use crate::config::SharedConfig;
use crate::repl::ReplyStreamHandler;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;

const API_BASE: &str = "https://api.openai.com/v1";

const MODELS: [(&str, usize); 4] = [
    ("gpt-3.5-turbo", 4096),
    ("gpt-3.5-turbo-16k", 16384),
    ("gpt-4", 8192),
    ("gpt-4-32k", 32768),
];

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OpenAIConfig {
    pub name: Option<String>,
    pub api_key: Option<String>,
    pub organization_id: Option<String>,
    pub extra: Option<ExtraConfig>,
}

#[async_trait]
impl Client for OpenAIClient {
    fn config(&self) -> (&SharedConfig, &Option<ExtraConfig>) {
        (&self.global_config, &self.config.extra)
    }

    async fn send_message_inner(&self, client: &ReqwestClient, data: SendData) -> Result<String> {
        let builder = self.request_builder(client, data)?;
        openai_send_message(builder).await
    }

    async fn send_message_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut ReplyStreamHandler,
        data: SendData,
    ) -> Result<()> {
        let builder = self.request_builder(client, data)?;
        openai_send_message_streaming(builder, handler).await
    }
}

impl OpenAIClient {
    pub const PROMPTS: [PromptType<'static>; 1] =
        [("api_key", "API Key:", true, PromptKind::String)];

    pub fn list_models(local_config: &OpenAIConfig, index: usize) -> Vec<ModelInfo> {
        let client = Self::name(local_config);
        MODELS
            .into_iter()
            .map(|(name, max_tokens)| ModelInfo::new(client, name, Some(max_tokens), index))
            .collect()
    }

    fn request_builder(&self, client: &ReqwestClient, data: SendData) -> Result<RequestBuilder> {
        let env_prefix = Self::name(&self.config).to_uppercase();

        let api_key = self.config.api_key.clone();
        let api_key = api_key
            .or_else(|| env::var(format!("{env_prefix}_API_KEY")).ok())
            .ok_or_else(|| anyhow!("Miss api_key"))?;

        let body = openai_build_body(data, self.model_info.name.clone());

        let api_base = env::var(format!("{env_prefix}_API_BASE"))
            .ok()
            .unwrap_or_else(|| API_BASE.to_string());

        let url = format!("{api_base}/chat/completions");

        let mut builder = client.post(url).bearer_auth(api_key).json(&body);

        if let Some(organization_id) = &self.config.organization_id {
            builder = builder.header("OpenAI-Organization", organization_id);
        }

        Ok(builder)
    }
}

pub async fn openai_send_message(builder: RequestBuilder) -> Result<String> {
    let data: Value = builder.send().await?.json().await?;
    if let Some(err_msg) = data["error"]["message"].as_str() {
        bail!("{err_msg}");
    }

    let output = data["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow!("Invalid response data: {data}"))?;

    Ok(output.to_string())
}

pub async fn openai_send_message_streaming(
    builder: RequestBuilder,
    handler: &mut ReplyStreamHandler,
) -> Result<()> {
    let res = builder.send().await?;
    if !res.status().is_success() {
        let data: Value = res.json().await?;
        if let Some(err_msg) = data["error"]["message"].as_str() {
            bail!("{err_msg}");
        }
        bail!("Request failed");
    }
    let mut stream = res.bytes_stream().eventsource();
    while let Some(part) = stream.next().await {
        let chunk = part?.data;
        if chunk == "[DONE]" {
            break;
        }
        let data: Value = serde_json::from_str(&chunk)?;
        if let Some(text) = data["choices"][0]["delta"]["content"].as_str() {
            handler.text(text)?;
        }
    }

    Ok(())
}

pub fn openai_build_body(data: SendData, model: String) -> Value {
    let SendData {
        messages,
        temperature,
        stream,
    } = data;

    let mut body = json!({
        "model": model,
        "messages": messages,
    });
    if let Some(v) = temperature {
        body["temperature"] = v.into();
    }
    if stream {
        body["stream"] = true.into();
    }
    body
}
