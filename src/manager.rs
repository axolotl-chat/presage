use libsignal_service::{profile_name::ProfileName, proto::Group as ProtoGroup};
use std::{
    fmt,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use futures::{channel::mpsc, channel::oneshot, future, pin_mut, AsyncReadExt, Stream, StreamExt};
use log::{debug, error, info, trace};
use parking_lot::Mutex;
use rand::{distributions::Alphanumeric, prelude::ThreadRng, Rng, RngCore};
use serde::{Deserialize, Serialize};
use url::Url;

use libsignal_service::{
    attachment_cipher::decrypt_in_place,
    cipher,
    configuration::{ServiceConfiguration, SignalServers, SignalingKey},
    content::{ContentBody, DataMessage, Metadata, SyncMessage},
    groups_v2::{Group, GroupChanges, GroupsManager, InMemoryCredentialsCache},
    messagepipe::ServiceCredentials,
    models::Contact,
    prelude::{
        phonenumber::PhoneNumber,
        protocol::{KeyPair, PrivateKey, PublicKey},
        Content, Envelope, GroupMasterKey, GroupSecretParams, PushService, Uuid,
    },
    proto::{sync_message, AttachmentPointer, GroupContextV2},
    provisioning::{
        generate_registration_id, LinkingManager, ProvisioningManager, SecondaryDeviceProvisioning,
        VerificationCodeResponse,
    },
    push_service::{
        AccountAttributes, DeviceCapabilities, ProfileKey, ServiceError, WhoAmIResponse,
        DEFAULT_DEVICE_ID,
    },
    receiver::MessageReceiver,
    sender::{AttachmentSpec, AttachmentUploadError},
    utils::{serde_private_key, serde_public_key, serde_signaling_key},
    websocket::SignalWebSocket,
    AccountManager, Profile, ServiceAddress,
};
use libsignal_service_hyper::push_service::HyperPushService;

use crate::{cache::CacheCell, Thread};
use crate::{store::Store, Error};

type ServiceCipher<C> = cipher::ServiceCipher<C, C, C, C, C, ThreadRng>;
type MessageSender<C> =
    libsignal_service::prelude::MessageSender<HyperPushService, C, C, C, C, C, ThreadRng>;

#[derive(Clone)]
pub struct Manager<Store, State> {
    /// Implementation of a config-store to give to libsignal
    config_store: Store,
    /// Part of the manager which is persisted in the store.
    state: State,
}
#[derive(Debug)]
pub struct Session {
    pub thread: Thread,
    pub last_message: Option<Content>,
    pub unread_messages_count: usize,
    pub contact: Option<Contact>,
    pub groupv2: Option<ProtoGroup>,
    pub title: Option<String>,
}

impl<Store, State: fmt::Debug> fmt::Debug for Manager<Store, State> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Manager")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RegistrationOptions<'a> {
    pub signal_servers: SignalServers,
    pub phone_number: PhoneNumber,
    pub use_voice_call: bool,
    pub captcha: Option<&'a str>,
    pub force: bool,
}

pub struct Registration;
pub struct Linking;

pub struct Confirmation {
    signal_servers: SignalServers,
    phone_number: PhoneNumber,
    password: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Registered {
    #[serde(skip)]
    push_service_cache: CacheCell<HyperPushService>,
    #[serde(skip)]
    websocket: Arc<Mutex<Option<SignalWebSocket>>>,

    pub signal_servers: SignalServers,
    pub device_name: Option<String>,
    pub phone_number: PhoneNumber,
    pub uuid: Uuid,
    password: String,
    #[serde(with = "serde_signaling_key")]
    signaling_key: SignalingKey,
    pub device_id: Option<u32>,
    pub(crate) registration_id: u32,
    #[serde(with = "serde_private_key")]
    pub(crate) private_key: PrivateKey,
    #[serde(with = "serde_public_key")]
    pub(crate) public_key: PublicKey,
    profile_key: ProfileKey,
}

impl fmt::Debug for Registered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registered")
            .field("websocket", &self.websocket.lock().is_some())
            .finish_non_exhaustive()
    }
}

impl Registered {
    pub fn device_id(&self) -> u32 {
        self.device_id.unwrap_or(DEFAULT_DEVICE_ID)
    }

    pub fn registration_id(&self) -> u32 {
        self.registration_id
    }

    pub fn private_key(&self) -> PrivateKey {
        self.private_key
    }

    pub fn public_key(&self) -> PublicKey {
        self.public_key
    }
}

