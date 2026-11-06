//! Who is who: phone numbers, names and groups, resolved from the store.
//!
//! Envelopes from Signal carry service IDs (UUIDs), never phone numbers. signal-cli
//! clients key chats by number when one is known, so the number comes from here.

use std::collections::HashMap;

use base64::prelude::*;
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::{Aci, Pni, ServiceId};
use presage::libsignal_service::zkgroup::groups::{GroupMasterKey, GroupSecretParams};

pub type MasterKey = [u8; 32];

#[derive(Debug, Clone)]
pub struct GroupEntry {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct Directory {
    pub own_number: String,
    numbers: HashMap<Uuid, String>,
    names: HashMap<Uuid, String>,
    by_number: HashMap<String, Uuid>,
    groups: HashMap<MasterKey, GroupEntry>,
    by_group_id: HashMap<String, MasterKey>,
}

/// A recipient as a signal-cli client names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub uuid: String,
    pub number: Option<String>,
    pub name: Option<String>,
}

impl Address {
    /// `source` in signal-cli's envelope: the number when known, else the UUID.
    pub fn source(&self) -> &str {
        self.number.as_deref().unwrap_or(&self.uuid)
    }
}

/// The ID signal-cli uses for a v2 group: base64 of the identifier derived from the
/// master key. The master key itself grants access, so it never leaves the daemon.
pub fn group_id(master_key: &MasterKey) -> String {
    let params = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(*master_key));
    BASE64_STANDARD.encode(params.get_group_identifier())
}

impl Directory {
    pub fn new(own_number: impl Into<String>, own_aci: Uuid) -> Self {
        let mut dir = Self {
            own_number: own_number.into(),
            ..Default::default()
        };
        let number = dir.own_number.clone();
        dir.add_contact(own_aci, Some(number), None);
        dir
    }

    pub fn add_contact(&mut self, uuid: Uuid, number: Option<String>, name: Option<String>) {
        if let Some(number) = number.filter(|n| !n.is_empty()) {
            self.by_number.insert(number.clone(), uuid);
            self.numbers.insert(uuid, number);
        }
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            self.names.insert(uuid, name);
        }
    }

    pub fn add_group(&mut self, master_key: MasterKey, name: impl Into<String>) {
        let id = group_id(&master_key);
        self.by_group_id.insert(id.clone(), master_key);
        self.groups.insert(
            master_key,
            GroupEntry {
                id,
                name: name.into(),
            },
        );
    }

    pub fn contacts(&self) -> impl Iterator<Item = Address> + '_ {
        let mut uuids: Vec<_> = self.numbers.keys().chain(self.names.keys()).collect();
        uuids.sort();
        uuids.dedup();
        uuids.into_iter().map(|u| self.address_of_uuid(*u))
    }

    pub fn groups(&self) -> impl Iterator<Item = (&MasterKey, &GroupEntry)> {
        self.groups.iter()
    }

    pub fn group(&self, master_key: &MasterKey) -> GroupEntry {
        self.groups
            .get(master_key)
            .cloned()
            .unwrap_or_else(|| GroupEntry {
                id: group_id(master_key),
                name: String::new(),
            })
    }

    pub fn master_key(&self, group_id: &str) -> Option<MasterKey> {
        self.by_group_id.get(group_id).copied()
    }

    pub fn address_of_uuid(&self, uuid: Uuid) -> Address {
        Address {
            uuid: uuid.to_string(),
            number: self.numbers.get(&uuid).cloned(),
            name: self.names.get(&uuid).cloned(),
        }
    }

    pub fn address(&self, id: &ServiceId) -> Address {
        match id {
            ServiceId::Aci(_) => self.address_of_uuid(id.raw_uuid()),
            ServiceId::Pni(_) => Address {
                uuid: id.service_id_string(),
                number: None,
                name: None,
            },
        }
    }

    /// Parses what a client calls a recipient: `+E164`, an ACI UUID, or `PNI:<uuid>`.
    /// Usernames (`u:`) need the network and are resolved by the caller.
    pub fn resolve(&self, recipient: &str) -> Result<ServiceId, String> {
        let r = recipient.trim();
        if r.starts_with('+') {
            return self
                .by_number
                .get(r)
                .map(|u| ServiceId::Aci(Aci::from(*u)))
                .ok_or_else(|| {
                    format!(
                        "{r} is not a known contact; Signal only reveals numbers that are shared, so use their UUID"
                    )
                });
        }
        if let Some(rest) = r.strip_prefix("PNI:") {
            return Uuid::parse_str(rest)
                .map(|u| ServiceId::Pni(Pni::from(u)))
                .map_err(|_| format!("invalid PNI: {r}"));
        }
        Uuid::parse_str(r)
            .map(|u| ServiceId::Aci(Aci::from(u)))
            .map_err(|_| format!("invalid recipient: {r}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    #[test]
    fn resolves_numbers_uuids_and_pnis() {
        let mut dir = Directory::new("+15550000001", uuid(1));
        dir.add_contact(uuid(2), Some("+15550000002".into()), Some("Ada".into()));

        assert_eq!(
            dir.resolve("+15550000001").unwrap().raw_uuid(),
            uuid(1),
            "own number"
        );
        assert_eq!(dir.resolve("+15550000002").unwrap().raw_uuid(), uuid(2));
        assert_eq!(
            dir.resolve(&uuid(3).to_string()).unwrap().raw_uuid(),
            uuid(3)
        );
        assert!(matches!(
            dir.resolve(&format!("PNI:{}", uuid(4))).unwrap(),
            ServiceId::Pni(_)
        ));
        assert!(dir.resolve("+15559999999").is_err());
        assert!(dir.resolve("nonsense").is_err());
    }

    #[test]
    fn addresses_prefer_the_number_as_source() {
        let mut dir = Directory::new("+15550000001", uuid(1));
        dir.add_contact(uuid(2), Some("+15550000002".into()), Some("Ada".into()));
        let known = dir.address_of_uuid(uuid(2));
        assert_eq!(known.source(), "+15550000002");
        assert_eq!(known.name.as_deref(), Some("Ada"));
        let unknown = dir.address_of_uuid(uuid(9));
        assert_eq!(unknown.source(), uuid(9).to_string());
    }

    #[test]
    fn group_ids_are_derived_not_the_master_key() {
        let mut dir = Directory::new("+15550000001", uuid(1));
        let key = [7u8; 32];
        dir.add_group(key, "Kitchen");
        let id = group_id(&key);
        assert_ne!(id, BASE64_STANDARD.encode(key));
        assert_eq!(BASE64_STANDARD.decode(&id).unwrap().len(), 32);
        assert_eq!(dir.master_key(&id), Some(key));
        assert_eq!(dir.group(&key).name, "Kitchen");
    }
}
