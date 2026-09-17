use agentd_x_adapter::{
    agentd::Agentd,
    config::Config,
    http,
    oauth::Credentials,
    service::{retry_delay, Service},
    store::Store,
    x::XClient,
};
use anyhow::{ensure, Result};
use clap::{Parser, Subcommand};
use std::{path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Run polling, turn submission and (if explicitly enabled) outbox delivery.
    Run,
    /// Verify X identity and resolve the owner username (read-only, may incur API usage).
    Doctor {
        #[arg(default_value = "jackysp")]
        owner: String,
    },
    /// Create the dedicated tenant and x-bot agent if absent; preserve existing configuration.
    Register,
    /// Inspect local state and previews; stop the service first to acquire its state lock.
    Status {
        #[arg(long, default_value = "state/adapter.db")]
        state: PathBuf,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();
    let args = Args::parse();
    if let Command::Status { state } = args.command {
        println!(
            "{}",
            serde_json::to_string_pretty(&Store::open(&state)?.report()?)?
        );
        return Ok(());
    }
    let config = Config::from_env()?;
    let client = http::client()?;
    let x = XClient::new(
        client.clone(),
        url::Url::parse("https://api.x.com/")?,
        Credentials {
            key: config.consumer_key.clone(),
            secret: config.consumer_secret.clone(),
            token: config.access_token.clone(),
            token_secret: config.access_secret.clone(),
        },
    );
    let agentd = Agentd::new(client, &config);
    if matches!(args.command, Command::Register) {
        agentd.register().await?;
        tracing::info!("agent registered (or preserved)");
        return Ok(());
    }
    let bot = x.me().await?;
    ensure!(
        bot.username.eq_ignore_ascii_case(&config.bot_username),
        "token belongs to a different X account"
    );
    if let Command::Doctor { owner } = args.command {
        let owner = x.user(owner.trim_start_matches('@')).await?;
        println!(
            "Bot: @{} ({})\nOwner: @{} ({})\nSet X_OWNER_ID={}",
            bot.username, bot.id, owner.username, owner.id, owner.id
        );
        return Ok(());
    }
    ensure!(
        agentd_x_adapter::config::valid_id(&config.owner_id),
        "set X_OWNER_ID from doctor before running"
    );
    ensure!(
        bot.id != config.owner_id,
        "owner must not be the bot itself"
    );
    let store = Store::open(&config.state)?;
    store.bind(&format!(
        "{}|{}|{}|{}|{}",
        bot.id, config.owner_id, config.agentd_url, config.tenant, config.agent
    ))?;
    let service = Service {
        config,
        x,
        agentd,
        store,
        bot,
    };
    tracing::info!(publish = service.config.publish, "adapter started");
    let mut poll_at = tokio::time::Instant::now();
    let mut outbox_at = poll_at;
    let mut failures = 0u32;
    let shutdown = shutdown();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            result=&mut shutdown=>{result?;break;},
            _=tokio::time::sleep(Duration::from_secs(2))=>{},
        }
        if tokio::time::Instant::now() >= poll_at {
            let delay = match service.poll().await {
                Ok(()) => {
                    failures = 0;
                    service.config.poll_secs
                }
                Err(e) => {
                    failures = failures.saturating_add(1);
                    tracing::warn!(error=%e,"mentions poll failed");
                    retry_delay(&e).max(30u64.saturating_mul(1u64 << failures.min(6)))
                }
            };
            poll_at = tokio::time::Instant::now() + Duration::from_secs(delay);
        }
        if let Err(e) = service.jobs().await {
            tracing::error!(error=%e,"local processing failed");
            return Err(e);
        }
        if tokio::time::Instant::now() >= outbox_at {
            let delay = match service.deliver().await {
                Ok(()) => 5,
                Err(e) => {
                    tracing::warn!(error=%e,"outbox processing failed");
                    retry_delay(&e)
                }
            };
            outbox_at = tokio::time::Instant::now() + Duration::from_secs(delay);
        }
    }
    Ok(())
}

async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {r=tokio::signal::ctrl_c()=>r?,_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
