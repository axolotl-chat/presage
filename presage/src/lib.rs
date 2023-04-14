mod cache;
mod errors;
mod manager;
mod presage_serde;
mod store;

pub use errors::Error;
pub use manager::{Confirmation, Linking, Manager, Registered, Registration, RegistrationOptions};
pub use store::{Store, StoreError, Thread};

use serde::Deserialize;
use serde::Serialize;

#[deprecated(note = "Please help use improve the prelude module instead")]
pub use libsignal_service;

pub mod prelude {
    pub use libsignal_service::{
        configuration::SignalServers,
        content::{
            self, Content, ContentBody, DataMessage, GroupContext, GroupContextV2, GroupType,
            Metadata, SyncMessage,
        },
        models::Contact,
        prelude::{
            phonenumber::{self, PhoneNumber},
            GroupMasterKey, GroupSecretParams, Uuid,
        },
        proto,
        sender::AttachmentSpec,
        ParseServiceAddressError, ServiceAddress,
    };
}

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "-rs-", env!("CARGO_PKG_VERSION"));

// TODO: open a PR in libsignal and make sure the bytes can be read from `GroupMasterKey` instead of using this type
pub type GroupMasterKeyBytes = [u8; 32];

#[derive(Deserialize, Serialize)]
pub struct ThreadMetadata {
    pub thread: Thread,
    pub last_message: Option<ThreadMetadataMessageContent>,
    pub unread_messages_count: usize,
    pub contact: Option<prelude::Contact>,
    pub title: Option<String>,
    pub archived: bool,
    pub muted: bool,
}

#[derive(Deserialize, Serialize)]
pub struct ThreadMetadataMessageContent {
    pub sender: prelude::ServiceAddress,
    pub timestamp: u64,
    pub message: Option<String>,

}