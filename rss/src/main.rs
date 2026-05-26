use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;

use regex::Regex;
use mime::Mime;

use matrix_sdk::Client;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::{OwnedRoomAliasId, OwnedUserId, RoomOrAliasId};
use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoomRequest;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use serde::Deserialize;

/// Bot configuration loaded from a TOML file
#[derive(Deserialize)]
struct Config {
    /// Matrix user ID of the bot in the format @user:server
    user_id: String,
    /// Bot account password
    password: String,
    /// Device display name used when registering a session on the homeserver
    device_display_name: String,
    /// List of recipients: user IDs (@user:server) or room aliases (#alias:server)
    recipients: Vec<String>,
    /// RSS feed URL to fetch and store episode links from
    feed_url: String,
    /// Optional SOCKS5 proxy URL (e.g. socks5://127.0.0.1:1080)
    proxy: Option<String>,
    /// How often to poll the feed, in minutes
    interval_minutes: u64,
    /// If true, set the bot's avatar to the image at avatar_path on startup
    change_avatar: Option<bool>,
    /// Path to the image file to use as the bot's avatar
    avatar_path: Option<String>,
}

/// A new (not yet processed) item from the RSS feed
#[derive(Debug)]
struct FeedItem {
    /// URL of the episode (`<link>`)
    url: String,
    /// Episode title (`<title>`)
    title: String,
    /// Full episode description (`<content:encoded>`)
    content: String,
}

/// A parsed recipient from the config
enum Recipient {
    User(OwnedUserId),
    RoomAlias(OwnedRoomAliasId),
}

impl Recipient {
    fn parse(s: &str) -> Result<Self, String> {
        match s.chars().next() {
            Some('@') => s
                .try_into()
                .map(Recipient::User)
                .map_err(|e| format!("Invalid user ID '{s}': {e}")),
            Some('#') => s
                .try_into()
                .map(Recipient::RoomAlias)
                .map_err(|e| format!("Invalid room alias '{s}': {e}")),
            _ => Err(format!(
                "Unknown recipient format '{s}': must start with @ or #"
            )),
        }
    }
}

#[tokio::main]
async fn main() {
    let config_path = match env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("Usage: devzen-matrix-bot <config.toml>");
            return;
        }
    };

    let config_str = match fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to read config file '{}': {e}", config_path);
            return;
        }
    };

    let config: Config = match toml::from_str(&config_str) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to parse config file '{}': {e}", config_path);
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

    let user_id = match <&matrix_sdk::ruma::UserId>::try_from(config.user_id.as_str()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("Invalid user ID '{}': {e}", config.user_id);
            return;
        }
    };

    let homeserver_url = format!("https://{}", user_id.server_name());

    // Build the client with an explicit homeserver URL, bypassing auto-discovery
    let client = match Client::builder()
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

    // Log in with username and password
    if let Err(e) = client
        .matrix_auth()
        .login_username(user_id, &config.password)
        .initial_device_display_name(&config.device_display_name)
        .await
    {
        eprintln!("Login failed: {e}");
        return;
    }

    println!(
        "Login successful! Logged in as: {}",
        client.user_id().expect("user_id must be available after login")
    );

    // Optionally set the bot's avatar before entering the main loop
    if config.change_avatar.unwrap_or(false) {
        match &config.avatar_path {
            Some(path) => {
                if let Err(e) = set_avatar(&client, path).await {
                    eprintln!("Failed to set avatar: {e}");
                }
            }
            None => eprintln!("change_avatar is true but avatar_path is not set in config"),
        }
    }

    let interval = tokio::time::Duration::from_secs(config.interval_minutes * 60);
    println!("Polling every {} minute(s). Press Ctrl+C to stop.", config.interval_minutes);

    loop {
        // Sync so the client is aware of already joined rooms
        if let Err(e) = client.sync_once(SyncSettings::default()).await {
            eprintln!("Sync failed: {e}");
        } else {
            // Fetch new RSS items (not yet in processed.txt)
            let new_items = match fetch_new_feed_urls(&config.feed_url, config.proxy.as_deref()).await {
                Ok(items) => items,
                Err(e) => {
                    eprintln!("{e}");
                    vec![]
                }
            };
            println!("Found {} new episode(s) in the feed.", new_items.len());

            // Send each new feed item to every recipient
            for item in &new_items {
                println!("{} {}\n\n{}", item.title, item.url, item.content);
                for recipient in &recipients {
                    send_feed_item(&client, recipient, item).await;
                }
            }

            // Only update processed.txt after all messages were sent
            if !new_items.is_empty() {
                if let Err(e) = append_urls_to_file(&new_items, "processed.txt") {
                    eprintln!("{e}");
                }
            }
        }

        println!("Sleeping for {} minute(s)...", config.interval_minutes);
        tokio::time::sleep(interval).await;
    }
}

