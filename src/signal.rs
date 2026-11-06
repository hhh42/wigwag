//! The Signal side: one presage manager, a receive loop, and a job queue.
//!
//! presage's futures are not `Send` (it uses `spawn_local` inside), so everything that
//! touches the manager runs on a `LocalSet`. HTTP handlers reach it through
//! [`SignalHandle`], which ships closures over a channel and awaits their result.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::prelude::*;
use futures::future::LocalBoxFuture;
use futures::{StreamExt, pin_mut};
use presage::Manager;
use presage::libsignal_service::content::{Content, ContentBody};
use presage::libsignal_service::prelude::phonenumber::{self, PhoneNumber};
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::sender::AttachmentSpec;
use presage::manager::Registered;
use presage::model::messages::Received;
use presage::proto::data_message::{Quote, Reaction};
use presage::proto::sync_message::Content as SyncContent;
use presage::proto::{
    AttachmentPointer, DataMessage, GroupContextV2, SyncMessage, TypingMessage, typing_message,
};
use presage::store::ContentsStore;
use presage_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error, info, warn};

use crate::attachments::{Attachments, attachment_id};
use crate::directory::{Directory, MasterKey};
use crate::envelope::Converter;
use crate::events::Hub;
use crate::rpc::{Backend, ReactionRequest, RpcError, RpcResult, SendRequest, Target, code};

pub type SignalManager = Manager<SqliteStore, Registered>;

pub struct Options {
    pub attachments: Attachments,
    pub keep_messages: bool,
    pub attachment_ttl: Duration,
}

pub struct Ctx {
    manager: SignalManager,
    dir: RefCell<Directory>,
    files: Attachments,
    ready: watch::Receiver<bool>,
}

type Job = Box<dyn FnOnce(Rc<Ctx>) -> LocalBoxFuture<'static, ()> + Send>;

#[derive(Clone)]
pub struct SignalHandle {
    jobs: mpsc::UnboundedSender<Job>,
    account: Arc<str>,
}

