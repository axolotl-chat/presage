use crate::{manager::Registered, Error};
use libsignal_service::{
    content::ContentBody,
    models::Contact,
    proto::Group,
    prelude::{
        protocol::{
            IdentityKeyStore, PreKeyStore, SenderKeyStore, SessionStoreExt, SignedPreKeyStore,
        },
        Content, Uuid,
    },
    proto::{sync_message::Sent, DataMessage, GroupContextV2, SyncMessage},
};

#[cfg(feature = "sled-store")]
pub mod sled;

#[cfg(feature = "volatile-store")]
pub mod volatile;

#[cfg(feature = "secret-volatile-store")]
pub mod secret_volatile;

pub trait Store:
    PreKeyStore
    + SignedPreKeyStore
    + SessionStoreExt
    + IdentityKeyStore
    + StateStore<Registered>
    + ContactsStore
    + MessageStore
    + GroupsStore
    + UnreadMessagesStore
    + SenderKeyStore
    + Sync
    + Clone
{
    /// Clear the entire store, this can be useful when re-initializing an existing client
    /// Note: you can implement this the way you want and only clear the database partially
    /// but should always make sure the state and all keys are gone.
    fn clear(&mut self) -> Result<(), Error>;

    fn pre_keys_offset_id(&self) -> Result<u32, Error>;
    fn set_pre_keys_offset_id(&mut self, id: u32) -> Result<(), Error>;

    fn next_signed_pre_key_id(&self) -> Result<u32, Error>;
    fn set_next_signed_pre_key_id(&mut self, id: u32) -> Result<(), Error>;
}

pub trait StateStore<S> {
    fn load_state(&self) -> Result<Registered, Error>;
    fn save_state(&mut self, state: &S) -> Result<(), Error>;
}

pub trait ContactsStore {
    type ContactsIter: Iterator<Item = Result<Contact, Error>>;

    fn clear_contacts(&mut self) -> Result<(), Error>;
    fn save_contacts(&mut self, contacts: impl Iterator<Item = Contact>) -> Result<(), Error>;
    fn save_contact(&mut self, contact: Contact) -> Result<(), Error>;
    fn contacts(&self) -> Result<Self::ContactsIter, Error>;
    fn contact_by_id(&self, id: Uuid) -> Result<Option<Contact>, Error>;
}

pub trait GroupsStore {
    type GroupsIter: Iterator<Item = Result<Group, Error>>;

    fn clear_groups(&mut self) -> Result<(), Error>;
    fn save_groups(&mut self, groups: impl Iterator<Item = Group>) -> Result<(), Error>;
    fn save_group(&self, group: Group) -> Result<(), Error>;
    fn groups(&self) -> Result<Self::GroupsIter, Error>;
    fn group_by_id(&self, id: Uuid) -> Result<Option<Group>, Error>;
}
/// A thread specifies where a message was sent, either to or from a contact or in a group.
#[derive(Debug, Hash, Eq, PartialEq, Clone)]
pub enum Thread {
    /// The message was sent inside a contact-chat.
    Contact(Uuid),
    // Cannot use GroupMasterKey as unable to extract the bytes.
    /// The message was sent inside a groups-chat with the [GroupMasterKey](crate::prelude::GroupMasterKey) (specified as bytes).
    Group([u8; 32]),
}

impl TryFrom<&Content> for Thread {
    type Error = Error;

    fn try_from(content: &Content) -> Result<Self, Error> {
        match &content.body {
            // Case 1: SyncMessage sent from other device notifying about a message sent to someone else.
            // => The recipient of the message mentioned in the SyncMessage is the thread.
            ContentBody::SynchronizeMessage(SyncMessage {
                sent:
                    Some(Sent {
                        destination_uuid: Some(uuid),
                        ..
                    }),
                ..
            }) => Ok(Self::Contact(Uuid::parse_str(uuid)?)),
            // Case 2: Received a group message
            // => The group is the thread.
            ContentBody::DataMessage(DataMessage {
                group_v2:
                    Some(GroupContextV2 {
                        master_key: Some(key),
                        ..
                    }),
                ..
            })
            | ContentBody::SynchronizeMessage(SyncMessage {
                sent:
                    Some(Sent {
                        message:
                            Some(DataMessage {
                                group_v2:
                                    Some(GroupContextV2 {
                                        master_key: Some(key),
                                        ..
                                    }),
                                ..
                            }),
                        ..
                    }),
                ..
            }) => Ok(Self::Group(
                key.clone()
                    .try_into()
                    .expect("Group master key to have 32 bytes"),
            )),
            // Case 3: Received a 1-1 message
            // => The message sender is the thread.
            _ => Ok(Thread::Contact(
                content
                    .metadata
                    .sender
                    .uuid
                    .ok_or(Error::ContentMissingUuid)?,
            )),
        }
    }
}

/// A [MessageStore] can store messages in the form [Content] and retrieve messages either by
/// [MessageIdentity], by [Thread] or completely.
pub trait MessageStore {
    type MessagesIter: Iterator<Item = Result<Content, Error>>;

    /// Save a message in a [Thread] identified by a timestamp.
    /// Returns the timestamp of the message as Vec<u8>.
    /// TODO: deriving the thread happens from the content, so we can also ditch the first parameter
    fn save_message(&mut self, thread: &Thread, message: Content) -> Result<u64, Error>;

    /// Delete a single message, identified by its received timestamp from a thread.
    fn delete_message(&mut self, thread: &Thread, timestamp: u64) -> Result<bool, Error>;

    /// Retrieve a message from a [Thread] by its timestamp.
    fn message(&self, thread: &Thread, timestamp: u64) -> Result<Option<Content>, Error>;

    /// Retruieve latest message from a [Thread].
    fn latest_message(&self, thread: &Thread) -> Result<Option<Content>, Error>;

    /// Retrieve a message from a [Thread].
    fn messages(&self, thread: &Thread, from: Option<u64>) -> Result<Self::MessagesIter, Error>;
}

pub trait UnreadMessagesStore {
    /// Mark a message as read.
    fn mark_as_read(&mut self, thread: &Thread, timestamp: u64) -> Result<(), Error>;

    /// Mark all messages in a thread as read.
    fn mark_all_as_read(&mut self, thread: &Thread) -> Result<(), Error>;

    /// Get the unread messages in a thread.
    fn unread_messages(&self, thread: &Thread) -> Result<Vec<u64>, Error>;

    /// Get the unrad message count in a thread.
    fn unread_messages_count(&self, thread: &Thread) -> Result<usize, Error>;

    /// Get the unread messages for each thread
    fn unread_messages_per_thread(&self) -> Result<Vec<(Thread, Vec<u64>)>, Error>;

    /// Add a message to the unread messages.
    /// This is used when a message is received and the sender is not me
    fn add_unread_message(&mut self, thread: &Thread, timestamp: u64) -> Result<(), Error>;
}
