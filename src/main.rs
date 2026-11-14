//! wigwag: a Signal daemon that speaks signal-cli's HTTP JSON-RPC API, without a JVM.
//!
//! Named after the US Army's 1860 flag code (one flag, swung left and right), which
//! needed nothing but a pole. See "Why wigwag" in the README.

mod agent;
mod attachments;
mod directory;
mod envelope;
mod events;
mod http;
mod ranges;
mod rpc;
mod signal;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::{Args, Parser, Subcommand};
use futures::channel::oneshot;
use presage::Manager;
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::prelude::phonenumber::PhoneNumber;
use presage::libsignal_service::profile_name::ProfileName;
use presage::manager::RegistrationOptions;
use presage::model::identity::OnNewIdentity;
use presage::store::StateStore;
use presage_store_sqlite::SqliteStore;
use tokio::io::AsyncBufReadExt;
use tracing::{info, warn};

use crate::attachments::Attachments;
use crate::events::Hub;
use crate::rpc::Backend;
use crate::signal::{Options, SignalManager, e164};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Where the account database and downloaded attachments live.
    #[arg(long, env = "WIGWAG_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,

    /// File holding the passphrase that encrypts the database (SQLCipher).
    #[arg(long, env = "WIGWAG_PASSPHRASE_FILE", global = true)]
    passphrase_file: Option<PathBuf>,

    /// The account's number. Optional: checked against the linked account when given,
    /// so `-a +1555…` from a signal-cli command line keeps working.
    #[arg(short = 'a', long, global = true)]
    account: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Link as a new device on an existing Signal account (scan the QR code with your phone).
    Link {
        #[arg(short = 'n', long, default_value = "wigwag")]
        device_name: String,
        /// Replace an existing registration in this data directory.
        #[arg(long)]
        force: bool,
    },
    /// Register a phone number as a new, primary Signal account.
    Register {
        /// The number, in E.164 form (+15551234567).
        number: PhoneNumber,
        /// The token from https://signalcaptchas.org/registration/generate.html
        /// (the `signalcaptcha://…` link, or just what follows it).
        #[arg(long)]
        captcha: String,
        /// Have the code read out by a phone call instead of sent by SMS.
        #[arg(long)]
        voice: bool,
        #[arg(long)]
        force: bool,
    },
    /// Serve signal-cli's HTTP API: /api/v1/rpc, /api/v1/events, /api/v1/check.
    Daemon {
        /// Address to listen on. Anything that can reach it can send as you: keep it local.
        #[arg(long, default_value = "127.0.0.1:8080")]
        http: SocketAddr,
        /// Don't serve HTTP at all (for running just the built-in agent).
        #[arg(long)]
        no_http: bool,
        /// Keep a copy of every message in the database (presage's default). Off: purged hourly.
        #[arg(long)]
        keep_messages: bool,
        /// Days to keep downloaded attachments.
        #[arg(long, default_value_t = 7)]
        attachment_ttl_days: u64,
        /// Largest attachment to download or send, in MiB.
        #[arg(long, default_value_t = 100)]
        max_attachment_mib: u64,
        /// Envelopes held for the next subscriber while none is connected.
        #[arg(long, default_value_t = 1000)]
        backlog: usize,
        /// File holding a token; when given, every request needs `Authorization: Bearer <token>`.
        #[arg(long, env = "WIGWAG_TOKEN_FILE")]
        token_file: Option<PathBuf>,
        /// Only send attachments given by path from inside this directory (data: URIs always work).
        #[arg(long, env = "WIGWAG_SEND_DIR")]
        send_dir: Option<PathBuf>,
        #[command(flatten)]
        agent: AgentArgs,
    },
    /// Send one message and exit.
    Send {
        /// Recipient: +E164, UUID, PNI:<uuid> or u:<username>. Omit with --group or --note-to-self.
        recipient: Option<String>,
        #[arg(short, long)]
        message: String,
        #[arg(short = 'g', long)]
        group: Option<String>,
        #[arg(long)]
        note_to_self: bool,
        #[arg(long = "attach")]
        attachments: Vec<String>,
    },
    /// Show the linked account.
    Whoami,
    /// Set the account's profile name (for a dedicated bot number).
    Profile {
        #[arg(long)]
        name: String,
        #[arg(long)]
        about: Option<String>,
    },
    /// Unlink this device from its account and forget it.
    Unlink,
}

