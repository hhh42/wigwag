//! Decrypted Signal content → signal-cli's JSON envelope, the shape its clients parse.
//! Fields that would be null are left out, as signal-cli leaves them out.

use presage::libsignal_service::content::{Content, ContentBody};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::proto::body_range::{AssociatedValue, Style};
use presage::proto::sync_message::{Content as SyncContent, Sent};
use presage::proto::{
    AttachmentPointer, BodyRange, DataMessage, EditMessage, ReceiptMessage, SyncMessage,
    TypingMessage, receipt_message, typing_message,
};
use serde_json::{Map, Value, json};

use crate::attachments::attachment_id;
use crate::directory::{Directory, MasterKey};
use crate::ranges::style_name;

struct Obj(Map<String, Value>);

impl Obj {
    fn new() -> Self {
        Self(Map::new())
    }
    fn put(&mut self, k: &str, v: impl Into<Value>) -> &mut Self {
        self.0.insert(k.into(), v.into());
        self
    }
    fn opt<V: Into<Value>>(&mut self, k: &str, v: Option<V>) -> &mut Self {
        if let Some(v) = v {
            self.0.insert(k.into(), v.into());
        }
        self
    }
    fn list(&mut self, k: &str, v: Vec<Value>) -> &mut Self {
        if !v.is_empty() {
            self.0.insert(k.into(), Value::Array(v));
        }
        self
    }
    fn done(self) -> Value {
        Value::Object(self.0)
    }
}

/// Whether an attachment was downloaded, by id; only those get an `id` to fetch.
pub trait Saved {
    fn saved(&self, id: &str) -> bool;
}

impl<F: Fn(&str) -> bool> Saved for F {
    fn saved(&self, id: &str) -> bool {
        self(id)
    }
}

pub struct Converter<'a> {
    pub dir: &'a Directory,
    pub saved: &'a dyn Saved,
}

fn uuid_of(text: Option<&String>, binary: Option<&Vec<u8>>) -> Option<Uuid> {
    binary
        .and_then(|b| Uuid::from_slice(b).ok())
        .or_else(|| text.and_then(|t| Uuid::parse_str(t).ok()))
}

fn service_id_of(text: Option<&String>, binary: Option<&Vec<u8>>) -> Option<ServiceId> {
    binary
        .and_then(|b| ServiceId::parse_from_service_id_binary(b))
        .or_else(|| text.and_then(|t| ServiceId::parse_from_service_id_string(t)))
}