pub fn e164(number: &PhoneNumber) -> String {
    number.format().mode(phonenumber::Mode::E164).to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Reads contacts and groups out of the store.
async fn load_directory(manager: &SignalManager) -> Directory {
    let reg = manager.registration_data();
    let mut dir = Directory::new(e164(&reg.phone_number), reg.service_ids.aci().into());
    match manager.store().contacts().await {
        Ok(contacts) => {
            for c in contacts.flatten() {
                dir.add_contact(
                    c.uuid,
                    c.phone_number.as_ref().map(e164),
                    Some(c.name.clone()),
                );
            }
        }
        Err(e) => warn!("could not read contacts: {e}"),
    }
    match manager.store().groups().await {
        Ok(groups) => {
            for (key, group) in groups.flatten() {
                dir.add_group(key, group.title);
            }
        }
        Err(e) => warn!("could not read groups: {e}"),
    }
    dir
}

/// Starts the receive loop, job runner and housekeeping. Call inside a `LocalSet`.
pub async fn start(
    manager: SignalManager,
    opts: Options,
    hub: Arc<Hub>,
    healthy: Arc<AtomicBool>,
) -> SignalHandle {
    let dir = load_directory(&manager).await;
    let account: Arc<str> = dir.own_number.clone().into();
    let (ready_tx, ready_rx) = watch::channel(false);
    let ctx = Rc::new(Ctx {
        manager,
        dir: RefCell::new(dir),
        files: opts.attachments,
        ready: ready_rx,
    });

    tokio::task::spawn_local(receive_loop(ctx.clone(), hub, healthy, ready_tx));
    tokio::task::spawn_local(housekeeping(
        ctx.clone(),
        opts.keep_messages,
        opts.attachment_ttl,
    ));

    let (jobs, mut rx) = mpsc::unbounded_channel::<Job>();
    tokio::task::spawn_local(async move {
        while let Some(job) = rx.recv().await {
            tokio::task::spawn_local(job(ctx.clone()));
        }
    });
    SignalHandle { jobs, account }
}

fn is_fatal(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("authorization failed") || e.contains("relink") || e.contains("not yet registered")
}

async fn receive_loop(
    ctx: Rc<Ctx>,
    hub: Arc<Hub>,
    healthy: Arc<AtomicBool>,
    ready: watch::Sender<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let mut manager = ctx.manager.clone();
        match manager.receive_messages().await {
            Ok(messages) => {
                healthy.store(true, Ordering::Relaxed);
                info!("connected to Signal");
                pin_mut!(messages);
                while let Some(item) = messages.next().await {
                    match item {
                        Received::QueueEmpty => {
                            if !*ready.borrow() {
                                info!("caught up with queued messages");
                            }
                            ready.send_replace(true);
                        }
                        Received::Contacts => {
                            info!("contacts synced");
                            *ctx.dir.borrow_mut() = load_directory(&ctx.manager).await;
                        }
                        Received::Content(content) => handle_content(&ctx, &hub, &content).await,
                        Received::DecryptionError(id) => {
                            warn!(sender = %id.service_id_string(), "could not decrypt a message")
                        }
                    }
                }
                warn!("Signal connection closed; reconnecting");
            }
            Err(e) => {
                let msg = e.to_string();
                error!("cannot receive from Signal: {msg}");
                if is_fatal(&msg) {
                    healthy.store(false, Ordering::Relaxed);
                    error!("this device looks unlinked or deregistered; run `wigwag link` again");
                }
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

fn data_messages(content: &Content) -> Vec<&DataMessage> {
    match &content.body {
        ContentBody::DataMessage(dm) => vec![dm],
        ContentBody::EditMessage(e) => e.data_message.iter().collect(),
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(SyncContent::Sent(sent)),
            ..
        }) => sent
            .message
            .iter()
            .chain(
                sent.edit_message
                    .iter()
                    .filter_map(|e| e.data_message.as_ref()),
            )
            .collect(),
        _ => vec![],
    }
}

async fn download(ctx: &Ctx, pointer: &AttachmentPointer) {
    let id = attachment_id(pointer);
    if ctx.files.exists(&id) {
        return;
    }
    if u64::from(pointer.size.unwrap_or(0)) > ctx.files.max_bytes {
        warn!(%id, size = pointer.size, "attachment over the size limit; not downloading");
        return;
    }
    match ctx.manager.get_attachment(pointer).await {
        Ok(data) => {
            if let Err(e) = ctx.files.save(&id, &data).await {
                error!(%id, "could not save attachment: {e}");
            }
        }
        Err(e) => warn!(%id, "could not download attachment: {e}"),
    }
}

async fn handle_content(ctx: &Ctx, hub: &Hub, content: &Content) {
    let mut unknown_group = false;
    for dm in data_messages(content) {
        for pointer in &dm.attachments {
            download(ctx, pointer).await;
        }
        if let Some(key) = dm
            .group_v2
            .as_ref()
            .and_then(|g| g.master_key.as_deref())
            .and_then(|k| MasterKey::try_from(k).ok())
        {
            unknown_group |= ctx.dir.borrow().group(&key).name.is_empty();
        }
    }
    if unknown_group {
        *ctx.dir.borrow_mut() = load_directory(&ctx.manager).await;
    }
    let dir = ctx.dir.borrow();
    let saved = |id: &str| ctx.files.exists(id);
    let converter = Converter {
        dir: &dir,
        saved: &saved,
    };
    match converter.event(content) {
        Some(event) => {
            debug!(
                "envelope from {}",
                content.metadata.sender.service_id_string()
            );
            hub.publish(event.to_string().into());
        }
        None => debug!("not forwarded: {:?}", std::mem::discriminant(&content.body)),
    }
}

async fn housekeeping(ctx: Rc<Ctx>, keep_messages: bool, ttl: Duration) {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tick.tick().await;
        let pruned = ctx.files.prune(ttl);
        if pruned > 0 {
            info!("deleted {pruned} old attachments");
        }
        if !keep_messages {
            // presage keeps every message; a bot needs none of them once delivered.
            let mut store = ctx.manager.store().clone();
            if let Err(e) = store.clear_messages().await {
                warn!("could not clear stored messages: {e}");
            }
        }
    }
}

/// Maps a presage error to the JSON-RPC error signal-cli would give.
pub fn rpc_error(error: impl std::fmt::Display) -> RpcError {
    let text = error.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("rate limit") || text.contains("429") {
        let retry: Option<u64> = lower
            .split("retry after")
            .nth(1)
            .and_then(|s| s.trim().split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|s| s.parse().ok());
        let mut result = json!({ "type": "RATE_LIMIT_FAILURE" });
        if let Some(r) = retry {
            result["retryAfterSeconds"] = r.into();
        }
        return RpcError {
            code: code::RATE_LIMIT,
            message: format!("RateLimitException: {text}"),
            data: Some(json!({ "response": { "results": [result] } })),
        };
    }
    if lower.contains("untrusted identity") {
        return RpcError::new(code::UNTRUSTED_IDENTITY, text);
    }
    if lower.contains("not found") || lower.contains("unknown recipient") {
        return RpcError::user(text);
    }
    RpcError::new(code::IO, text)
}

fn failure_type(e: &RpcError) -> &'static str {
    match e.code {
        code::RATE_LIMIT => "RATE_LIMIT_FAILURE",
        code::UNTRUSTED_IDENTITY => "IDENTITY_FAILURE",
        code::USER => "UNREGISTERED_FAILURE",
        _ => "NETWORK_FAILURE",
    }
}

