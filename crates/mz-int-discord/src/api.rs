//! Panggilan Bot API Discord (`axios` di `discord.service.ts`): URL dasar
//! `api_base::DISCORD`, header `Authorization: Bot ${DISCORD_BOT_TOKEN}`.
//! Status non-2xx dan galat jaringan = lemparan axios ([`DiscordError`]).

use mz_core::AppState;
use mz_int_common::api_base;
use serde::Serialize;
use serde_json::Value;

/// Padanan `AxiosError` yang dibaca Node: `error.response?.status`, `error.message`,
/// `error.response?.data?.message`.
#[derive(Debug)]
pub struct DiscordError {
    pub status: Option<u16>,
    pub message: String,
    pub data_message: Option<String>,
}

impl std::fmt::Display for DiscordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// `Bot ${process.env.DISCORD_BOT_TOKEN}` (`undefined` bila tidak diset, seperti Node).
fn bot_header(state: &AppState) -> String {
    let token = state
        .config
        .var("DISCORD_BOT_TOKEN")
        .unwrap_or_else(|| "undefined".to_owned());
    format!("Bot {token}")
}

/// Body respons seperti `response.data` axios: JSON bila bisa di-parse, selain itu
/// string mentah (termasuk `""` untuk body kosong).
fn parse_body(text: String) -> Value {
    if text.is_empty() {
        return Value::String(text);
    }
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

async fn send(state: &AppState, req: reqwest::RequestBuilder) -> Result<Value, DiscordError> {
    let res = req
        .header(reqwest::header::AUTHORIZATION, bot_header(state))
        .send()
        .await
        .map_err(|e| DiscordError {
            status: None,
            message: e.without_url().to_string(),
            data_message: None,
        })?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    let data = parse_body(text);
    if status.is_success() {
        return Ok(data);
    }
    Err(DiscordError {
        status: Some(status.as_u16()),
        message: format!("Request failed with status code {}", status.as_u16()),
        data_message: data.get("message").map(|m| crate::util::js_string(Some(m))),
    })
}

fn url(state: &AppState, path: &str) -> String {
    format!("{}{path}", api_base::DISCORD.get(state))
}

/// `axios.get(`${DISCORD_API}${path}`, { headers, params })`.
pub async fn get(
    state: &AppState,
    path: &str,
    params: &[(&str, &str)],
) -> Result<Value, DiscordError> {
    let mut req = state.http.get(url(state, path));
    if !params.is_empty() {
        req = req.query(params);
    }
    send(state, req).await
}

/// `axios.post(url, body, { headers })`.
pub async fn post(
    state: &AppState,
    path: &str,
    body: &impl Serialize,
) -> Result<Value, DiscordError> {
    send(state, state.http.post(url(state, path)).json(body)).await
}

/// `axios.delete(url, { headers })`.
pub async fn delete(state: &AppState, path: &str) -> Result<Value, DiscordError> {
    send(state, state.http.delete(url(state, path))).await
}