impl Converter<'_> {
    /// The `{"envelope": …, "account": …}` event, or `None` for content clients never see
    /// (decryption noise, call signalling, sync bookkeeping).
    pub fn event(&self, content: &Content) -> Option<Value> {
        let envelope = self.envelope(content)?;
        Some(json!({ "envelope": envelope, "account": self.dir.own_number }))
    }

    pub fn envelope(&self, content: &Content) -> Option<Value> {
        let m = &content.metadata;
        let sender = self.dir.address(&m.sender);
        let mut e = Obj::new();
        e.put("source", sender.source())
            .opt("sourceNumber", sender.number.clone())
            .put("sourceUuid", sender.uuid.clone())
            .opt("sourceName", sender.name.clone())
            .put("sourceDevice", u32::from(m.sender_device))
            .put("timestamp", m.client_timestamp.timestamp_millis())
            .put(
                "serverReceivedTimestamp",
                m.server_timestamp.timestamp_millis(),
            )
            .put(
                "serverDeliveredTimestamp",
                m.server_timestamp.timestamp_millis(),
            );

        match &content.body {
            ContentBody::DataMessage(dm) => {
                e.put("dataMessage", self.data_message(dm));
            }
            ContentBody::EditMessage(edit) => {
                e.put("editMessage", self.edit_message(edit)?);
            }
            ContentBody::SynchronizeMessage(SyncMessage {
                content: Some(SyncContent::Sent(sent)),
                ..
            }) => {
                e.put("syncMessage", json!({ "sentMessage": self.sent(sent)? }));
            }
            ContentBody::TypingMessage(t) => {
                e.put("typingMessage", self.typing(t));
            }
            ContentBody::ReceiptMessage(r) => {
                e.put(
                    "receiptMessage",
                    receipt(r, m.server_timestamp.timestamp_millis()),
                );
            }
            ContentBody::StoryMessage(story) => {
                e.put(
                    "storyMessage",
                    json!({ "allowsReplies": story.allows_replies.unwrap_or(false) }),
                );
            }
            _ => return None,
        }
        Some(e.done())
    }

    fn edit_message(&self, edit: &EditMessage) -> Option<Value> {
        let dm = edit.data_message.as_ref()?;
        Some(json!({
            "targetSentTimestamp": edit.target_sent_timestamp,
            "dataMessage": self.data_message(dm),
        }))
    }

    fn sent(&self, sent: &Sent) -> Option<Value> {
        let dm = sent
            .message
            .as_ref()
            .or_else(|| sent.edit_message.as_ref()?.data_message.as_ref())?;
        let Value::Object(mut out) = self.data_message(dm) else {
            unreachable!()
        };
        let dest = service_id_of(
            sent.destination_service_id.as_ref(),
            sent.destination_service_id_binary.as_ref(),
        )
        .map(|id| self.dir.address(&id));
        if let Some(dest) = dest {
            out.insert("destination".into(), dest.source().into());
            if let Some(n) = dest.number {
                out.insert("destinationNumber".into(), n.into());
            }
            out.insert("destinationUuid".into(), dest.uuid.into());
        }
        if let Some(ts) = sent.timestamp {
            out.insert("timestamp".into(), ts.into());
        }
        Some(Value::Object(out))
    }

    fn typing(&self, t: &TypingMessage) -> Value {
        let action = match t.action() {
            typing_message::Action::Started => "STARTED",
            typing_message::Action::Stopped => "STOPPED",
        };
        let mut o = Obj::new();
        o.put("action", action).opt("timestamp", t.timestamp).opt(
            "groupId",
            t.group_id.as_ref().map(|g| {
                use base64::prelude::*;
                BASE64_STANDARD.encode(g)
            }),
        );
        o.done()
    }

    pub fn data_message(&self, dm: &DataMessage) -> Value {
        let mut o = Obj::new();
        o.opt("timestamp", dm.timestamp)
            .opt("message", dm.body.clone())
            .put("expiresInSeconds", dm.expire_timer.unwrap_or(0))
            .put("viewOnce", dm.is_view_once.unwrap_or(false))
            .list(
                "attachments",
                dm.attachments.iter().map(|a| self.attachment(a)).collect(),
            )
            .list("mentions", self.mentions(&dm.body_ranges))
            .list("textStyles", text_styles(&dm.body_ranges));

        if let Some(q) = &dm.quote {
            let author = uuid_of(q.author_aci.as_ref(), q.author_aci_binary.as_ref())
                .map(|u| self.dir.address_of_uuid(u));
            let mut quote = Obj::new();
            quote.opt("id", q.id);
            if let Some(a) = author {
                quote
                    .put("author", a.source())
                    .opt("authorNumber", a.number.clone())
                    .put("authorUuid", a.uuid.clone());
            }
            quote.opt("text", q.text.clone()).list(
                "attachments",
                q.attachments
                    .iter()
                    .map(|a| {
                        let mut o = Obj::new();
                        o.opt("contentType", a.content_type.clone())
                            .opt("filename", a.file_name.clone());
                        o.done()
                    })
                    .collect(),
            );
            o.put("quote", quote.done());
        }

        if let Some(r) = &dm.reaction {
            let target = uuid_of(
                r.target_author_aci.as_ref(),
                r.target_author_aci_binary.as_ref(),
            )
            .map(|u| self.dir.address_of_uuid(u));
            let mut reaction = Obj::new();
            reaction
                .opt("emoji", r.emoji.clone())
                .put("isRemove", r.remove.unwrap_or(false))
                .opt("targetSentTimestamp", r.target_sent_timestamp);
            if let Some(t) = target {
                reaction
                    .put("targetAuthor", t.source())
                    .opt("targetAuthorNumber", t.number.clone())
                    .put("targetAuthorUuid", t.uuid.clone());
            }
            o.put("reaction", reaction.done());
        }

        if let Some(key) = dm
            .group_v2
            .as_ref()
            .and_then(|g| g.master_key.as_ref())
            .and_then(|k| MasterKey::try_from(k.as_slice()).ok())
        {
            let group = self.dir.group(&key);
            let mut info = Obj::new();
            info.put("groupId", group.id)
                .opt(
                    "groupName",
                    Some(group.name).filter(|n: &String| !n.is_empty()),
                )
                .opt("revision", dm.group_v2.as_ref().and_then(|g| g.revision))
                .put("type", "DELIVER");
            o.put("groupInfo", info.done());
        }

        if let Some(d) = &dm.delete {
            o.put(
                "remoteDelete",
                json!({ "timestamp": d.target_sent_timestamp }),
            );
        }
        if let Some(s) = &dm.sticker {
            let mut sticker = Obj::new();
            sticker
                .opt("packId", s.pack_id.as_ref().map(hex::encode))
                .opt("stickerId", s.sticker_id);
            o.put("sticker", sticker.done());
        }
        o.done()
    }

    fn attachment(&self, a: &AttachmentPointer) -> Value {
        let id = attachment_id(a);
        let mut o = Obj::new();
        o.put(
            "contentType",
            a.content_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".into()),
        )
        .opt("filename", a.file_name.clone())
        .opt("id", self.saved.saved(&id).then_some(id))
        .opt("size", a.size)
        .opt("width", a.width)
        .opt("height", a.height)
        .opt("caption", a.caption.clone())
        .opt("uploadTimestamp", a.upload_timestamp)
        .put("voiceNote", a.flags.unwrap_or(0) & 1 == 1);
        o.done()
    }

    fn mentions(&self, ranges: &[BodyRange]) -> Vec<Value> {
        ranges
            .iter()
            .filter_map(|r| {
                let uuid = match r.associated_value.as_ref()? {
                    AssociatedValue::MentionAci(s) => Uuid::parse_str(s).ok()?,
                    AssociatedValue::MentionAciBinary(b) => Uuid::from_slice(b).ok()?,
                    AssociatedValue::Style(_) => return None,
                };
                let a = self.dir.address_of_uuid(uuid);
                let mut o = Obj::new();
                o.put("name", a.name.clone().unwrap_or_else(|| a.source().into()))
                    .opt("number", a.number.clone())
                    .put("uuid", a.uuid.clone())
                    .put("start", r.start.unwrap_or(0))
                    .put("length", r.length.unwrap_or(0));
                Some(o.done())
            })
            .collect()
    }
}