impl Ctx {
    /// Sends only after queued messages are processed, so sessions and keys are current.
    async fn wait_ready(&self) {
        let mut ready = self.ready.clone();
        if tokio::time::timeout(Duration::from_secs(30), ready.wait_for(|r| *r))
            .await
            .is_err()
        {
            warn!("still catching up with Signal after 30s; sending anyway");
        }
    }

    fn resolve(&self, recipient: &str) -> Result<ServiceId, RpcError> {
        self.dir.borrow().resolve(recipient).map_err(RpcError::user)
    }

    async fn resolve_any(&self, recipient: &str) -> Result<ServiceId, RpcError> {
        if let Some(name) = recipient.strip_prefix("u:") {
            let mut m = self.manager.clone();
            return m
                .lookup_username(name)
                .await
                .map_err(rpc_error)?
                .map(ServiceId::from)
                .ok_or_else(|| RpcError::user(format!("no Signal account with username {name}")));
        }
        self.resolve(recipient)
    }

    async fn group_key(&self, group_id: &str) -> Result<MasterKey, RpcError> {
        if let Some(k) = self.dir.borrow().master_key(group_id) {
            return Ok(k);
        }
        *self.dir.borrow_mut() = load_directory(&self.manager).await;
        self.dir
            .borrow()
            .master_key(group_id)
            .ok_or_else(|| RpcError::user(format!("unknown group {group_id}")))
    }

    fn own(&self) -> ServiceId {
        self.manager.registration_data().service_ids.aci().into()
    }

    fn address_json(&self, id: &ServiceId) -> Value {
        let a = self.dir.borrow().address(id);
        json!({ "uuid": a.uuid, "number": a.number })
    }

    /// Delivers one content body to a target; per-recipient results, as signal-cli reports them.
    async fn deliver(
        &self,
        target: &Target,
        body: ContentBody,
        timestamp: u64,
    ) -> Result<Vec<Value>, RpcError> {
        let mut m = self.manager.clone();
        match target {
            Target::Group(id) => {
                let key = self.group_key(id).await?;
                let body = match body {
                    ContentBody::DataMessage(mut dm) => {
                        let revision = m
                            .store()
                            .group(key)
                            .await
                            .ok()
                            .flatten()
                            .map(|g| g.revision);
                        dm.group_v2 = Some(GroupContextV2 {
                            master_key: Some(key.to_vec()),
                            revision: revision.or(Some(0)),
                            ..Default::default()
                        });
                        ContentBody::DataMessage(dm)
                    }
                    other => other,
                };
                m.send_message_to_group(&key, body, timestamp)
                    .await
                    .map_err(rpc_error)?;
                Ok(vec![])
            }
            Target::NoteToSelf | Target::Recipients(_) => {
                let ids = match target {
                    Target::Recipients(rs) => {
                        let mut ids = vec![];
                        for r in rs {
                            ids.push(self.resolve_any(r).await?);
                        }
                        ids
                    }
                    _ => vec![self.own()],
                };
                let mut results = vec![];
                let mut first_error = None;
                for id in &ids {
                    let r = m.send_message(*id, body.clone(), timestamp).await;
                    let mut result = json!({ "recipientAddress": self.address_json(id) });
                    match r {
                        Ok(()) => result["type"] = "SUCCESS".into(),
                        Err(e) => {
                            let e = rpc_error(e);
                            warn!(to = %id.service_id_string(), "send failed: {}", e.message);
                            result["type"] = failure_type(&e).into();
                            first_error.get_or_insert(e);
                        }
                    }
                    results.push(result);
                }
                match first_error {
                    // All failed: an error, so the client's retry logic engages.
                    Some(e) if results.iter().all(|r| r["type"] != "SUCCESS") => Err(e),
                    _ => Ok(results),
                }
            }
        }
    }

