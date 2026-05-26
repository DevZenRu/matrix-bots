use axum::{
    Router,
    body::Bytes,
    http::StatusCode,
    response::IntoResponse,
    routing::any,
};
use matrix_sdk::Client;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::{OwnedRoomAliasId, RoomOrAliasId};
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use serde::Deserialize;
use std::{env, fs, net::SocketAddr};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Config {
    /// ID of the "In Discussion" Trello list
    in_discussion_list_id: String,
    /// Trello application key
    trello_app_key: String,
    /// Trello read token
    trello_read_token: String,
    /// Matrix user ID of the bot in the format @user:server
    matrix_user_id: String,
    /// Bot account password
    matrix_password: String,
    /// Device display name used when registering a session on the homeserver
    matrix_device_display_name: String,
    /// List of recipients: user IDs (@user:server) or room aliases (#alias:server)
    recipients: Vec<String>,
    /// Port to listen on
    port: Option<u16>,
    /// Host/IP to listen on
    host: Option<String>,
}

// ---------------------------------------------------------------------------
// Trello webhook payload structures
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TrelloWebhookEvent {
    action: TrelloAction,
}

#[derive(Debug, Deserialize)]
struct TrelloAction {
    #[serde(rename = "type")]
    action_type: String,
    data: TrelloActionData,
}

#[derive(Debug, Deserialize)]
struct TrelloActionData {
    card: TrelloCardRef,
    #[serde(rename = "listAfter")]
    list_after: Option<TrelloList>,
}

/// Minimal card info that arrives in the webhook payload (no desc).
#[derive(Debug, Deserialize)]
struct TrelloCardRef {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct TrelloList {
    id: String,
}

/// Full card object returned by the Trello REST API.
#[derive(Debug, Deserialize)]
struct TrelloCard {
    desc: String,
}

// ---------------------------------------------------------------------------
// URL extraction
// ---------------------------------------------------------------------------

fn extract_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("http://").or_else(|| remaining.find("https://")) {
        remaining = &remaining[start..];
        let end = remaining
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | ')' | ']' | '<' | '>'))
            .unwrap_or(remaining.len());
        let url = &remaining[..end];
        if !urls.contains(&url.to_owned()) {
            urls.push(url.to_owned());
        }
        remaining = &remaining[end..];
    }
    urls
}

// ---------------------------------------------------------------------------
// Trello API client
// ---------------------------------------------------------------------------

async fn fetch_card(
    client: &reqwest::Client,
    card_id: &str,
    app_key: &str,
    token: &str,
) -> Result<TrelloCard, String> {
    let url = format!(
        "https://api.trello.com/1/cards/{}?key={}&token={}",
        card_id, app_key, token
    );

    let mut last_err = String::new();
    for attempt in 1..=3 {
        match client.get(&url).send().await {
            Err(e) => {
                last_err = format!("Request failed: {e}");
                eprintln!("Attempt {attempt}/3 failed for card '{card_id}': {e}");
            }
            Ok(resp) => {
                if !resp.status().is_success() {
                    return Err(format!("Trello API returned HTTP {}", resp.status()));
                }
                return resp
                    .json::<TrelloCard>()
                    .await
                    .map_err(|e| format!("Failed to parse card JSON: {e}"));
            }
        }
    }

    Err(last_err)
}

// ---------------------------------------------------------------------------
// Matrix recipient
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Recipient {
    RoomAlias(OwnedRoomAliasId),
}

impl Recipient {
    fn parse(s: &str) -> Result<Self, String> {
        match s.chars().next() {
            Some('#') => s
                .try_into()
                .map(Recipient::RoomAlias)
                .map_err(|e| format!("Invalid room alias '{s}': {e}")),
            _ => Err(format!(
                "Unknown recipient format '{s}': must start with #"
            )),
        }
    }
}

