mod cache;
mod errors;
mod serializers;
use serde::{Deserialize, Serialize};
pub mod manager;
pub mod store;

pub use libsignal_service;
/// Protobufs used in Signal protocol and service communication
pub use libsignal_service::proto;

pub use errors::Error;
pub use manager::Manager;

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "-rs-", env!("CARGO_PKG_VERSION"));

pub type AvatarBytes = Vec<u8>;
// TODO: open a PR in libsignal and make sure the bytes can be read from `GroupMasterKey` instead of using this type
pub type GroupMasterKeyBytes = [u8; 32];

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ThreadMetadata {
    pub thread: store::Thread,
    pub last_message: Option<ThreadMetadataMessageContent>,
    pub unread_messages_count: usize,
    pub title: Option<String>,
    pub archived: bool,
    pub muted: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ThreadMetadataMessageContent {
    pub sender: libsignal_service::prelude::Uuid,
    pub timestamp: u64,
    pub message: Option<String>,
}
