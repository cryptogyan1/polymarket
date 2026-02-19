use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use serde::Deserialize;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

const SPORTS_WS_URL: &str = "wss://sports-api.polymarket.com/ws";

#[derive(Debug, Deserialize)]
struct SportsResult {
    #[serde(rename = "gameId")]
    game_id: Option<u64>,
    #[serde(rename = "leagueAbbreviation")]
    league: Option<String>,
    slug: Option<String>,
    #[serde(rename = "homeTeam")]
    home_team: Option<String>,
    #[serde(rename = "awayTeam")]
    away_team: Option<String>,
    status: Option<String>,
    score: Option<String>,
    period: Option<String>,
    elapsed: Option<String>,
    live: Option<bool>,
    ended: Option<bool>,
}

pub fn spawn_sports_scores_listener(slug_filters: Vec<String>) {
    tokio::spawn(async move {
        loop {
            if let Err(err) = connect_and_stream(&slug_filters).await {
                warn!("🏟️ Sports WS error: {} — reconnecting in 1s", err);
            }
            sleep(Duration::from_secs(1)).await;
        }
    });
}

async fn connect_and_stream(slug_filters: &[String]) -> anyhow::Result<()> {
    info!("🏟️ Connecting Sports WebSocket: {}", SPORTS_WS_URL);

    let (ws, _) = connect_async(Url::parse(SPORTS_WS_URL)?).await?;
    let (mut write, mut read) = ws.split();

    while let Some(msg) = read.next().await {
        match msg? {
            Message::Text(txt) => {
                if txt == "ping" {
                    write.send(Message::Text("pong".to_string())).await?;
                    debug!("🏟️ Sports ping/pong");
                    continue;
                }

                match serde_json::from_str::<SportsResult>(&txt) {
                    Ok(update) => log_sports_update(update, slug_filters),
                    Err(_) => debug!("🏟️ Non-result sports message ignored"),
                }
            }
            Message::Ping(data) => {
                let _ = write.send(Message::Pong(data)).await;
            }
            Message::Close(_) => {
                anyhow::bail!("sports websocket closed by server");
            }
            _ => {}
        }
    }

    anyhow::bail!("sports websocket stream ended")
}

fn log_sports_update(update: SportsResult, slug_filters: &[String]) {
    let slug = update.slug.unwrap_or_default();
    if !slug_filters.is_empty() && !slug_filters.iter().any(|x| x == &slug) {
        return;
    }

    info!(
        "🏟️ Sports update | game_id={} league={} {} vs {} | status={} period={} elapsed={} score={} live={} ended={} slug={}",
        update.game_id.map(|v| v.to_string()).unwrap_or_else(|| "?".to_string()),
        update.league.unwrap_or_else(|| "?".to_string()),
        update.home_team.unwrap_or_else(|| "?".to_string()),
        update.away_team.unwrap_or_else(|| "?".to_string()),
        update.status.unwrap_or_else(|| "?".to_string()),
        update.period.unwrap_or_else(|| "?".to_string()),
        update.elapsed.unwrap_or_else(|| "-".to_string()),
        update.score.unwrap_or_else(|| "-".to_string()),
        update.live.map(|v| v.to_string()).unwrap_or_else(|| "?".to_string()),
        update.ended.map(|v| v.to_string()).unwrap_or_else(|| "?".to_string()),
        slug,
    );
}