async fn send_to_matrix(
    matrix_client: &Client,
    recipients: &[Recipient],
    title: &str,
    urls: &[String],
) {
    let plain = if urls.is_empty() {
        title.to_owned()
    } else {
        format!("{title}\n{}", urls.iter().map(|u| format!("* {u}")).collect::<Vec<_>>().join("\n"))
    };

    let html = if urls.is_empty() {
        format!("<p>{title}</p>")
    } else {
        let links = urls
            .iter()
            .map(|u| format!("<li><a href=\"{u}\">{u}</a></li>"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("<p>{title}</p>\n<ul>\n{links}\n</ul>")
    };

    let message = RoomMessageEventContent::text_html(plain, html);

    // Sync once so the client is aware of already joined rooms
    if let Err(e) = matrix_client.sync_once(SyncSettings::default()).await {
        eprintln!("Matrix sync failed: {e}");
    }

    for recipient in recipients {
        match recipient {
            Recipient::RoomAlias(alias) => {
                let alias_or_id: &RoomOrAliasId =
                    <&RoomOrAliasId>::try_from(alias.as_str()).unwrap();
                let room = match matrix_client.join_room_by_id_or_alias(alias_or_id, &[]).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("Failed to join room '{alias}': {e}");
                        continue;
                    }
                };

                if let Err(e) = room.send(message.clone()).await {
                    eprintln!("Failed to send message to room '{alias}': {e}");
                } else {
                    println!("Sent to room '{alias}'");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    in_discussion_list_id: String,
    trello_app_key: String,
    trello_read_token: String,
    http_client: reqwest::Client,
    matrix_client: Client,
    recipients: Vec<Recipient>,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

async fn trello_hook(
    axum::extract::State(state): axum::extract::State<AppState>,
    method: axum::http::Method,
    body: Bytes,
) -> impl IntoResponse {
    // Trello sends a HEAD/GET request to verify the webhook URL — just respond 200.
    if method == axum::http::Method::GET || method == axum::http::Method::HEAD {
        return StatusCode::OK;
    }

    if method != axum::http::Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED;
    }

    let event: TrelloWebhookEvent = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("Failed to parse webhook JSON: {err}");
            eprintln!("Raw body: {}", String::from_utf8_lossy(&body));
            return StatusCode::OK;
        }
    };

    if event.action.action_type != "updateCard" {
        return StatusCode::OK;
    }

    let data = &event.action.data;

    // Check if card moved into the "In Discussion" list.
    let moved_to_in_discussion = data
        .list_after
        .as_ref()
        .map(|l| l.id == state.in_discussion_list_id)
        .unwrap_or(false);

    if moved_to_in_discussion {
        let card_id = &data.card.id;
        let title = &data.card.name;

        match fetch_card(&state.http_client, card_id, &state.trello_app_key, &state.trello_read_token).await {
            Ok(card) => {
                let urls = extract_urls(&card.desc);

                // Print to console.
                println!("{title}");
                for url in &urls {
                    println!("* {url}");
                }

                // Send to Matrix.
                send_to_matrix(&state.matrix_client, &state.recipients, title, &urls).await;
            }
            Err(e) => {
                eprintln!("Failed to fetch card '{card_id}': {e}");
            }
        }
    }

    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let config_path = match env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("Usage: devzen-matrix-topics-bot <config.toml>");
            return;
        }
    };

    let config_str = match fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to read config file '{config_path}': {e}");
            return;
        }
    };

    let config: Config = match toml::from_str(&config_str) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to parse config file '{config_path}': {e}");
            return;
        }
    };

    // Parse all recipients upfront so we can report errors before connecting
    let mut recipients = Vec::new();
    for raw in &config.recipients {
        match Recipient::parse(raw) {
            Ok(r) => recipients.push(r),
            Err(e) => {
                eprintln!("{e}");
                return;
            }
        }
    }

    // Build and log in the Matrix client
    let user_id = match <&matrix_sdk::ruma::UserId>::try_from(config.matrix_user_id.as_str()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("Invalid Matrix user ID '{}': {e}", config.matrix_user_id);
            return;
        }
    };

    let homeserver_url = format!("https://{}", user_id.server_name());
    let matrix_client = match Client::builder()
        .homeserver_url(&homeserver_url)
        .build()
        .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to create Matrix client: {e}");
            return;
        }
    };

    if let Err(e) = matrix_client
        .matrix_auth()
        .login_username(user_id, &config.matrix_password)
        .initial_device_display_name(&config.matrix_device_display_name)
        .await
    {
        eprintln!("Matrix login failed: {e}");
        return;
    }

    println!(
        "Logged in to Matrix as {}",
        matrix_client.user_id().expect("user_id must be available after login")
    );

    let port = config.port.unwrap_or(3000);
    let host = config.host.as_deref().unwrap_or("127.0.0.1");

    let addr: SocketAddr = match format!("{host}:{port}").parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Invalid host/port '{host}:{port}': {e}");
            return;
        }
    };

    let state = AppState {
        in_discussion_list_id: config.in_discussion_list_id,
        trello_app_key: config.trello_app_key,
        trello_read_token: config.trello_read_token,
        http_client: reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:151.0) Gecko/20100101 Firefox/151.0")
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to build HTTP client"),
        matrix_client,
        recipients,
    };

    let app = Router::new()
        .route("/trellohook", any(trello_hook))
        .with_state(state);

    println!("Listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