impl<C: Store> Manager<C, Registration> {
    /// Registers a new account with a phone number (and some options).
    ///
    /// The returned value is a [confirmation manager](Manager::confirm_verification_code) which you then
    /// have to use to send the confirmation code.
    ///
    /// ```no_run
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     use std::str::FromStr;
    ///
    ///     use presage::{
    ///         prelude::{phonenumber::PhoneNumber, SignalServers},
    ///         Manager, MigrationConflictStrategy, RegistrationOptions, SledStore,
    ///     };
    ///
    ///     let config_store =
    ///         SledStore::open("/tmp/presage-example", MigrationConflictStrategy::Drop)?;
    ///
    ///     let manager = Manager::register(
    ///         config_store,
    ///         RegistrationOptions {
    ///             signal_servers: SignalServers::Production,
    ///             phone_number: PhoneNumber::from_str("+16137827274")?,
    ///             use_voice_call: false,
    ///             captcha: None,
    ///             force: false,
    ///         },
    ///     )
    ///     .await?;
    ///
    ///     Ok(())
    /// }
    /// ```
    pub async fn register(
        mut config_store: C,
        registration_options: RegistrationOptions<'_>,
    ) -> Result<Manager<C, Confirmation>, Error> {
        let RegistrationOptions {
            signal_servers,
            phone_number,
            use_voice_call,
            captcha,
            force,
        } = registration_options;

        // check if we are already registered
        if !force && config_store.load_state().is_ok() {
            return Err(Error::AlreadyRegisteredError);
        }

        config_store.clear()?;

        // generate a random 24 bytes password
        let rng = rand::thread_rng();
        let password: String = rng.sample_iter(&Alphanumeric).take(24).collect();

        let service_configuration: ServiceConfiguration = signal_servers.into();
        let mut push_service =
            HyperPushService::new(service_configuration, None, crate::USER_AGENT.to_string());

        let mut provisioning_manager: ProvisioningManager<HyperPushService> =
            ProvisioningManager::new(&mut push_service, phone_number.clone(), password.clone());

        let verification_code_response = if use_voice_call {
            provisioning_manager
                .request_voice_verification_code(captcha, None)
                .await?
        } else {
            provisioning_manager
                .request_sms_verification_code(captcha, None)
                .await?
        };

        if let VerificationCodeResponse::CaptchaRequired = verification_code_response {
            return Err(Error::CaptchaRequired);
        }

        let manager = Manager {
            config_store,
            state: Confirmation {
                signal_servers,
                phone_number,
                password,
            },
        };

        Ok(manager)
    }
}

impl<C: Store> Manager<C, Linking> {
    /// Links this client as a secondary device from the device used to register the account (usually a phone).
    /// The URL to present to the user will be sent in the channel given as the argument.
    ///
    /// ```no_run
    /// use futures::{channel::oneshot, future, StreamExt};
    /// use presage::{prelude::SignalServers, Manager, MigrationConflictStrategy, SledStore};
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     let config_store =
    ///         SledStore::open("/tmp/presage-example", MigrationConflictStrategy::Drop)?;
    ///
    ///     let (mut tx, mut rx) = oneshot::channel();
    ///     let (manager, err) = future::join(
    ///         Manager::link_secondary_device(
    ///             config_store,
    ///             SignalServers::Production,
    ///             "my-linked-client".into(),
    ///             tx,
    ///         ),
    ///         async move {
    ///             match rx.await {
    ///                 Ok(url) => log::info!("Show URL {} as QR code to user", url),
    ///                 Err(e) => log::info!("Error linking device: {}", e),
    ///             }
    ///         },
    ///     )
    ///     .await;
    ///
    ///     Ok(())
    /// }
    /// ```
    pub async fn link_secondary_device(
        mut config_store: C,
        signal_servers: SignalServers,
        device_name: String,
        provisioning_link_channel: oneshot::Sender<Url>,
    ) -> Result<Manager<C, Registered>, Error> {
        // clear the database: the moment we start the process, old API credentials are invalidated
        // and you won't be able to use this client anyways
        config_store.clear()?;

        // generate a random 24 bytes password
        let mut rng = rand::thread_rng();
        let password: String = rng.sample_iter(&Alphanumeric).take(24).collect();

        // generate a 52 bytes signaling key
        let mut signaling_key = [0u8; 52];
        rng.fill_bytes(&mut signaling_key);

        let service_configuration: ServiceConfiguration = signal_servers.into();
        let push_service =
            HyperPushService::new(service_configuration, None, crate::USER_AGENT.to_string());

        let mut linking_manager: LinkingManager<HyperPushService> =
            LinkingManager::new(push_service, password.clone());

        let (tx, mut rx) = mpsc::channel(1);

        let (fut1, fut2) = future::join(
            linking_manager.provision_secondary_device(&mut rand::thread_rng(), signaling_key, tx),
            async move {
                if let Some(SecondaryDeviceProvisioning::Url(url)) = rx.next().await {
                    log::info!("generating qrcode from provisioning link: {}", &url);
                    if provisioning_link_channel.send(url).is_err() {
                        return Err(Error::LinkError);
                    }
                } else {
                    return Err(Error::LinkError);
                }

                if let Some(SecondaryDeviceProvisioning::NewDeviceRegistration {
                    phone_number,
                    device_id,
                    registration_id,
                    uuid,
                    private_key,
                    public_key,
                    profile_key,
                }) = rx.next().await
                {
                    log::info!("successfully registered device {}", &uuid);
                    Ok((
                        phone_number,
                        device_id.device_id,
                        registration_id,
                        uuid,
                        private_key,
                        public_key,
                        profile_key,
                    ))
                } else {
                    Err(Error::NoProvisioningMessageReceived)
                }
            },
        )
        .await;

        fut1?;
        let (phone_number, device_id, registration_id, uuid, private_key, public_key, profile_key) =
            fut2?;

        let mut manager = Manager {
            config_store,
            state: Registered {
                push_service_cache: CacheCell::default(),
                websocket: Default::default(),
                signal_servers,
                device_name: Some(device_name),
                phone_number,
                uuid,
                signaling_key,
                password,
                device_id: Some(device_id),
                registration_id,
                public_key,
                private_key,
                profile_key: ProfileKey(profile_key.try_into().expect("32 bytes for profile key")),
            },
        };

        manager.config_store.save_state(&manager.state)?;

        match (
            manager.register_pre_keys().await,
            manager.set_account_attributes().await,
            manager.request_contacts_sync().await,
        ) {
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                // clear the entire store on any error, there's no possible recovery here
                manager.config_store.clear()?;
                Err(e)
            }
            _ => Ok(manager),
        }
    }
}