fn text_styles(ranges: &[BodyRange]) -> Vec<Value> {
    ranges
        .iter()
        .filter_map(|r| {
            let AssociatedValue::Style(s) = r.associated_value.as_ref()? else {
                return None;
            };
            let name = style_name(Style::try_from(*s).ok()?)?;
            Some(json!({ "style": name, "start": r.start.unwrap_or(0), "length": r.length.unwrap_or(0) }))
        })
        .collect()
}

fn receipt(r: &ReceiptMessage, when: i64) -> Value {
    let kind = r.r#type();
    json!({
        "when": when,
        "isDelivery": kind == receipt_message::Type::Delivery,
        "isRead": kind == receipt_message::Type::Read,
        "isViewed": kind == receipt_message::Type::Viewed,
        "timestamps": r.timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use presage::libsignal_service::content::Metadata;
    use presage::libsignal_service::protocol::{Aci, DeviceId};
    use presage::proto::data_message::{Quote, Reaction};
    use presage::proto::{GroupContextV2, StoryMessage};

    const OWN: &str = "+15550000001";

    fn uuid(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn dir() -> Directory {
        let mut d = Directory::new(OWN, uuid(1));
        d.add_contact(uuid(2), Some("+15550000002".into()), Some("Ada".into()));
        d.add_group([9; 32], "Kitchen");
        d
    }

    fn content(sender: u8, body: impl Into<ContentBody>) -> Content {
        let at = chrono::Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        Content {
            metadata: Metadata {
                sender: Aci::from(uuid(sender)).into(),
                sender_device: DeviceId::new(1).unwrap(),
                destination: Aci::from(uuid(1)).into(),
                server_guid: None,
                client_timestamp: at,
                server_timestamp: at,
                needs_receipt: false,
                unidentified_sender: true,
                was_plaintext: false,
                pni_verified: None,
            },
            body: body.into(),
        }
    }

    fn convert(c: &Content) -> Value {
        let d = dir();
        let saved = |_: &str| true;
        Converter {
            dir: &d,
            saved: &saved,
        }
        .event(c)
        .expect("an event")
    }

    #[test]
    fn a_direct_message_reads_like_signal_cli() {
        let dm = DataMessage {
            body: Some("hi there".into()),
            timestamp: Some(1_700_000_000_000),
            body_ranges: vec![crate::ranges::style_range("0:2:BOLD").unwrap()],
            ..Default::default()
        };
        let ev = convert(&content(2, dm));
        assert_eq!(ev["account"], OWN);
        let e = &ev["envelope"];
        assert_eq!(e["source"], "+15550000002");
        assert_eq!(e["sourceNumber"], "+15550000002");
        assert_eq!(e["sourceUuid"], uuid(2).to_string());
        assert_eq!(e["sourceName"], "Ada");
        assert_eq!(e["timestamp"], 1_700_000_000_000i64);
        assert_eq!(e["dataMessage"]["message"], "hi there");
        assert_eq!(e["dataMessage"]["textStyles"][0]["style"], "BOLD");
        assert!(e["dataMessage"].get("groupInfo").is_none());
    }

    #[test]
    fn unknown_senders_are_keyed_by_uuid() {
        let ev = convert(&content(
            5,
            DataMessage {
                body: Some("x".into()),
                ..Default::default()
            },
        ));
        assert_eq!(ev["envelope"]["source"], uuid(5).to_string());
        assert!(ev["envelope"].get("sourceNumber").is_none());
    }

    #[test]
    fn note_to_self_arrives_as_a_sync_sent_message_to_our_number() {
        let sent = Sent {
            destination_service_id: Some(uuid(1).to_string()),
            timestamp: Some(42),
            message: Some(DataMessage {
                body: Some("lights off".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let body = SyncMessage {
            content: Some(SyncContent::Sent(sent)),
            ..Default::default()
        };
        let ev = convert(&content(1, body));
        let s = &ev["envelope"]["syncMessage"]["sentMessage"];
        assert_eq!(s["destinationNumber"], OWN);
        assert_eq!(s["destination"], OWN);
        assert_eq!(s["message"], "lights off");
        assert_eq!(s["timestamp"], 42);
        assert_eq!(ev["envelope"]["source"], OWN);
    }

    #[test]
    fn groups_quotes_reactions_and_mentions() {
        let dm = DataMessage {
            body: Some("\u{FFFC} look".into()),
            group_v2: Some(GroupContextV2 {
                master_key: Some(vec![9; 32]),
                revision: Some(3),
                ..Default::default()
            }),
            body_ranges: vec![crate::ranges::mention_range(0, 1, uuid(1))],
            quote: Some(Quote {
                id: Some(77),
                author_aci_binary: Some(uuid(2).as_bytes().to_vec()),
                text: Some("earlier".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let d = &convert(&content(2, dm))["envelope"]["dataMessage"];
        assert_eq!(d["groupInfo"]["groupName"], "Kitchen");
        assert_eq!(d["groupInfo"]["revision"], 3);
        assert_eq!(
            d["groupInfo"]["groupId"],
            crate::directory::group_id(&[9; 32])
        );
        assert_eq!(d["mentions"][0]["number"], OWN);
        assert_eq!(d["mentions"][0]["start"], 0);
        assert_eq!(d["quote"]["id"], 77);
        assert_eq!(d["quote"]["authorNumber"], "+15550000002");
        assert_eq!(d["quote"]["text"], "earlier");

        let react = DataMessage {
            reaction: Some(Reaction {
                emoji: Some("👍".into()),
                target_author_aci: Some(uuid(1).to_string()),
                target_sent_timestamp: Some(5),
                ..Default::default()
            }),
            ..Default::default()
        };
        let r = &convert(&content(2, react))["envelope"]["dataMessage"]["reaction"];
        assert_eq!(r["emoji"], "👍");
        assert_eq!(r["targetAuthor"], OWN);
        assert_eq!(r["isRemove"], false);
    }

    #[test]
    fn attachments_get_an_id_only_once_downloaded() {
        let dm = DataMessage {
            attachments: vec![AttachmentPointer {
                content_type: Some("image/jpeg".into()),
                digest: Some(vec![1]),
                size: Some(10),
                ..Default::default()
            }],
            ..Default::default()
        };
        let c = content(2, dm);
        let d = dir();
        let got = Converter {
            dir: &d,
            saved: &|_: &str| true,
        }
        .event(&c)
        .unwrap();
        let a = &got["envelope"]["dataMessage"]["attachments"][0];
        assert!(a["id"].as_str().unwrap().ends_with(".jpg"));
        assert_eq!(a["size"], 10);

        let missing = Converter {
            dir: &d,
            saved: &|_: &str| false,
        }
        .event(&c)
        .unwrap();
        assert!(
            missing["envelope"]["dataMessage"]["attachments"][0]
                .get("id")
                .is_none()
        );
    }

    #[test]
    fn edits_typing_receipts_and_stories() {
        let edit = EditMessage {
            target_sent_timestamp: Some(9),
            data_message: Some(DataMessage {
                body: Some("fixed".into()),
                ..Default::default()
            }),
        };
        let e = convert(&content(2, edit));
        assert_eq!(
            e["envelope"]["editMessage"]["dataMessage"]["message"],
            "fixed"
        );

        let typing = TypingMessage {
            action: Some(typing_message::Action::Started.into()),
            timestamp: Some(1),
            group_id: None,
        };
        assert_eq!(
            convert(&content(2, typing))["envelope"]["typingMessage"]["action"],
            "STARTED"
        );

        let receipt = ReceiptMessage {
            r#type: Some(receipt_message::Type::Read.into()),
            timestamp: vec![1, 2],
        };
        let r = &convert(&content(2, receipt))["envelope"]["receiptMessage"];
        assert_eq!(r["isRead"], true);
        assert_eq!(r["timestamps"], json!([1, 2]));

        let story = convert(&content(2, StoryMessage::default()));
        assert!(story["envelope"].get("storyMessage").is_some());
    }
}