/// The built-in agent: answer to a name in Signal by running a command.
#[derive(Args)]
#[command(next_help_heading = "Agent")]
struct AgentArgs {
    /// Turn the agent on, with this name; mentioning it in a chat triggers a reply.
    #[arg(long = "agent", env = "WIGWAG_AGENT_NAME", value_name = "NAME")]
    name: Option<String>,
    /// Another name it answers to (repeatable).
    #[arg(long = "agent-alias", value_name = "NAME")]
    aliases: Vec<String>,
    /// Command to run; gets the prompt on stdin and prints the reply.
    #[arg(
        long = "agent-command",
        env = "WIGWAG_AGENT_COMMAND",
        default_value = "claude -p"
    )]
    command: String,
    /// UUID or +number of someone else who may summon the agent (repeatable). The owner always can.
    #[arg(long = "allow", env = "WIGWAG_ALLOW", value_delimiter = ',')]
    allow: Vec<String>,
    /// Whose agent this is, by name.
    #[arg(long, env = "WIGWAG_OWNER", default_value = "my owner")]
    owner: String,
    /// Don't answer every Note to Self message.
    #[arg(long)]
    no_note_to_self: bool,
    /// Agent replies in a row before it waits for a person to speak.
    #[arg(long, env = "WIGWAG_MAX_AGENT_TURNS", default_value_t = 4)]
    max_agent_turns: u32,
    #[arg(long, default_value_t = 60)]
    max_replies_per_hour: usize,
    /// Recent messages given to the agent.
    #[arg(long = "agent-history", default_value_t = 20)]
    history: usize,
    /// Seconds the agent may take.
    #[arg(long = "agent-timeout", default_value_t = 300)]
    timeout: u64,
    /// File with extra instructions for the agent (persona, rules).
    #[arg(long = "agent-instructions")]
    instructions: Option<PathBuf>,
    /// Run the agent but don't send its replies.
    #[arg(long = "agent-dry-run")]
    dry_run: bool,
}

impl AgentArgs {
    fn config(&self) -> anyhow::Result<Option<agent::Config>> {
        let Some(name) = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        else {
            return Ok(None);
        };
        let command = shlex::split(&self.command)
            .filter(|c| !c.is_empty())
            .context("--agent-command is empty or badly quoted")?;
        let instructions = match &self.instructions {
            Some(p) => std::fs::read_to_string(p)
                .with_context(|| format!("cannot read {}", p.display()))?,
            None => String::new(),
        };
        let clean = |v: &[String]| -> Vec<String> {
            v.iter()
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        };
        Ok(Some(agent::Config {
            name: name.to_string(),
            command,
            aliases: clean(&self.aliases),
            allow: clean(&self.allow).into_iter().collect(),
            owner: self.owner.clone(),
            note_to_self: !self.no_note_to_self,
            max_agent_turns: self.max_agent_turns,
            max_replies_per_hour: self.max_replies_per_hour,
            history: self.history,
            timeout: Duration::from_secs(self.timeout),
            instructions,
            dry_run: self.dry_run,
        }))
    }
}

