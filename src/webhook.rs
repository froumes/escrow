use once_cell::sync::Lazy;
use tracing::warn;

// Shared HTTP client - reqwest clients are designed to be cloned/reused
static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .build()
        .expect("Failed to build reqwest client")
});

async fn post_embed(webhook_url: &str, payload: serde_json::Value) {
    if let Err(e) = HTTP_CLIENT.post(webhook_url).json(&payload).send().await {
        warn!("[Webhook] Failed to send webhook: {}", e);
    }
}

/// Format a number with M/K suffixes matching TypeScript formatNumber()
fn format_number(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.2}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.2}K", n / 1_000.0)
    } else {
        format!("{:.0}", n)
    }
}

/// Sanitize an item name for use as an icon URL path component
fn sanitize_item_name(name: &str) -> String {
    name.replace(|c: char| !c.is_alphanumeric() && c != '_', "_")
}

/// Unix timestamp seconds for Discord relative timestamps
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format seconds as a human-readable duration ("2h 5m 30s" etc.)
fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}h {}m", h, m)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

/// Format purse amount with b/m/k suffixes matching TypeScript formatPurse()
/// Examples: 1_404_040_000 → "1.40b", 96_532_000 → "96.53m", 590_278 → "590.3k"
fn format_purse(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2}b", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.2}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}