/// Reads an image file from `path`, detects its MIME type from the extension,
/// uploads it to the homeserver and sets it as the bot's avatar.
async fn set_avatar(client: &Client, path: &str) -> Result<(), String> {
    let data = fs::read(path)
        .map_err(|e| format!("Failed to read avatar file '{path}': {e}"))?;
    println!("[avatar] Read {} bytes from '{path}'", data.len());

    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let content_type: Mime = match ext.to_lowercase().as_str() {
        "jpg" | "jpeg" => mime::IMAGE_JPEG,
        "png"          => mime::IMAGE_PNG,
        "gif"          => mime::IMAGE_GIF,
        "webp"         => "image/webp".parse().unwrap(),
        other          => return Err(format!("Unsupported avatar file extension: '{other}'")),
    };
    println!("[avatar] Detected content type: {content_type}");

    println!("[avatar] Uploading media...");
    let mxc_uri = client
        .account()
        .upload_avatar(&content_type, data)
        .await
        .map_err(|e| format!("Failed to upload avatar: {e}"))?;
    println!("[avatar] Uploaded, mxc URI: {mxc_uri}");

    println!("[avatar] Verifying: fetching current avatar URL...");
    match client.account().get_avatar_url().await {
        Ok(Some(url)) => println!("[avatar] Server reports avatar URL: {url}"),
        Ok(None)      => println!("[avatar] Server reports no avatar URL set"),
        Err(e)        => println!("[avatar] Could not fetch avatar URL: {e}"),
    }

    println!("Avatar set from '{path}'.");
    Ok(())
}

/// Fetches the RSS feed at `url` and returns only those item links that are
/// not yet present in `processed.txt`. The file is read if it exists; if it
/// doesn't exist yet, all links are considered new.
async fn fetch_new_feed_urls(url: &str, proxy: Option<&str>) -> Result<Vec<FeedItem>, String> {
    println!("Fetching RSS feed: {url}");

    let client = {
        let mut builder = reqwest::Client::builder();
        if let Some(proxy_url) = proxy {
            let p = reqwest::Proxy::all(proxy_url)
                .map_err(|e| format!("Invalid proxy URL '{proxy_url}': {e}"))?;
            builder = builder.proxy(p);
            println!("Using SOCKS5 proxy: {proxy_url}");
        }
        builder.build().map_err(|e| format!("Failed to build HTTP client: {e}"))?
    };

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch RSS feed '{url}': {e}"))?;

    let body = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read RSS response body: {e}"))?;

    let channel = rss::Channel::read_from(&body[..])
        .map_err(|e| format!("Failed to parse RSS feed: {e}"))?;

    let existing: HashSet<String> = fs::read_to_string("processed.txt")
        .unwrap_or_default()
        .lines()
        .map(|l| l.to_string())
        .collect();

    let new_items = channel
        .items()
        .iter()
        .filter(|item| item.link().is_some_and(|l| !existing.contains(l)))
        .map(|item| FeedItem {
            url: item.link().unwrap_or_default().to_string(),
            title: item.title().unwrap_or_default().to_string(),
            content: preprocess_content(item.content().unwrap_or_default()),
        })
        .collect();

    Ok(new_items)
}

/// Removes lines matching devzen.ru image tags.
fn preprocess_content(content: &str) -> String {
    let img_re = Regex::new(r#"<img [^>]*src="https://devzen\.ru/wp-content/uploads/"#).unwrap();
    content
        .split('\n')
        .filter(|line| !img_re.is_match(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Appends each URL in `urls` as a new line to the file at `path`,
/// creating the file if it does not exist yet.
fn append_urls_to_file(items: &[FeedItem], path: &str) -> Result<(), String> {
    if items.is_empty() {
        println!("No new URLs to save.");
        return Ok(());
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("Failed to open '{path}' for appending: {e}"))?;

    for item in items {
        writeln!(file, "{}", item.url)
            .map_err(|e| format!("Failed to write to '{path}': {e}"))?;
    }

    println!("Saved {} new URL(s) to '{path}'.", items.len());
    Ok(())
}

/// Formats a feed item as a plain-text message and sends it to the recipient's room.
async fn send_feed_item(client: &Client, recipient: &Recipient, item: &FeedItem) {
    let plain = format!("{} {}", item.title, item.url);
    let html = format!("<p><strong>{}</strong> <a href=\"{}\">{}</a></p>\n{}",
        item.title, item.url, item.url, item.content);
    let message = RoomMessageEventContent::text_html(plain, html);

    match recipient {
        Recipient::User(user_id) => {
            // Create a direct message room and invite the user
            let mut request = CreateRoomRequest::new();
            request.invite = vec![user_id.clone()];
            request.is_direct = true;

            let room_id = match client.create_room(request).await {
                Ok(response) => response.room_id().to_owned(),
                Err(e) => {
                    eprintln!("Failed to create DM room for '{user_id}': {e}");
                    return;
                }
            };

            let room = match client.get_room(&room_id) {
                Some(r) => r,
                None => {
                    eprintln!("Room '{room_id}' not found after creation");
                    return;
                }
            };

            if let Err(e) = room.send(message).await {
                eprintln!("Failed to send message to '{user_id}': {e}");
            } else {
                println!("Sent item '{}' to user '{user_id}'", item.title);
            }
        }

        Recipient::RoomAlias(alias) => {
            let alias_or_id: &RoomOrAliasId = <&RoomOrAliasId>::try_from(alias.as_str()).unwrap();
            let room = match client.join_room_by_id_or_alias(alias_or_id, &[]).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Failed to join room '{alias}': {e}");
                    return;
                }
            };

            if let Err(e) = room.send(message).await {
                eprintln!("Failed to send message to room '{alias}': {e}");
            } else {
                println!("Sent item '{}' to room '{alias}'", item.title);
            }
        }
    }
}