impl<C: Store> Manager<C, Confirmation> {
    /// Confirm a newly registered account using the code you
    /// received by SMS or phone call.
    ///
    /// Returns a [registered manager](Manager::load_registered) that you can use
    /// to send and receive messages.
    pub async fn confirm_verification_code(
        self,
        confirm_code: u32,
    ) -> Result<Manager<C, Registered>, Error> {
        trace!("confirming verification code");

        // see libsignal-protocol-c / signal_protocol_key_helper_generate_registration_id
        let registration_id = generate_registration_id(&mut rand::thread_rng());
        trace!("registration_id: {}", registration_id);

        let credentials = ServiceCredentials {
            uuid: None,
            phonenumber: self.state.phone_number.clone(),
            password: Some(self.state.password.clone()),
            signaling_key: None,
            device_id: None,
        };

        let service_configuration: ServiceConfiguration = self.state.signal_servers.into();
        let mut push_service = HyperPushService::new(
            service_configuration,
            Some(credentials),
            crate::USER_AGENT.to_string(),
        );

        let mut provisioning_manager: ProvisioningManager<HyperPushService> =
            ProvisioningManager::new(
                &mut push_service,
                self.state.phone_number.clone(),
                self.state.password.to_string(),
            );

        let mut rng = rand::thread_rng();

        // generate a 52 bytes signaling key
        let mut signaling_key = [0u8; 52];
        rng.fill_bytes(&mut signaling_key);

        let mut profile_key = [0u8; 32];
        rng.fill_bytes(&mut profile_key);
        let profile_key = ProfileKey(profile_key);

        let registered = provisioning_manager
            .confirm_verification_code(
                confirm_code,
                AccountAttributes {
                    name: "".to_string(),
                    signaling_key: Some(signaling_key.to_vec()),
                    registration_id,
                    voice: false,
                    video: false,
                    fetches_messages: true,
                    pin: None,
                    registration_lock: None,
                    unidentified_access_key: Some(profile_key.derive_access_key()),
                    unrestricted_unidentified_access: false, // TODO: make this configurable?
                    discoverable_by_phone_number: true,
                    capabilities: DeviceCapabilities {
                        gv2: true,
                        gv1_migration: true,
                        ..Default::default()
                    },
                },
            )
            .await?;

        let identity_key_pair = KeyPair::generate(&mut rand::thread_rng());

        let phone_number = self.state.phone_number.clone();
        let password = self.state.password.clone();

        trace!("confirmed! (and registered)");

        let mut manager = Manager {
            config_store: self.config_store,
            state: Registered {
                push_service_cache: CacheCell::default(),
                websocket: Default::default(),
                signal_servers: self.state.signal_servers,
                device_name: None,
                phone_number,
                uuid: registered.uuid,
                password,
                signaling_key,
                device_id: None,
                registration_id,
                private_key: identity_key_pair.private_key,
                public_key: identity_key_pair.public_key,
                profile_key,
            },
        };

        manager.config_store.save_state(&manager.state)?;

        if let Err(e) = manager.register_pre_keys().await {
            // clear the entire store on any error, there's no possible recovery here
            manager.config_store.clear()?;
            Err(e)
        } else {
            Ok(manager)
        }
    }
}

impl<C: Store> Manager<C, Registered> {
    /// Loads a previously registered account from the implemented [Store].
    ///
    /// Returns a instance of [Manager] you can use to send & receive messages.
    pub fn load_registered(config_store: C) -> Result<Self, Error> {
        let state = config_store.load_state()?;
        Ok(Self {
            config_store,
            state,
        })
    }

    async fn register_pre_keys(&mut self) -> Result<(), Error> {
        trace!("registering pre keys");
        let mut account_manager =
            AccountManager::new(self.push_service()?, Some(*self.state.profile_key));

        let (pre_keys_offset_id, next_signed_pre_key_id) = account_manager
            .update_pre_key_bundle(
                &self.config_store.clone(),
                &mut self.config_store.clone(),
                &mut self.config_store.clone(),
                &mut rand::thread_rng(),
                self.config_store.pre_keys_offset_id()?,
                self.config_store.next_signed_pre_key_id()?,
                true,
            )
            .await?;

        self.config_store
            .set_pre_keys_offset_id(pre_keys_offset_id)?;
        self.config_store
            .set_next_signed_pre_key_id(next_signed_pre_key_id)?;

        trace!("registered pre keys");
        Ok(())
    }

    async fn set_account_attributes(&mut self) -> Result<(), Error> {
        trace!("setting account attributes");
        let mut account_manager =
            AccountManager::new(self.push_service()?, Some(*self.state.profile_key));

        account_manager
            .set_account_attributes(AccountAttributes {
                name: self
                    .state
                    .device_name
                    .clone()
                    .expect("Device name to be set"),
                registration_id: self.state.registration_id,
                signaling_key: None,
                voice: false,
                video: false,
                fetches_messages: true,
                pin: None,
                registration_lock: None,
                unidentified_access_key: Some(self.state.profile_key.derive_access_key()),
                unrestricted_unidentified_access: false,
                discoverable_by_phone_number: true,
                capabilities: DeviceCapabilities {
                    gv2: true,
                    gv1_migration: true,
                    ..Default::default()
                },
            })
            .await?;

        trace!("done setting account attributes");
        Ok(())
    }