    async fn send(&self, req: SendRequest) -> RpcResult {
        self.wait_ready().await;
        let timestamp = now_ms();

        let mut specs = vec![];
        for a in &req.attachments {
            let out = self.files.outgoing(a).await.map_err(RpcError::user)?;
            specs.push((
                AttachmentSpec {
                    content_type: out.content_type,
                    length: out.data.len(),
                    file_name: out.file_name,
                    preview: None,
                    voice_note: None,
                    borderless: None,
                    width: None,
                    height: None,
                    caption: None,
                    blur_hash: None,
                },
                out.data,
            ));
        }
        let attachments = self
            .manager
            .upload_attachments(specs)
            .await
            .map_err(rpc_error)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(rpc_error)?;

        let mut body_ranges = vec![];
        for s in &req.text_styles {
            body_ranges.push(crate::ranges::style_range(s).map_err(RpcError::params)?);
        }
        for mention in &req.mentions {
            let id = self.resolve_any(&mention.recipient).await?;
            body_ranges.push(crate::ranges::mention_range(
                mention.start,
                mention.length,
                id.raw_uuid(),
            ));
        }

        let quote = match &req.quote {
            Some(q) => {
                let author = self.resolve_any(&q.author).await?;
                Some(Quote {
                    id: Some(q.timestamp),
                    author_aci: Some(author.raw_uuid().to_string()),
                    text: q.text.clone(),
                    ..Default::default()
                })
            }
            None => None,
        };

        let dm = DataMessage {
            body: Some(req.message.clone()).filter(|m| !m.is_empty()),
            attachments,
            body_ranges,
            quote,
            is_view_once: req.view_once.then_some(true),
            timestamp: Some(timestamp),
            ..Default::default()
        };
        let results = self
            .deliver(&req.target, ContentBody::DataMessage(dm), timestamp)
            .await?;
        Ok(json!({ "timestamp": timestamp, "results": results }))
    }

    async fn typing(&self, target: Target, stop: bool) -> RpcResult {
        let timestamp = now_ms();
        let own = self.own();
        if let Target::Recipients(rs) = &target
            && rs.iter().all(|r| self.resolve(r).is_ok_and(|id| id == own))
        {
            // Nobody sees typing in Note to Self.
            return Ok(json!({ "timestamp": timestamp, "results": [] }));
        }
        if matches!(target, Target::NoteToSelf) {
            return Ok(json!({ "timestamp": timestamp, "results": [] }));
        }
        let group_id = match &target {
            Target::Group(id) => Some(
                BASE64_STANDARD
                    .decode(id)
                    .map_err(|_| RpcError::params("groupId is not base64"))?,
            ),
            _ => None,
        };
        let action = if stop {
            typing_message::Action::Stopped
        } else {
            typing_message::Action::Started
        };
        let msg = TypingMessage {
            timestamp: Some(timestamp),
            action: Some(action.into()),
            group_id,
        };
        let results = self
            .deliver(&target, ContentBody::TypingMessage(msg), timestamp)
            .await?;
        Ok(json!({ "timestamp": timestamp, "results": results }))
    }

