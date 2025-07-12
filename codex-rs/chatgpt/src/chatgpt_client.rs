use codex_core::config::Config;

use crate::chatgpt_token::get_chatgpt_token_data;
use crate::chatgpt_token::init_chatgpt_token_from_auth;

use anyhow::Context;
use serde::de::DeserializeOwned;

/// Make a GET request to the ChatGPT backend API.
pub(crate) async fn chatgpt_get_request<T: DeserializeOwned>(
    config: &Config,
    path: String,
) -> anyhow::Result<T> {
    let chatgpt_base_url = &config.chatgpt_base_url;
    init_chatgpt_token_from_auth(&config.codex_home).await?;

    // Make direct HTTP request to ChatGPT backend API with the token
    let client = reqwest::Client::new();
    let url = format!("{chatgpt_base_url}{path}");

    let token =
        get_chatgpt_token_data().ok_or_else(|| anyhow::anyhow!("ChatGPT token not available"))?;

    let response = client
        .get(&url)
        .bearer_auth(&token.access_token)
        .header("chatgpt-account-id", &token.account_id)
        .header("Content-Type", "application/json")
        .header("User-Agent", "codex-cli")
        .send()
        .await
        .context("Failed to send request")?;

    if response.status().is_success() {
        let result: T = response
            .json()
            .await
            .context("Failed to parse JSON response")?;
        Ok(result)
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Request failed with status {}: {}", status, body)
    }
}
