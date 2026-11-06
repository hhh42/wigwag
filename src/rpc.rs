//! signal-cli's JSON-RPC 2.0 surface: request parsing, typed params, error codes.
//!
//! Only the methods bots use are implemented. Everything else answers
//! "method not found" rather than pretending.

use std::future::Future;

use serde::Deserialize;
use serde_json::{Value, json};

/// JSON-RPC and signal-cli error codes. Clients key rate-limit backoff on `RATE_LIMIT`.
pub mod code {
    pub const PARSE: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const USER: i64 = -1;
    pub const IO: i64 = -3;
    pub const UNTRUSTED_IDENTITY: i64 = -4;
    pub const RATE_LIMIT: i64 = -5;
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
    pub fn params(message: impl Into<String>) -> Self {
        Self::new(code::INVALID_PARAMS, message)
    }
    pub fn user(message: impl Into<String>) -> Self {
        Self::new(code::USER, message)
    }
    fn to_json(&self) -> Value {
        let mut e = json!({ "code": self.code, "message": self.message });
        if let Some(d) = &self.data {
            e["data"] = d.clone();
        }
        e
    }
}

pub type RpcResult = Result<Value, RpcError>;

/// Where a message goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Recipients(Vec<String>),
    Group(String),
    NoteToSelf,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Quote {
    pub timestamp: u64,
    pub author: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub start: u32,
    pub length: u32,
    pub recipient: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRequest {
    pub target: Target,
    pub message: String,
    pub text_styles: Vec<String>,
    pub mentions: Vec<Mention>,
    pub attachments: Vec<String>,
    pub quote: Option<Quote>,
    pub view_once: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionRequest {
    pub target: Target,
    pub emoji: String,
    pub target_author: String,
    pub target_timestamp: u64,
    pub remove: bool,
}

/// What actually talks to Signal. The daemon implements it; tests fake it.
pub trait Backend: Send + Sync + 'static {
    fn account(&self) -> &str;
    fn send(&self, req: SendRequest) -> impl Future<Output = RpcResult> + Send;
    fn send_typing(&self, target: Target, stop: bool) -> impl Future<Output = RpcResult> + Send;
    fn send_reaction(&self, req: ReactionRequest) -> impl Future<Output = RpcResult> + Send;
    fn get_attachment(&self, id: String) -> impl Future<Output = RpcResult> + Send;
    fn list_contacts(&self) -> impl Future<Output = RpcResult> + Send;
    fn get_contact(&self, address: String) -> impl Future<Output = RpcResult> + Send;
    fn list_groups(&self) -> impl Future<Output = RpcResult> + Send;
}

/// A field that clients send either as one value or as a list.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn into_vec(v: Option<Self>) -> Vec<T> {
        match v {
            None => vec![],
            Some(Self::One(t)) => vec![t],
            Some(Self::Many(t)) => t,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct TargetParams {
    account: Option<String>,
    recipient: Option<OneOrMany<String>>,
    group_id: Option<OneOrMany<String>>,
    note_to_self: bool,
}

impl TargetParams {
    fn target(self) -> Result<Target, RpcError> {
        let recipients = OneOrMany::into_vec(self.recipient);
        let groups = OneOrMany::into_vec(self.group_id);
        match (recipients.is_empty(), groups.as_slice(), self.note_to_self) {
            (true, [], true) => Ok(Target::NoteToSelf),
            (false, [], false) => Ok(Target::Recipients(recipients)),
            (true, [g], false) => Ok(Target::Group(g.clone())),
            (true, [], false) => Err(RpcError::params(
                "one of recipient, groupId or noteToSelf is required",
            )),
            (true, [_, ..], false) => Err(RpcError::params("only one groupId per call")),
            _ => Err(RpcError::params(
                "recipient, groupId and noteToSelf are exclusive",
            )),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendParams {
    #[serde(flatten)]
    target: TargetParams,
    #[serde(default)]
    message: Option<String>,
    text_style: Option<OneOrMany<String>>,
    text_styles: Option<OneOrMany<String>>,
    mention: Option<OneOrMany<String>>,
    attachment: Option<OneOrMany<String>>,
    attachments: Option<OneOrMany<String>>,
    quote_timestamp: Option<u64>,
    quote_author: Option<String>,
    quote_message: Option<String>,
    #[serde(default)]
    view_once: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TypingParams {
    #[serde(flatten)]
    target: TargetParams,
    #[serde(default)]
    stop: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionParams {
    #[serde(flatten)]
    target: TargetParams,
    emoji: String,
    target_author: String,
    target_timestamp: u64,
    #[serde(default)]
    remove: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentParams {
    account: Option<String>,
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContactParams {
    account: Option<String>,
    #[serde(alias = "recipient")]
    contact_address: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AccountOnly {
    account: Option<String>,
}

fn parse<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, RpcError> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|e| RpcError::params(e.to_string()))
}

fn check_account(backend: &impl Backend, account: Option<&str>) -> Result<(), RpcError> {
    match account {
        Some(a) if !a.is_empty() && a != backend.account() => Err(RpcError::params(format!(
            "this daemon serves {}, not {a}",
            backend.account()
        ))),
        _ => Ok(()),
    }
}

fn send_request(p: SendParams) -> Result<SendRequest, RpcError> {
    let mut text_styles = OneOrMany::into_vec(p.text_style);
    text_styles.extend(OneOrMany::into_vec(p.text_styles));
    for s in &text_styles {
        crate::ranges::style_range(s).map_err(RpcError::params)?;
    }
    let mentions = OneOrMany::into_vec(p.mention)
        .iter()
        .map(|m| {
            let (start, length, recipient) = crate::ranges::split(m).map_err(RpcError::params)?;
            Ok(Mention {
                start,
                length,
                recipient: recipient.to_string(),
            })
        })
        .collect::<Result<_, RpcError>>()?;
    let mut attachments = OneOrMany::into_vec(p.attachment);
    attachments.extend(OneOrMany::into_vec(p.attachments));
    let quote = match (p.quote_timestamp, p.quote_author) {
        (Some(timestamp), Some(author)) => Some(Quote {
            timestamp,
            author,
            text: p.quote_message,
        }),
        (None, None) => None,
        _ => {
            return Err(RpcError::params(
                "quoteTimestamp and quoteAuthor go together",
            ));
        }
    };
    let message = p.message.unwrap_or_default();
    if message.is_empty() && attachments.is_empty() {
        return Err(RpcError::params(
            "nothing to send: no message or attachments",
        ));
    }
    Ok(SendRequest {
        target: p.target.target()?,
        message,
        text_styles,
        mentions,
        attachments,
        quote,
        view_once: p.view_once,
    })
}

/// Runs one method call.
pub async fn call(backend: &impl Backend, method: &str, params: Value) -> RpcResult {
    match method {
        "send" => {
            let p: SendParams = parse(params)?;
            check_account(backend, p.target.account.as_deref())?;
            backend.send(send_request(p)?).await
        }
        "sendTyping" => {
            let p: TypingParams = parse(params)?;
            check_account(backend, p.target.account.as_deref())?;
            backend.send_typing(p.target.target()?, p.stop).await
        }
        "sendReaction" => {
            let p: ReactionParams = parse(params)?;
            check_account(backend, p.target.account.as_deref())?;
            backend
                .send_reaction(ReactionRequest {
                    target: p.target.target()?,
                    emoji: p.emoji,
                    target_author: p.target_author,
                    target_timestamp: p.target_timestamp,
                    remove: p.remove,
                })
                .await
        }
        "getAttachment" => {
            let p: AttachmentParams = parse(params)?;
            check_account(backend, p.account.as_deref())?;
            backend.get_attachment(p.id).await
        }
        "listContacts" => {
            let p: AccountOnly = parse(params)?;
            check_account(backend, p.account.as_deref())?;
            backend.list_contacts().await
        }
        "getContact" => {
            let p: ContactParams = parse(params)?;
            check_account(backend, p.account.as_deref())?;
            backend.get_contact(p.contact_address).await
        }
        "listGroups" => {
            let p: AccountOnly = parse(params)?;
            check_account(backend, p.account.as_deref())?;
            backend.list_groups().await
        }
        "listAccounts" => Ok(json!([{ "number": backend.account() }])),
        "version" => Ok(json!({ "version": concat!("wigwag ", env!("CARGO_PKG_VERSION")) })),
        other => Err(RpcError::new(
            code::METHOD_NOT_FOUND,
            format!("method not implemented: {other}"),
        )),
    }
}

fn response(id: Value, result: RpcResult) -> Value {
    match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "result": r, "id": id }),
        Err(e) => json!({ "jsonrpc": "2.0", "error": e.to_json(), "id": id }),
    }
}

async fn one(backend: &impl Backend, req: Value) -> Option<Value> {
    let Value::Object(mut req) = req else {
        return Some(response(
            Value::Null,
            Err(RpcError::new(
                code::INVALID_REQUEST,
                "request must be an object",
            )),
        ));
    };
    let id = req.remove("id");
    let Some(Value::String(method)) = req.remove("method") else {
        return Some(response(
            id.unwrap_or(Value::Null),
            Err(RpcError::new(code::INVALID_REQUEST, "missing method")),
        ));
    };
    let params = req.remove("params").unwrap_or(Value::Null);
    let result = call(backend, &method, params).await;
    // A request without an id is a notification: run it, answer nothing.
    id.map(|id| response(id, result))
}

/// Handles a request body: one call or a batch. `None` means "reply with no content".
pub async fn handle(backend: &impl Backend, body: &[u8]) -> Option<Value> {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return Some(response(
                Value::Null,
                Err(RpcError::new(code::PARSE, e.to_string())),
            ));
        }
    };
    match parsed {
        Value::Array(reqs) if reqs.is_empty() => Some(response(
            Value::Null,
            Err(RpcError::new(code::INVALID_REQUEST, "empty batch")),
        )),
        Value::Array(reqs) => {
            let mut out = vec![];
            for r in reqs {
                out.extend(one(backend, r).await);
            }
            (!out.is_empty()).then_some(Value::Array(out))
        }
        req => one(backend, req).await,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what it was asked to do.
    #[derive(Default)]
    pub struct Fake {
        pub calls: Mutex<Vec<String>>,
        pub sent: Mutex<Vec<SendRequest>>,
    }

    impl Backend for Fake {
        fn account(&self) -> &str {
            "+15550000001"
        }
        async fn send(&self, req: SendRequest) -> RpcResult {
            self.sent.lock().unwrap().push(req);
            Ok(json!({ "timestamp": 1, "results": [{ "type": "SUCCESS" }] }))
        }
        async fn send_typing(&self, target: Target, stop: bool) -> RpcResult {
            self.calls
                .lock()
                .unwrap()
                .push(format!("typing {target:?} {stop}"));
            Ok(json!({ "timestamp": 2, "results": [] }))
        }
        async fn send_reaction(&self, req: ReactionRequest) -> RpcResult {
            self.calls
                .lock()
                .unwrap()
                .push(format!("react {} {}", req.emoji, req.remove));
            Ok(json!({ "timestamp": 3, "results": [] }))
        }
        async fn get_attachment(&self, id: String) -> RpcResult {
            Ok(json!({ "data": id }))
        }
        async fn list_contacts(&self) -> RpcResult {
            Ok(json!([]))
        }
        async fn get_contact(&self, address: String) -> RpcResult {
            Ok(json!({ "number": address }))
        }
        async fn list_groups(&self) -> RpcResult {
            Ok(json!([]))
        }
    }

    async fn rpc(fake: &Fake, req: Value) -> Value {
        handle(fake, req.to_string().as_bytes()).await.unwrap()
    }

    #[tokio::test]
    async fn sends_what_hermes_sends() {
        let fake = Fake::default();
        let r = rpc(
            &fake,
            json!({"jsonrpc": "2.0", "method": "send", "id": "send_1", "params": {
                "account": "+15550000001", "recipient": ["+15550000002"],
                "message": "hello", "textStyle": "0:5:BOLD"}}),
        )
        .await;
        assert_eq!(r["id"], "send_1");
        assert_eq!(r["result"]["results"][0]["type"], "SUCCESS");
        let sent = fake.sent.lock().unwrap();
        assert_eq!(
            sent[0].target,
            Target::Recipients(vec!["+15550000002".into()])
        );
        assert_eq!(sent[0].text_styles, vec!["0:5:BOLD".to_string()]);
    }

    #[tokio::test]
    async fn accepts_string_or_list_and_groups() {
        let fake = Fake::default();
        rpc(
            &fake,
            json!({"jsonrpc": "2.0", "method": "send", "id": 1, "params": {
                "groupId": "abc=", "message": "", "attachments": ["/tmp/a.png"],
                "textStyles": ["0:1:ITALIC", "1:1:MONOSPACE"]}}),
        )
        .await;
        let sent = fake.sent.lock().unwrap();
        assert_eq!(sent[0].target, Target::Group("abc=".into()));
        assert_eq!(sent[0].attachments, vec!["/tmp/a.png".to_string()]);
        assert_eq!(sent[0].text_styles.len(), 2);
    }

    #[tokio::test]
    async fn rejects_bad_params_with_signal_cli_codes() {
        let fake = Fake::default();
        let wrong_account = rpc(
            &fake,
            json!({"jsonrpc": "2.0", "method": "send", "id": 1,
                   "params": {"account": "+19999999999", "recipient": "+1", "message": "x"}}),
        )
        .await;
        assert_eq!(wrong_account["error"]["code"], code::INVALID_PARAMS);

        let no_target = rpc(
            &fake,
            json!({"jsonrpc": "2.0", "method": "send", "id": 2, "params": {"message": "x"}}),
        )
        .await;
        assert_eq!(no_target["error"]["code"], code::INVALID_PARAMS);

        let bad_style = rpc(
            &fake,
            json!({"jsonrpc": "2.0", "method": "send", "id": 3,
                   "params": {"recipient": "+1", "message": "x", "textStyle": "0:1:GLITTER"}}),
        )
        .await;
        assert_eq!(bad_style["error"]["code"], code::INVALID_PARAMS);

        let unknown = rpc(
            &fake,
            json!({"jsonrpc": "2.0", "method": "updateProfile", "id": 4}),
        )
        .await;
        assert_eq!(unknown["error"]["code"], code::METHOD_NOT_FOUND);

        let garbage = handle(&fake, b"{nope").await.unwrap();
        assert_eq!(garbage["error"]["code"], code::PARSE);
    }

    #[tokio::test]
    async fn typing_reactions_and_lookups() {
        let fake = Fake::default();
        rpc(
            &fake,
            json!({"jsonrpc": "2.0", "method": "sendTyping", "id": "typing-stop",
                   "params": {"recipient": ["+1"], "stop": true}}),
        )
        .await;
        rpc(
            &fake,
            json!({"jsonrpc": "2.0", "method": "sendReaction", "id": 1,
                   "params": {"recipient": ["+1"], "emoji": "", "targetAuthor": "+1",
                              "targetTimestamp": 5, "remove": true}}),
        )
        .await;
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![
                r#"typing Recipients(["+1"]) true"#.to_string(),
                "react  true".to_string()
            ]
        );
        let c = rpc(
            &fake,
            json!({"jsonrpc": "2.0", "method": "getContact", "id": 1,
                   "params": {"contactAddress": "+1"}}),
        )
        .await;
        assert_eq!(c["result"]["number"], "+1");
    }

    #[tokio::test]
    async fn batches_and_notifications() {
        let fake = Fake::default();
        let r = rpc(
            &fake,
            json!([
                {"jsonrpc": "2.0", "method": "version", "id": 1},
                {"jsonrpc": "2.0", "method": "listContacts"},
            ]),
        )
        .await;
        assert_eq!(r.as_array().unwrap().len(), 1, "notifications get no reply");
        assert!(
            handle(&fake, br#"{"jsonrpc":"2.0","method":"version"}"#)
                .await
                .is_none()
        );
    }
}