    async fn reaction(&self, req: ReactionRequest) -> RpcResult {
        self.wait_ready().await;
        let timestamp = now_ms();
        let author = self.resolve_any(&req.target_author).await?;
        let dm = DataMessage {
            reaction: Some(Reaction {
                emoji: Some(req.emoji.clone()),
                remove: Some(req.remove),
                target_author_aci: Some(author.raw_uuid().to_string()),
                target_sent_timestamp: Some(req.target_timestamp),
                ..Default::default()
            }),
            timestamp: Some(timestamp),
            ..Default::default()
        };
        let results = self
            .deliver(&req.target, ContentBody::DataMessage(dm), timestamp)
            .await?;
        Ok(json!({ "timestamp": timestamp, "results": results }))
    }

    async fn attachment(&self, id: String) -> RpcResult {
        let data = self
            .files
            .read(&id)
            .await
            .map_err(|e| RpcError::user(format!("attachment {id}: {e}")))?;
        Ok(json!({ "data": BASE64_STANDARD.encode(data) }))
    }

    fn contacts(&self) -> RpcResult {
        let dir = self.dir.borrow();
        let list: Vec<Value> = dir
            .contacts()
            .map(|a| json!({ "number": a.number, "uuid": a.uuid, "name": a.name, "profileName": a.name }))
            .collect();
        Ok(Value::Array(list))
    }

    fn contact(&self, address: &str) -> RpcResult {
        let id = self.resolve(address)?;
        let a = self.dir.borrow().address(&id);
        Ok(json!({ "number": a.number, "uuid": a.uuid, "name": a.name, "profileName": a.name }))
    }

    fn groups(&self) -> RpcResult {
        let dir = self.dir.borrow();
        let list: Vec<Value> = dir
            .groups()
            .map(|(_, g)| json!({ "id": g.id, "name": g.name, "isMember": true }))
            .collect();
        Ok(Value::Array(list))
    }
}

impl SignalHandle {
    async fn run<F, Fut>(&self, f: F) -> RpcResult
    where
        F: FnOnce(Rc<Ctx>) -> Fut + Send + 'static,
        Fut: Future<Output = RpcResult> + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: Job = Box::new(move |ctx| {
            Box::pin(async move {
                let _ = tx.send(f(ctx).await);
            })
        });
        self.jobs
            .send(job)
            .map_err(|_| RpcError::new(code::IO, "signal worker stopped"))?;
        rx.await
            .map_err(|_| RpcError::new(code::IO, "signal worker dropped the request"))?
    }
}

impl Backend for SignalHandle {
    fn account(&self) -> &str {
        &self.account
    }
    async fn send(&self, req: SendRequest) -> RpcResult {
        self.run(move |c| async move { c.send(req).await }).await
    }
    async fn send_typing(&self, target: Target, stop: bool) -> RpcResult {
        self.run(move |c| async move { c.typing(target, stop).await })
            .await
    }
    async fn send_reaction(&self, req: ReactionRequest) -> RpcResult {
        self.run(move |c| async move { c.reaction(req).await })
            .await
    }
    async fn get_attachment(&self, id: String) -> RpcResult {
        self.run(move |c| async move { c.attachment(id).await })
            .await
    }
    async fn list_contacts(&self) -> RpcResult {
        self.run(|c| async move { c.contacts() }).await
    }
    async fn get_contact(&self, address: String) -> RpcResult {
        self.run(move |c| async move { c.contact(&address) }).await
    }
    async fn list_groups(&self) -> RpcResult {
        self.run(|c| async move { c.groups() }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_map_to_signal_cli_code_with_retry_after() {
        let e = rpc_error("libsignal-service error: Rate limit exceeded: retry after 4");
        assert_eq!(e.code, code::RATE_LIMIT);
        assert!(e.message.starts_with("RateLimitException"));
        assert_eq!(
            e.data.unwrap()["response"]["results"][0]["retryAfterSeconds"],
            4
        );
        let unspecified = rpc_error("Rate limit exceeded: retry after unspecified");
        assert_eq!(unspecified.code, code::RATE_LIMIT);
    }

    #[test]
    fn other_errors() {
        assert_eq!(
            rpc_error("Untrusted identity key with Aci(..)").code,
            code::UNTRUSTED_IDENTITY
        );
        assert_eq!(rpc_error("Recipient not found: Aci(..)").code, code::USER);
        assert_eq!(rpc_error("Websocket closing: x").code, code::IO);
        assert!(is_fatal("libsignal-service error: Authorization failed"));
        assert!(!is_fatal("Websocket closing"));
    }
}