pub async fn send_webhook_initialized(
    ingame_name: &str,
    ah_enabled: bool,
    bazaar_enabled: bool,
    connection_id: Option<&str>,
    premium: Option<(&str, &str)>, // (tier, expires)
    webhook_url: &str,
) {
    let mut description = format!(
        "AH Flips: {} | Bazaar Flips: {}\n<t:{}:R>",
        if ah_enabled { "✅" } else { "❌" },
        if bazaar_enabled { "✅" } else { "❌" },
        now_unix()
    );
    if let Some((tier, expires)) = premium {
        description.push_str(&format!("\n\n**Coflnet {}** expires {}", tier, expires));
    }

    let mut fields: Vec<serde_json::Value> = Vec::new();
    if let Some(conn_id) = connection_id {
        fields.push(serde_json::json!({
            "name": "Connection ID",
            "value": format!("`{}`", conn_id),
            "inline": false
        }));
    }

    let embed = if fields.is_empty() {
        serde_json::json!({
            "title": "✓ Started BAF",
            "description": description,
            "color": 0x00ff88u32,
            "footer": {
                "text": format!("BAF - {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        })
    } else {
        serde_json::json!({
            "title": "✓ Started BAF",
            "description": description,
            "color": 0x00ff88u32,
            "fields": fields,
            "footer": {
                "text": format!("BAF - {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        })
    };
    let payload = serde_json::json!({ "embeds": [embed] });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_startup_complete(
    ingame_name: &str,
    orders_found: u64,
    ah_enabled: bool,
    bazaar_enabled: bool,
    connection_id: Option<&str>,
    premium: Option<(&str, &str)>, // (tier, expires)
    webhook_url: &str,
) {
    let mut description = format!(
        "Ready to accept flips!\n\nAH Flips: {}\nBazaar Flips: {}",
        if ah_enabled { "✅ Enabled" } else { "❌ Disabled" },
        if bazaar_enabled { "✅ Enabled" } else { "❌ Disabled" }
    );
    if let Some((tier, expires)) = premium {
        description.push_str(&format!("\n\n**Coflnet {}** expires {}", tier, expires));
    }

    let mut fields = vec![
        serde_json::json!({"name": "1️⃣ Cookie Check", "value": "```✓ Complete```", "inline": true}),
        serde_json::json!({
            "name": "2️⃣ Order Discovery",
            "value": if bazaar_enabled {
                format!("```✓ Found {} order(s)```", orders_found)
            } else {
                "```- Skipped (Bazaar disabled)```".to_string()
            },
            "inline": true
        }),
        serde_json::json!({"name": "3️⃣ Claim Items", "value": "```✓ Complete```", "inline": true}),
    ];
    if let Some(conn_id) = connection_id {
        fields.push(serde_json::json!({
            "name": "Connection ID",
            "value": format!("`{}`", conn_id),
            "inline": false
        }));
    }

    let payload = serde_json::json!({
        "embeds": [{
            "title": "🚀 Startup Workflow Complete",
            "description": description,
            "color": 0x2ecc71u32,
            "fields": fields,
            "footer": {
                "text": format!("BAF - {}", ingame_name),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            },
            "timestamp": chrono::Utc::now().to_rfc3339()
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_item_purchased(
    ingame_name: &str,
    item_name: &str,
    price: u64,
    target: Option<u64>,
    profit: Option<i64>,
    purse: Option<u64>,
    buy_speed_ms: Option<u64>,
    auction_uuid: Option<&str>,
    webhook_url: &str,
) {
    let mut fields = vec![
        serde_json::json!({
            "name": "💰 Purchase Price",
            "value": format!("```fix\n{} coins\n```", format_number(price as f64)),
            "inline": true
        }),
    ];
    if let Some(t) = target {
        fields.push(serde_json::json!({
            "name": "🎯 Target Price",
            "value": format!("```fix\n{} coins\n```", format_number(t as f64)),
            "inline": true
        }));
    }
    if let Some(p) = profit {
        let sign = if p >= 0 { "+" } else { "" };
        let roi_str = if let Some(t) = target {
            if t > 0 && price > 0 {
                format!(" ({:.1}%)", (p as f64 / price as f64) * 100.0)
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        fields.push(serde_json::json!({
            "name": "📈 Expected Profit",
            "value": format!("```diff\n{}{} coins{}\n```", sign, format_number(p as f64), roi_str),
            "inline": true
        }));
    }
    if let Some(ms) = buy_speed_ms {
        fields.push(serde_json::json!({
            "name": "⚡ Buy Speed",
            "value": format!("```\n{}ms\n```", ms),
            "inline": true
        }));
    }
    if let Some(uuid) = auction_uuid {
        if !uuid.is_empty() {
            fields.push(serde_json::json!({
                "name": "🔗 Auction Link",
                "value": format!("[View on Coflnet](https://sky.coflnet.com/auction/{}?refId=9KKPN9)", uuid),
                "inline": false
            }));
        }
    }
    let safe_item = sanitize_item_name(item_name);
    let payload = serde_json::json!({
        "embeds": [{
            "title": "🛒 Item Purchased Successfully",
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0x00ff00,
            "fields": fields,
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_item_sold(
    ingame_name: &str,
    item_name: &str,
    price: u64,
    buyer: &str,
    profit: Option<i64>,
    buy_price: Option<u64>,
    time_to_sell_secs: Option<u64>,
    purse: Option<u64>,
    auction_uuid: Option<&str>,
    webhook_url: &str,
) {
    let safe_item = sanitize_item_name(item_name);
    let status_emoji = match profit {
        Some(p) if p >= 0 => "✅",
        Some(_) => "❌",
        None => "✅",
    };
    let title = match profit {
        Some(p) if p >= 0 => "Item Sold (Profit)",
        Some(_) => "Item Sold (Loss)",
        None => "Item Sold",
    };
    let mut fields = vec![
        serde_json::json!({
            "name": "👤 Buyer",
            "value": format!("```\n{}\n```", buyer),
            "inline": true
        }),
        serde_json::json!({
            "name": "💵 Sale Price",
            "value": format!("```fix\n{} coins\n```", format_number(price as f64)),
            "inline": true
        }),
    ];
    if let Some(p) = profit {
        let sign = if p >= 0 { "+" } else { "-" };
        let abs_profit = if p >= 0 { p as f64 } else { (-p) as f64 };
        fields.push(serde_json::json!({
            "name": "💰 Net Profit",
            "value": format!("```diff\n{}{} coins\n```", sign, format_number(abs_profit)),
            "inline": true
        }));
        // ROI percentage (matching TypeScript sendWebhookItemSold)
        if let Some(bp) = buy_price {
            if bp > 0 {
                let roi = (p as f64 / bp as f64) * 100.0;
                fields.push(serde_json::json!({
                    "name": "📊 ROI",
                    "value": format!("```{:.1}%```", roi),
                    "inline": true
                }));
            }
        }
    }
    if let Some(secs) = time_to_sell_secs {
        fields.push(serde_json::json!({
            "name": "⏱️ Time to Sell",
            "value": format!("```\n{}\n```", format_duration(secs)),
            "inline": true
        }));
    }
    if let Some(uuid) = auction_uuid {
        if !uuid.is_empty() {
            fields.push(serde_json::json!({
                "name": "🔗 Auction Link",
                "value": format!("[View on Coflnet](https://sky.coflnet.com/auction/{}?refId=9KKPN9)", uuid),
                "inline": false
            }));
        }
    }
    let payload = serde_json::json!({
        "embeds": [{
            "title": format!("{} {}", status_emoji, title),
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0x0099ff,
            "fields": fields,
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_bazaar_order_placed(
    ingame_name: &str,
    item_name: &str,
    amount: u64,
    price_per_unit: f64,
    total_price: f64,
    is_buy_order: bool,
    purse: Option<u64>,
    webhook_url: &str,
) {
    let order_type = if is_buy_order { "Buy Order" } else { "Sell Offer" };
    let order_emoji = if is_buy_order { "🛒" } else { "🏷️" };
    let color: u32 = if is_buy_order { 0x00cccc } else { 0xff9900 };
    let safe_item = sanitize_item_name(item_name);
    let payload = serde_json::json!({
        "embeds": [{
            "title": format!("{} Bazaar {} Placed", order_emoji, order_type),
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": color,
            "fields": [
                {"name": "📦 Amount",       "value": format!("```fix\n{}x\n```", amount),                     "inline": true},
                {"name": "💵 Price/Unit",   "value": format!("```fix\n{} coins\n```", format_number(price_per_unit)), "inline": true},
                {"name": "💰 Total Price",  "value": format!("```fix\n{} coins\n```", format_number(total_price)),    "inline": true},
                {"name": "📊 Order Type",   "value": format!("```\n{}\n```", order_type),                     "inline": false},
            ],
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}

pub async fn send_webhook_auction_listed(
    ingame_name: &str,
    item_name: &str,
    starting_bid: u64,
    duration_hours: u64,
    purse: Option<u64>,
    webhook_url: &str,
) {
    let safe_item = sanitize_item_name(item_name);
    let expires_unix = now_unix() + duration_hours * 3600;
    let payload = serde_json::json!({
        "embeds": [{
            "title": "🏷️ BIN Auction Listed",
            "description": format!("**{}** • <t:{}:R>", item_name, now_unix()),
            "color": 0xe67e22u32,
            "fields": [
                {"name": "💵 BIN Price",  "value": format!("```fix\n{} coins\n```", format_number(starting_bid as f64)), "inline": true},
                {"name": "⏳ Duration",   "value": format!("```\n{}h\n```", duration_hours),                             "inline": true},
                {"name": "📅 Expires",    "value": format!("<t:{}:R>", expires_unix),                                    "inline": true},
            ],
            "thumbnail": {"url": format!("https://sky.coflnet.com/static/icon/{}", safe_item)},
            "footer": {
                "text": format!("BAF • {}{}", ingame_name,
                    purse.map(|p| format!(" • Purse: {} coins", format_purse(p))).unwrap_or_default()),
                "icon_url": format!("https://mc-heads.net/avatar/{}/32.png", ingame_name)
            }
        }]
    });
    post_embed(webhook_url, payload).await;
}