    async fn wait_for_contacts_sync(
        &mut self,
        mut messages: impl Stream<Item = Content> + Unpin,
    ) -> Result<(), Error> {
        let mut message_receiver = MessageReceiver::new(self.push_service()?);
        while let Some(Content { body, .. }) = messages.next().await {
            if let ContentBody::SynchronizeMessage(SyncMessage {
                contacts: Some(contacts),
                ..
            }) = body
            {
                let contacts = message_receiver.retrieve_contacts(&contacts).await?;
                let _ = self.config_store.clear_contacts();
                self.config_store
                    .save_contacts(contacts.filter_map(Result::ok))?;
                info!("saved contacts");
                return Ok(());
            }
        }
        Ok(())
    }

    /// Request that the primary device to encrypt & send all of its contacts as a message to ourselves
    /// which can be then received, decrypted and stored in the message receiving loop.
    ///
    /// **Note**: If successful, the contacts are not yet received and stored, but will only be
    /// processed when they're received using the `MessageReceiver`.
    pub async fn request_contacts_sync(&mut self) -> Result<(), Error> {
        trace!("requesting contacts sync");
        let sync_message = SyncMessage {
            request: Some(sync_message::Request {
                r#type: Some(sync_message::request::Type::Contacts as i32),
            }),
            ..Default::default()
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        let messages = self.receive_messages().await?;
        pin_mut!(messages);

        // first request the sync
        self.send_message(self.state.uuid, sync_message, timestamp)
            .await?;

        // wait for it to arrive
        info!("waiting for contacts sync for up to 3 minutes");
        tokio::time::timeout(
            Duration::from_secs(3 * 60),
            self.wait_for_contacts_sync(messages),
        )
        .await
        .map_err(Error::from)??;

        Ok(())
    }

    /// Returns a handle on the registered state
    pub fn state(&self) -> &Registered {
        &self.state
    }

    /// Get the profile UUID
    pub fn uuid(&self) -> Uuid {
        self.state.uuid
    }

    /// Fetches basic information on the registered device.
    pub async fn whoami(&self) -> Result<WhoAmIResponse, Error> {
        Ok(self.push_service()?.whoami().await?)
    }

    /// Fetches the profile (name, about, status emoji) of the registered user.
    pub async fn retrieve_profile(&self) -> Result<Profile, Error> {
        self.retrieve_profile_by_uuid(self.state.uuid, *self.state.profile_key)
            .await
    }

    /// Fetches the profile of the provided user by UUID and profile key.
    pub async fn retrieve_profile_by_uuid(
        &self,
        uuid: Uuid,
        profile_key: [u8; 32],
    ) -> Result<Profile, Error> {
        let mut account_manager = AccountManager::new(self.push_service()?, Some(profile_key));
        Ok(account_manager.retrieve_profile(uuid.into()).await?)
    }

    /// Returns an iterator on contacts stored in the [Store].
    ///
    /// **Note:** after [requesting contacts sync](Manager::request_contacts_sync), you need
    /// to start the [receiving message loop](Manager::receive_messages) for contacts to be processed
    pub fn get_contacts(&self) -> Result<impl Iterator<Item = Result<Contact, Error>>, Error> {
        self.config_store.contacts()
    }

    /// Returns an iterator on groups stored in the [Store].
    pub fn get_groups(&self) -> Result<impl Iterator<Item = Result<ProtoGroup, Error>>, Error> {
        self.config_store.groups()
    }

    pub fn get_contact_by_id(&self, id: Uuid) -> Result<Option<Contact>, Error> {
        self.config_store.contact_by_id(id)
    }

    pub fn save_contact(&mut self, contact: Contact) -> Result<(), Error> {
        self.config_store.save_contact(contact)
    }
    pub async fn request_contacts_update_from_profile(&mut self) -> Result<(), Error> {
        log::debug!("requesting contacts update from profile");
        for contact in self.get_contacts()? {
            let mut contact = contact?;
            if contact.name.is_empty() {
                let k = contact.profile_key.to_vec();
                let profile_key: [u8; 32] = match k.try_into() {
                    Ok(key) => key,
                    Err(_) => continue,
                };
                let profile = match self
                    .retrieve_profile_by_uuid(
                        contact.address.uuid.unwrap_or(Uuid::nil()),
                        profile_key,
                    )
                    .await
                {
                    Ok(profile) => profile,
                    Err(_) => continue,
                };
                let name = profile.name.unwrap_or(ProfileName {
                    given_name: match contact.address.phonenumber {
                        Some(_) => "".to_string(),
                        None => continue,
                    },
                    family_name: None,
                });
                contact.name = name.to_string();
                match self.save_contact(contact) {
                    Ok(_) => {}
                    Err(e) => {
                        println!("Error saving contact: {:?}", e);
                    }
                };
                println!("Updating contact: {:?}", name);
            }
        }
        Ok(())
    }

    // save thread to store if it doesn't exist
    pub fn save_thread(
        &mut self,
        thread: Thread,
        profile_key: Option<Vec<u8>>,
    ) -> Result<(), Error> {
        match thread {
            Thread::Group(_) => {
                //todo!()
                Ok(())
            }
            Thread::Contact(contact) => {
                let contact_from_store = self.config_store.contact_by_id(contact);
                if contact_from_store?.is_none() {
                    let new_contact = Contact {
                        address: ServiceAddress {
                            uuid: Some(contact),
                            phonenumber: None,
                            relay: None,
                        },
                        name: "".to_string(),
                        color: None,
                        verified: libsignal_service::proto::Verified {
                            destination_e164: None,
                            destination_uuid: None,
                            identity_key: None,
                            state: None,
                            null_message: None,
                        },
                        profile_key: profile_key.unwrap_or(Vec::new()),
                        blocked: false,
                        expire_timer: 0,
                        inbox_position: 0,
                        archived: false,
                        avatar: None,
                    };
                    self.config_store.save_contact(new_contact)?;
                }
                Ok(())
            }
        }
    }

    async fn receive_messages_encrypted(
        &mut self,
    ) -> Result<impl Stream<Item = Result<Envelope, ServiceError>>, Error> {
        let credentials = self.credentials()?.ok_or(Error::NotYetRegisteredError)?;
        let pipe = MessageReceiver::new(self.push_service()?)
            .create_message_pipe(credentials)
            .await?;
        self.state.websocket.lock().replace(pipe.ws());
        Ok(pipe.stream())
    }

    /// Starts receiving and storing messages.
    ///
    /// Returns a [Stream] of messages to consume. Messages will also be stored by the implementation of the [MessageStore].
    pub async fn receive_messages(&mut self) -> Result<impl Stream<Item = Content>, Error> {
        self.receive_messages_stream(false).await
    }

    async fn receive_messages_stream(
        &mut self,
        include_internal_events: bool,
    ) -> Result<impl Stream<Item = Content>, Error> {
        struct StreamState<S, C> {
            encrypted_messages: S,
            service_cipher: ServiceCipher<C>,
            config_store: C,
            include_internal_events: bool,
        }

        let init = StreamState {
            encrypted_messages: Box::pin(self.receive_messages_encrypted().await?),
            service_cipher: self.new_service_cipher()?,
            config_store: self.config_store.clone(),
            include_internal_events,
        };

        Ok(futures::stream::unfold(init, |mut state| async move {
            loop {
                match state.encrypted_messages.next().await {
                    Some(Ok(envelope)) => {
                        match state.service_cipher.open_envelope(envelope).await {
                            Ok(Some(content)) => {
                                // contacts synchronization sent from the primary device (happens after linking, or on demand)
                                if let ContentBody::SynchronizeMessage(SyncMessage {
                                    contacts: Some(_),
                                    ..
                                }) = &content.body
                                {
                                    if state.include_internal_events {
                                        return Some((content, state));
                                    } else {
                                        return None;
                                    }
                                }

                                if let Ok(thread) = Thread::try_from(&content) {
                                    // TODO: handle reactions here, we should update the original message?
                                    match &content.body {
                                        ContentBody::ReceiptMessage(_) => {
                                            // todo!()
                                            continue;
                                        }
                                        ContentBody::TypingMessage(_) => {
                                            // todo!()
                                            continue;
                                        }
                                        ContentBody::SynchronizeMessage(message) => {
                                            // don't save sync messages except for sent messages
                                            if message.sent.is_none() {
                                                continue;
                                            }
                                        }
                                        ContentBody::DataMessage(message) => {
                                            // ignore empty messages
                                            if message.body == Some("".to_string()) {
                                                continue;
                                            }
                                        }
                                        _ => {}
                                    }

                                    if let Err(e) =
                                        state.config_store.save_message(&thread, content.clone())
                                    {
                                        log::error!("Error saving message to store: {}", e);
                                    }
                                    // create thread if it doesn't exist
                                    match thread {
                                        Thread::Group(id) => {
                                            match state.config_store.group_by_id(id.to_vec()) {
                                                Ok(Some(_)) => {}
                                                _ => {
                                                    log::info!("Creating new group");
                                                    let new_group = ProtoGroup {
                                                        members: Vec::new(),
                                                        avatar: "".to_string(),
                                                        disappearing_messages_timer: Vec::new(),
                                                        access_control: None,
                                                        version: 0,
                                                        invite_link_password: Vec::new(),
                                                        public_key: id.to_vec(),
                                                        announcements_only: false,
                                                        members_banned: Vec::new(),
                                                        description_bytes: Vec::new(),
                                                        members_pending_admin_approval: Vec::new(),
                                                        members_pending_profile_key: Vec::new(),
                                                        title: Vec::new(),
                                                    };
                                                    match state.config_store.save_group(new_group) {
                                                        Ok(_) => {}
                                                        Err(e) => {
                                                            log::error!("Error saving group: {}", e)
                                                        }
                                                    };
                                                }
                                            }
                                        }
                                        Thread::Contact(contact) => {
                                            let contact_from_store =
                                                state.config_store.contact_by_id(contact);
                                            log::debug!(
                                                "Contact from store: {:?}",
                                                contact_from_store
                                            );
                                            let profile_key = match &content.body {
                                                ContentBody::DataMessage(message) => {
                                                    message.profile_key.clone()
                                                }
                                                _ => None,
                                            };
                                            let service_adress = content.metadata.sender.clone();

                                            match contact_from_store {
                                                Ok(Some(mut c)) => {
                                                    if c.profile_key.clone().len() == 0 {
                                                        log::debug!(
                                                            "Contact doesn't have a profile key"
                                                        );
                                                        c.profile_key =
                                                            profile_key.unwrap_or(Vec::new());
                                                        match state.config_store.save_contact(c) {
                                                            Ok(_) => log::info!("Contact saved"),
                                                            Err(e) => log::error!(
                                                                "Error saving contact: {}",
                                                                e
                                                            ),
                                                        }
                                                    }
                                                }
                                                _ => {
                                                    log::info!("Creating new contact: {}", contact);
                                                    let new_contact = Contact {
                                                        address: service_adress,
                                                        name: "".to_string(),
                                                        color: None,
                                                        verified:
                                                            libsignal_service::proto::Verified {
                                                                destination_e164: None,
                                                                destination_uuid: None,
                                                                identity_key: None,
                                                                state: None,
                                                                null_message: None,
                                                            },
                                                        profile_key: profile_key
                                                            .unwrap_or(Vec::new()),
                                                        blocked: false,
                                                        expire_timer: 0,
                                                        inbox_position: 0,
                                                        archived: false,
                                                        avatar: None,
                                                    };
                                                    match state
                                                        .config_store
                                                        .save_contact(new_contact)
                                                    {
                                                        Ok(_) => log::info!("Contact saved"),
                                                        Err(e) => log::error!(
                                                            "Error saving contact: {}",
                                                            e
                                                        ),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                return Some((content, state));
                            }
                            Ok(None) => debug!("Empty envelope..., message will be skipped!"),
                            Err(e) => {
                                error!("Error opening envelope: {:?}, message will be skipped!", e);
                            }
                        }
                    }
                    Some(Err(e)) => error!("Error: {}", e),
                    None => return None,
                }
            }
        }))
    }
    /// Returns the last x messages for the given thread.
    pub fn get_messages(
        &mut self,
        thread: &Thread,
        count: Option<u64>,
    ) -> Result<Vec<Content>, Error> {
        let iter = self.config_store.messages(thread, count);
        let mut messages: Vec<Content> = Vec::new();
        for msg in iter? {
            match msg {
                Ok(msg) => {
                    messages.push(msg);
                }
                Err(e) => {
                    log::info!("Error: {}", e);
                }
            }
        }
        self.config_store.mark_all_as_read(thread)?;
        Ok(messages)
    }

    /// Sends a messages to the provided [ServiceAddress].
    /// The timestamp should be set to now and is used by Signal mobile apps
    /// to order messages later, and apply reactions.
    pub async fn send_message(
        &mut self,
        recipient_addr: impl Into<ServiceAddress>,
        message: impl Into<ContentBody>,
        timestamp: u64,
    ) -> Result<(), Error> {
        let mut sender = self.new_message_sender()?;

        let online_only = false;
        let recipient = recipient_addr.into();
        let content_body: ContentBody = message.into();

        sender
            .send_message(
                &recipient,
                None,
                content_body.clone(),
                timestamp,
                online_only,
            )
            .await?;

        // save the message
        let thread = Thread::Contact(recipient.uuid.ok_or(Error::ContentMissingUuid)?);
        let content = Content {
            metadata: Metadata {
                sender: self.state.uuid.into(),
                sender_device: self.state.device_id(),
                timestamp,
                needs_receipt: false,
                unidentified_sender: false,
            },
            body: content_body,
        };

        self.config_store.save_message(&thread, content)?;

        Ok(())
    }

    /// Uploads attachments prior to linking them in a message.
    pub async fn upload_attachments(
        &self,
        attachments: Vec<(AttachmentSpec, Vec<u8>)>,
    ) -> Result<Vec<Result<AttachmentPointer, AttachmentUploadError>>, Error> {
        let sender = self.new_message_sender()?;
        let upload = future::join_all(attachments.into_iter().map(move |(spec, contents)| {
            let mut sender = sender.clone();
            async move { sender.upload_attachment(spec, contents).await }
        }));
        Ok(upload.await)
    }

    /// Sends one message in a group (v2).
    pub async fn send_message_to_group(
        &mut self,
        recipients: impl IntoIterator<Item = ServiceAddress>,
        message: DataMessage,
        timestamp: u64,
    ) -> Result<(), Error> {
        let mut sender = self.new_message_sender()?;

        let recipients: Vec<_> = recipients.into_iter().collect();

        let online_only = false;
        let results = sender
            .send_message_to_group(recipients, None, message.clone(), timestamp, online_only)
            .await;

        // return first error if any
        results.into_iter().find(|res| res.is_err()).transpose()?;

        let content = Content {
            metadata: Metadata {
                sender: self.state.uuid.into(),
                sender_device: self.state.device_id(),
                timestamp,
                needs_receipt: false, // TODO: this is just wrong
                unidentified_sender: false,
            },
            body: message.into(),
        };
        let thread = Thread::try_from(&content)?;
        self.config_store.save_message(&thread, content)?;

        Ok(())
    }

    /// Get all sessions with unread messages.
    pub async fn get_unread_sessions(&self) -> Result<Vec<(Thread, Vec<u64>)>, Error> {
        return self.config_store.unread_messages_per_thread();
    }

    /// Get all sessions with unread messages as counter.
    pub fn get_unread_sessions_count(&self) -> Result<Vec<(Thread, usize)>, Error> {
        let unread_sessions = self
            .config_store
            .unread_messages_count_per_thread()
            .unwrap();
        let mut unread_sessions_count = Vec::new();
        for (thread, messages_count) in unread_sessions {
            unread_sessions_count.push((thread, messages_count));
        }
        Ok(unread_sessions_count)
    }

    /// reset the unread messages counter for a given thread.
    pub fn clear_unread_messages(&mut self, thread: &Thread) -> Result<(), Error> {
        self.config_store.mark_all_as_read(thread)
    }

    pub async fn get_title_for_thread(&self, thread: &Thread) -> Result<String, Error> {
        match thread {
            Thread::Contact(uuid) => {
                let contact = match self.get_contact_by_id(*uuid) {
                    Ok(contact) => contact,
                    Err(e) => {
                        log::info!("Error getting contact by id: {}, {:?}", e, uuid);
                        None
                    }
                };
                Ok(match contact {
                    Some(contact) => contact.name,
                    None => uuid.to_string(),
                })
            }
            Thread::Group(id) => match self.config_store.group_by_id(id.to_vec())? {
                Some(group) => Ok(String::from_utf8(group.title).unwrap_or("".to_string())),
                None => Ok("".to_string()),
            },
        }
    }

    pub async fn load_conversations(&self) -> Result<Vec<Session>, Error> {
        let contacts = match self.get_contacts() {
            Ok(contacts) => contacts,
            Err(e) => {
                log::info!("Error getting contacts: {}", e);
                return Ok(Vec::new());
            }
        };
        let mut conversations = Vec::new();
        for contact in contacts {
            match contact {
                Ok(contact) => {
                    let thread = Thread::Contact(contact.address.uuid.unwrap());
                    let unread_messages_count =
                        match self.config_store.unread_messages_count(&thread) {
                            Ok(count) => count,
                            Err(e) => {
                                log::info!("Error getting unread messages count: {}", e);
                                0
                            }
                        };
                    let latest_message = match self.config_store.latest_message(&thread) {
                        Ok(message) => message,
                        Err(e) => {
                            log::info!("Error getting latest message: {}", e);
                            None
                        }
                    };

                    let title: Option<String> = match self.get_title_for_thread(&thread).await {
                        Ok(title) => Some(title),
                        Err(e) => {
                            log::info!("Error getting title: {}", e);
                            None
                        }
                    };

                    let conversation = Session {
                        thread: thread,
                        last_message: latest_message,
                        contact: Some(contact),
                        unread_messages_count: unread_messages_count,
                        groupv2: None,
                        title,
                    };

                    conversations.push(conversation);
                }
                Err(e) => {
                    log::error!("Error getting contact: {}", e);
                }
            }
        }
        for group in self.get_groups()? {
            match group {
                Ok(group) => {
                    let key = group.public_key.to_vec();
                    let group_id: [u8; 32] = match key.try_into() {
                        Ok(g) => g,
                        Err(e) => {
                            log::info!("{:?}", e);
                            [0; 32]
                        }
                    };
                    let thread = Thread::Group(group_id);
                    let unread_messages_count =
                        match self.config_store.unread_messages_count(&thread) {
                            Ok(count) => count,
                            Err(e) => {
                                log::info!("Error getting unread messages for group count: {}", e);
                                0
                            }
                        };
                    let latest_message = match self.config_store.latest_message(&thread) {
                        Ok(message) => message,
                        Err(e) => {
                            log::info!("Error getting latest message: {}", e);
                            None
                        }
                    };
                    let mut title: Option<String> = match self.get_title_for_thread(&thread).await {
                        Ok(title) => Some(title),
                        Err(e) => {
                            log::info!("Error getting title: {}", e);
                            None
                        }
                    };
                    if title.is_some() {
                        if title.as_ref().unwrap().is_empty() {
                            let master_key = GroupMasterKey::new(group_id);
                            match self.get_group_v2(master_key).await {
                                Ok(updated_group) => {
                                    match self.save_group(updated_group, group.public_key.to_vec())
                                    {
                                        Ok(_) => {
                                            title = match self.get_title_for_thread(&thread).await {
                                                Ok(title) => Some(title),
                                                Err(e) => {
                                                    log::info!("Error getting title: {}", e);
                                                    None
                                                }
                                            };
                                        }
                                        Err(e) => {
                                            log::info!("Error saving group: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::info!("Error getting group: {}", e);
                                }
                            }
                        }
                    }
                    let conversation = Session {
                        thread: thread,
                        last_message: latest_message,
                        contact: None,
                        unread_messages_count: unread_messages_count,
                        groupv2: Some(group),
                        title,
                    };
                    conversations.push(conversation);
                }
                Err(e) => {
                    log::info!("Error getting group: {}", e);
                }
            }
        }
        conversations.sort_unstable_by(|a, b| {
            let a_timestamp = a
                .last_message
                .as_ref()
                .map(|m| m.metadata.timestamp)
                .unwrap_or(0);
            let b_timestamp = b
                .last_message
                .as_ref()
                .map(|m| m.metadata.timestamp)
                .unwrap_or(0);
            b_timestamp.cmp(&a_timestamp)
        });

        Ok(conversations)
    }

    /// Get all sessions
    // pub async fn get_sessions(&self) -> Result<Vec<Session>, Error> {

    //     Err(Error::NotImplemented)
    // }

    /// Clears all sessions established with [recipient](ServiceAddress).
    pub async fn clear_sessions(&self, recipient: &ServiceAddress) -> Result<(), Error> {
        self.config_store
            .delete_all_sessions(&recipient.identifier())
            .await?;
        Ok(())
    }

    pub async fn get_group_v2(&self, group_master_key: GroupMasterKey) -> Result<Group, Error> {
        let service_configuration: ServiceConfiguration = self.state.signal_servers.into();
        let server_public_params = service_configuration.zkgroup_server_public_params;

        let mut groups_v2_credentials_cache = InMemoryCredentialsCache::default();
        let mut groups_v2_manager = GroupsManager::new(
            self.push_service()?,
            &mut groups_v2_credentials_cache,
            server_public_params,
        );

        let group_secret_params = GroupSecretParams::derive_from_master_key(group_master_key);
        let authorization = groups_v2_manager
            .get_authorization_for_today(self.state.uuid, group_secret_params)
            .await?;

        Ok(groups_v2_manager
            .get_group(group_secret_params, authorization)
            .await?)
    }

    /// Decrypts a blob of [GroupContextV2] and deserializes it in a higher level [GroupChanges] struct.
    pub fn decrypt_group_context(
        &self,
        group_context: GroupContextV2,
    ) -> Result<Option<GroupChanges>, Error> {
        let service_configuration: ServiceConfiguration = self.state.signal_servers.into();
        let server_public_params = service_configuration.zkgroup_server_public_params;
        let mut groups_v2_credentials_cache = InMemoryCredentialsCache::default();
        let groups_v2_manager = GroupsManager::new(
            self.push_service()?,
            &mut groups_v2_credentials_cache,
            server_public_params,
        );

        let group_changes = groups_v2_manager
            .decrypt_group_context(group_context)
            .map_err(ServiceError::GroupsV2DecryptionError)?;

        Ok(group_changes)
    }

    pub fn save_group(&self, group: Group, key: Vec<u8>) -> Result<(), Error> {
        let proto_group: ProtoGroup = ProtoGroup {
            title: group.title.as_bytes().to_vec(),
            members: Vec::new(),
            avatar: group.avatar,
            public_key: key,
            disappearing_messages_timer: Vec::new(),
            access_control: group.access_control,
            version: group.version,
            members_pending_admin_approval: Vec::new(),
            members_pending_profile_key: Vec::new(),
            invite_link_password: group.invite_link_password,
            description_bytes: group.description.unwrap_or_default().as_bytes().to_vec(),
            announcements_only: false,
            members_banned: Vec::new(),
        };
        self.config_store.save_group(proto_group)?;
        Ok(())
    }
    /// Downloads and decrypts a single attachment.
    pub async fn get_attachment(
        &self,
        attachment_pointer: &AttachmentPointer,
    ) -> Result<Vec<u8>, Error> {
        let mut service = self.push_service()?;
        let mut attachment_stream = service.get_attachment(attachment_pointer).await?;

        // We need the whole file for the crypto to check out
        let mut ciphertext = Vec::new();
        let len = attachment_stream.read_to_end(&mut ciphertext).await?;

        trace!("downloaded encrypted attachment of {} bytes", len);

        let key: [u8; 64] = attachment_pointer.key().try_into()?;
        decrypt_in_place(key, &mut ciphertext)?;

        Ok(ciphertext)
    }

    fn credentials(&self) -> Result<Option<ServiceCredentials>, Error> {
        Ok(Some(ServiceCredentials {
            uuid: Some(self.state.uuid),
            phonenumber: self.state.phone_number.clone(),
            password: Some(self.state.password.clone()),
            signaling_key: Some(self.state.signaling_key),
            device_id: self.state.device_id,
        }))
    }

    /// Returns a clone of a cached push service.
    ///
    /// If no service is yet cached, it will create and cache one.
    fn push_service(&self) -> Result<HyperPushService, Error> {
        self.state.push_service_cache.get(|| {
            let credentials = self.credentials()?;
            let service_configuration: ServiceConfiguration = self.state.signal_servers.into();

            Ok(HyperPushService::new(
                service_configuration,
                credentials,
                crate::USER_AGENT.to_string(),
            ))
        })
    }

    /// Creates a new message sender.
    fn new_message_sender(&self) -> Result<MessageSender<C>, Error> {
        let local_addr = ServiceAddress {
            uuid: Some(self.state.uuid),
            phonenumber: Some(self.state.phone_number.clone()),
            relay: None,
        };

        Ok(MessageSender::new(
            self.state
                .websocket
                .lock()
                .clone()
                .ok_or(Error::MessagePipeNotStarted)?,
            self.push_service()?,
            self.new_service_cipher()?,
            rand::thread_rng(),
            self.config_store.clone(),
            self.config_store.clone(),
            local_addr,
            self.state.device_id.unwrap_or(DEFAULT_DEVICE_ID).into(),
        ))
    }

    /// Creates a new service cipher.
    fn new_service_cipher(&self) -> Result<ServiceCipher<C>, Error> {
        let service_configuration: ServiceConfiguration = self.state.signal_servers.into();
        let service_cipher = ServiceCipher::new(
            self.config_store.clone(),
            self.config_store.clone(),
            self.config_store.clone(),
            self.config_store.clone(),
            self.config_store.clone(),
            rand::thread_rng(),
            service_configuration.unidentified_sender_trust_root,
            self.state.uuid,
            self.state.device_id.unwrap_or(DEFAULT_DEVICE_ID),
        );

        Ok(service_cipher)
    }
}