fn data_dir(cli: &Cli) -> anyhow::Result<PathBuf> {
    let dir = match &cli.data_dir {
        Some(d) => d.clone(),
        None => directories::ProjectDirs::from("", "", "wigwag")
            .context("no home directory; pass --data-dir")?
            .data_dir()
            .to_path_buf(),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

fn passphrase(cli: &Cli) -> anyhow::Result<Option<String>> {
    if let Some(path) = &cli.passphrase_file {
        let p = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        return Ok(Some(p.trim_end_matches(['\r', '\n']).to_string()));
    }
    Ok(std::env::var("WIGWAG_PASSPHRASE")
        .ok()
        .filter(|p| !p.is_empty()))
}

async fn open_store(cli: &Cli) -> anyhow::Result<SqliteStore> {
    let dir = data_dir(cli)?;
    let pass = passphrase(cli)?;
    if pass.is_none() {
        warn!("no passphrase: the database holding your Signal keys is not encrypted");
    }
    let db = dir.join("wigwag.db");
    // A bot can't press "verify": accept new safety numbers, as signal-cli's
    // --trust-new-identities=always does.
    SqliteStore::open_with_passphrase(
        db.to_str().context("non-UTF-8 data directory")?,
        pass.as_deref(),
        OnNewIdentity::Trust,
    )
    .await
    .with_context(|| format!("cannot open {}", db.display()))
}

async fn load(cli: &Cli) -> anyhow::Result<SignalManager> {
    let store = open_store(cli).await?;
    let manager = Manager::load_registered(store)
        .await
        .context("no Signal account here yet: run `wigwag link` or `wigwag register` first")?;
    if let Some(a) = &cli.account {
        let own = e164(&manager.registration_data().phone_number);
        if *a != own {
            bail!("--account {a} does not match this data directory's account {own}");
        }
    }
    Ok(manager)
}

fn options(cli: &Cli, max_mib: u64, ttl_days: u64, keep_messages: bool) -> anyhow::Result<Options> {
    Ok(Options {
        attachments: Attachments::new(data_dir(cli)?.join("attachments"), max_mib << 20)?,
        keep_messages,
        attachment_ttl: Duration::from_secs(ttl_days * 86_400),
    })
}

async fn link(cli: &Cli, device_name: String, force: bool) -> anyhow::Result<()> {
    let store = open_store(cli).await?;
    if store.is_registered().await && !force {
        bail!("already linked; `wigwag unlink` first, or pass --force to replace it");
    }
    let (tx, rx) = oneshot::channel();
    let (manager, _) = futures::future::join(
        Manager::link_secondary_device(store, SignalServers::Production, device_name, tx),
        async move {
            if let Ok(url) = rx.await {
                println!("In Signal on your phone: Settings → Linked devices → +, then scan:\n");
                qr2term::print_qr(url.to_string()).expect("render QR code");
                println!("\nOr open this link on the phone: {url}");
            }
        },
    )
    .await;
    let mut manager = manager.context("linking failed")?;
    let reg = manager.registration_data();
    println!(
        "Linked {} (ACI {}).",
        e164(&reg.phone_number),
        reg.service_ids.aci().service_id_string()
    );
    if let Err(e) = manager.request_contacts().await {
        warn!("could not ask the phone for contacts: {e}");
    }
    Ok(())
}

async fn register(
    cli: &Cli,
    number: PhoneNumber,
    captcha: String,
    voice: bool,
    force: bool,
) -> anyhow::Result<()> {
    let store = open_store(cli).await?;
    let captcha = captcha
        .trim()
        .trim_start_matches("signalcaptcha://")
        .to_string();
    let manager = Manager::register(
        store,
        RegistrationOptions {
            signal_servers: SignalServers::Production,
            phone_number: number,
            use_voice_call: voice,
            captcha: Some(&captcha),
            force,
        },
    )
    .await
    .context("registration request failed")?;
    println!("Enter the verification code Signal sent:");
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let code = lines
        .next_line()
        .await?
        .context("no verification code given")?;
    let manager = manager
        .confirm_verification_code(code.trim())
        .await
        .context("verification failed")?;
    println!(
        "Registered {}. Set a profile name with `wigwag profile --name …`.",
        e164(&manager.registration_data().phone_number)
    );
    Ok(())
}

fn read_token(path: &std::path::Path) -> anyhow::Result<String> {
    let t =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let t = t.trim().to_string();
    if t.len() < 16 {
        bail!(
            "the token in {} is under 16 characters; try `openssl rand -hex 32`",
            path.display()
        );
    }
    Ok(t)
}

async fn daemon(
    cli: &Cli,
    http: Option<SocketAddr>,
    opts: Options,
    backlog: usize,
    token: Option<String>,
    agent: Option<agent::Config>,
) -> anyhow::Result<()> {
    if http.is_none() && agent.is_none() {
        bail!("--no-http without --agent leaves nothing to do");
    }
    if let Some(addr) = http {
        if !addr.ip().is_loopback() && token.is_none() {
            warn!(
                "listening on {addr} with no --token-file: anyone who can reach it can read your messages and send as this account"
            );
        } else if token.is_none() {
            info!("no --token-file: any program on this machine can use the API");
        }
    }
    let manager = load(cli).await?;
    let hub = Arc::new(Hub::new(backlog));
    let healthy = Arc::new(AtomicBool::new(true));
    let backend = signal::start(manager, opts, hub.clone(), healthy.clone()).await;
    if let Some(cfg) = agent {
        tokio::spawn(agent::run(cfg, Arc::new(backend.clone()), hub.clone()));
    }
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let Some(addr) = http else {
        info!(account = backend.account(), "running without HTTP");
        shutdown.await;
        return Ok(());
    };
    info!(account = backend.account(), "serving on http://{addr}");
    let app = http::router(Arc::new(http::AppState {
        backend,
        hub,
        healthy,
        guard: http::Guard {
            token,
            loopback_host_only: addr.ip().is_loopback(),
        },
    }));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

async fn send_once(
    cli: &Cli,
    recipient: Option<String>,
    message: String,
    group: Option<String>,
    note_to_self: bool,
    attachments: Vec<String>,
) -> anyhow::Result<()> {
    let target = match (recipient, group, note_to_self) {
        (Some(r), None, false) => rpc::Target::Recipients(vec![r]),
        (None, Some(g), false) => rpc::Target::Group(g),
        (None, None, true) => rpc::Target::NoteToSelf,
        _ => bail!("give exactly one of: a recipient, --group, --note-to-self"),
    };
    let manager = load(cli).await?;
    let opts = options(cli, 100, 7, false)?;
    let backend = signal::start(
        manager,
        opts,
        Arc::new(Hub::new(16)),
        Arc::new(AtomicBool::new(true)),
    )
    .await;
    let result = backend
        .send(rpc::SendRequest {
            target,
            message,
            text_styles: vec![],
            mentions: vec![],
            attachments,
            quote: None,
            view_once: false,
        })
        .await
        .map_err(|e| anyhow::anyhow!("{} (code {})", e.message, e.code))?;
    println!("{result}");
    Ok(())
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match &cli.command {
        Command::Link { device_name, force } => link(&cli, device_name.clone(), *force).await,
        Command::Register {
            number,
            captcha,
            voice,
            force,
        } => register(&cli, number.clone(), captcha.clone(), *voice, *force).await,
        Command::Daemon {
            http,
            keep_messages,
            attachment_ttl_days,
            max_attachment_mib,
            backlog,
            no_http,
            token_file,
            send_dir,
            agent,
        } => {
            let mut opts = options(
                &cli,
                *max_attachment_mib,
                *attachment_ttl_days,
                *keep_messages,
            )?;
            if let Some(d) = send_dir {
                opts.attachments.restrict_sends_to(d)?;
            }
            let token = token_file.as_deref().map(read_token).transpose()?;
            let agent = agent.config()?;
            let http = (!*no_http).then_some(*http);
            daemon(&cli, http, opts, *backlog, token, agent).await
        }
        Command::Send {
            recipient,
            message,
            group,
            note_to_self,
            attachments,
        } => {
            send_once(
                &cli,
                recipient.clone(),
                message.clone(),
                group.clone(),
                *note_to_self,
                attachments.clone(),
            )
            .await
        }
        Command::Whoami => {
            let manager = load(&cli).await?;
            let reg = manager.registration_data();
            println!("number: {}", e164(&reg.phone_number));
            println!("aci:    {}", reg.service_ids.aci().service_id_string());
            println!("pni:    {}", reg.service_ids.pni().service_id_string());
            println!("device: {}", manager.device_id());
            Ok(())
        }
        Command::Profile { name, about } => {
            let mut manager = load(&cli).await?;
            manager
                .update_profile(
                    ProfileName {
                        given_name: name.clone(),
                        family_name: None,
                    },
                    about.clone(),
                    None,
                )
                .await?;
            println!("Profile updated.");
            Ok(())
        }
        Command::Unlink => {
            let manager = load(&cli).await?;
            manager.unlink_self().await?;
            let mut store = manager.store().clone();
            store.clear_registration().await?;
            println!("Unlinked and forgotten.");
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .from_env_lossy()
        .add_directive("libsignal=error".parse()?)
        .add_directive("presage=warn".parse()?)
        .add_directive("libsignal_service=warn".parse()?)
        // sqlx logs every statement at debug, and that includes `PRAGMA key = …`:
        // the database key. Never let RUST_LOG=debug print it.
        .add_directive("sqlx=warn".parse()?);
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
    let cli = Cli::parse();
    tokio::task::LocalSet::new().run_until(run(cli)).await
}
