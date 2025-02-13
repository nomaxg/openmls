//! ### Don't Panic!
//!
//! Functions in this module should never panic. However, if there is a bug in
//! the implementation, a function will return an unrecoverable `LibraryError`.
//! This means that some functions that are not expected to fail and throw an
//! error, will still return a `Result` since they may throw a `LibraryError`.

// Private
mod apply_proposals;
mod new_from_welcome;
mod validation;

// Crate
pub(crate) mod create_commit;
pub(crate) mod create_commit_params;
pub(crate) mod new_from_external_init;
pub(crate) mod past_secrets;
pub(crate) mod process;
pub(crate) mod proposals;
pub(crate) mod staged_commit;

// Tests
#[cfg(test)]
mod test_core_group;
#[cfg(test)]
mod test_create_commit_params;
#[cfg(test)]
mod test_duplicate_extension;
#[cfg(test)]
mod test_external_init;
#[cfg(test)]
mod test_past_secrets;
#[cfg(test)]
mod test_proposals;

#[cfg(test)]
use super::errors::CreateGroupContextExtProposalError;

use crate::{
    ciphersuite::{signable::Signable, HpkePublicKey},
    credentials::*,
    error::LibraryError,
    extensions::errors::*,
    framing::*,
    group::*,
    key_packages::*,
    messages::VerifiableGroupInfo,
    messages::{proposals::*, *},
    schedule::{message_secrets::*, psk::*, *},
    tree::{secret_tree::SecretTreeError, sender_ratchet::SenderRatchetConfiguration},
    treesync::{node::leaf_node::Capabilities, *},
    versions::ProtocolVersion,
};

use self::{past_secrets::MessageSecretsStore, staged_commit::StagedCommit};
use log::{debug, trace};
use openmls_traits::{crypto::OpenMlsCrypto, types::Ciphersuite};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::convert::TryFrom;
#[cfg(test)]
use std::io::{Error, Read, Write};
use tls_codec::Serialize as TlsSerializeTrait;

use super::{
    errors::{
        CoreGroupBuildError, CreateAddProposalError, ExporterError, ProposalValidationError,
        ValidationError,
    },
    group_context::*,
};

#[derive(Debug)]
pub(crate) struct CreateCommitResult {
    pub(crate) commit: MlsAuthContent,
    pub(crate) welcome_option: Option<Welcome>,
    pub(crate) staged_commit: StagedCommit,
}

/// A member in the group is identified by this [`Member`] struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// The member's leaf index in the ratchet tree.
    pub index: u32,
    /// The member's identity from the credential.
    pub identity: Vec<u8>,
    /// The member's public HPHKE encryption key.
    pub encryption_key: Vec<u8>,
    /// The member's public signature key.
    pub signature_key: Vec<u8>,
}

impl Member {
    /// Create new member.
    pub fn new(
        index: u32,
        encryption_key: Vec<u8>,
        signature_key: Vec<u8>,
        identity: Vec<u8>,
    ) -> Self {
        Self {
            index,
            encryption_key,
            signature_key,
            identity,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct CoreGroup {
    ciphersuite: Ciphersuite,
    group_context: GroupContext,
    group_epoch_secrets: GroupEpochSecrets,
    tree: TreeSync,
    interim_transcript_hash: Vec<u8>,
    // Group config.
    // Set to true if the ratchet tree extension is added to the `GroupInfo`.
    // Defaults to `false`.
    use_ratchet_tree_extension: bool,
    // The MLS protocol version used in this group.
    mls_version: ProtocolVersion,
    /// A [`MessageSecretsStore`] that stores message secrets.
    /// By default this store has the length of 1, i.e. only the [`MessageSecrets`]
    /// of the current epoch is kept.
    /// If more secrets from past epochs should be kept in order to be
    /// able to decrypt application messages from previous epochs, the size of
    /// the store must be increased through [`max_past_epochs()`].
    message_secrets_store: MessageSecretsStore,
}

/// Builder for [`CoreGroup`].
pub(crate) struct CoreGroupBuilder {
    key_package_bundle: KeyPackageBundle,
    own_leaf_extensions: Vec<Extension>,
    group_id: GroupId,
    config: Option<CoreGroupConfig>,
    psk_ids: Vec<PreSharedKeyId>,
    version: Option<ProtocolVersion>,
    required_capabilities: Option<RequiredCapabilitiesExtension>,
    max_past_epochs: usize,
    lifetime: Option<LifetimeExtension>,
}

impl CoreGroupBuilder {
    /// Create a new [`CoreGroupBuilder`].
    pub(crate) fn new(group_id: GroupId, key_package_bundle: KeyPackageBundle) -> Self {
        Self {
            key_package_bundle,
            group_id,
            config: None,
            psk_ids: vec![],
            version: None,
            required_capabilities: None,
            max_past_epochs: 0,
            own_leaf_extensions: vec![],
            lifetime: None,
        }
    }
    /// Set the [`CoreGroupConfig`] of the [`CoreGroup`].
    pub(crate) fn with_config(mut self, config: CoreGroupConfig) -> Self {
        self.config = Some(config);
        self
    }
    /// Set the [`Vec<PreSharedKeyId>`] of the [`CoreGroup`].
    #[cfg(test)]
    pub(crate) fn with_psk(mut self, psk_ids: Vec<PreSharedKeyId>) -> Self {
        self.psk_ids = psk_ids;
        self
    }
    /// Set the [`RequiredCapabilitiesExtension`] of the [`CoreGroup`].
    pub(crate) fn with_required_capabilities(
        mut self,
        required_capabilities: RequiredCapabilitiesExtension,
    ) -> Self {
        self.required_capabilities = Some(required_capabilities);
        self
    }
    /// Set the number of past epochs the group should keep secrets.
    pub fn with_max_past_epoch_secrets(mut self, max_past_epochs: usize) -> Self {
        self.max_past_epochs = max_past_epochs;
        self
    }
    /// Set the [`LifetimeExtension`] for the own leaf in the group.
    pub fn with_lifetime(mut self, lifetime: LifetimeExtension) -> Self {
        self.lifetime = Some(lifetime);
        self
    }
    /// Set extensions for the own leaf in the group.
    #[cfg(test)]
    pub fn with_extensions(mut self, extensions: Vec<Extension>) -> Self {
        self.own_leaf_extensions = extensions;
        self
    }

    /// Build the [`CoreGroup`].
    /// Any values that haven't been set in the builder are set to their default
    /// values (which might be random).
    ///
    /// This function performs cryptographic operations and there requires an
    /// [`OpenMlsCryptoProvider`].
    pub(crate) fn build(
        self,
        credential_bundle: &CredentialBundle,
        backend: &impl OpenMlsCryptoProvider,
    ) -> Result<CoreGroup, CoreGroupBuildError> {
        let ciphersuite = self.key_package_bundle.key_package().ciphersuite();
        let config = self.config.unwrap_or_default();
        let capabilities = self
            .required_capabilities
            .as_ref()
            .map(|re| re.extensions());
        let version = self.version.unwrap_or_default();

        debug!("Created group {:x?}", self.group_id);
        trace!(" >>> with {:?}, {:?}", ciphersuite, config);
        let (tree, commit_secret) = TreeSync::new(
            backend,
            self.key_package_bundle,
            credential_bundle,
            self.lifetime.unwrap_or_default(),
            Capabilities::new(
                Some(&[version]),     // TODO: Allow more versions
                Some(&[ciphersuite]), // TODO: allow more ciphersuites
                capabilities,
                None,
                None,
            ),
            self.own_leaf_extensions,
        )?;

        let required_capabilities = self.required_capabilities.unwrap_or_default();
        required_capabilities.check_support().map_err(|e| match e {
            ExtensionError::UnsupportedProposalType => CoreGroupBuildError::UnsupportedProposalType,
            ExtensionError::UnsupportedExtensionType => {
                CoreGroupBuildError::UnsupportedExtensionType
            }
            _ => LibraryError::custom("Unexpected ExtensionError").into(),
        })?;
        let required_capabilities = &[Extension::RequiredCapabilities(required_capabilities)];

        let group_context = GroupContext::create_initial_group_context(
            ciphersuite,
            self.group_id,
            tree.tree_hash().to_vec(),
            required_capabilities,
        );
        // Derive an initial joiner secret based on the commit secret.
        // Derive an epoch secret from the joiner secret.
        // We use a random `InitSecret` for initialization.
        let joiner_secret = JoinerSecret::new(
            backend,
            commit_secret,
            &InitSecret::random(ciphersuite, backend, version)
                .map_err(LibraryError::unexpected_crypto_error)?,
        )
        .map_err(LibraryError::unexpected_crypto_error)?;

        let serialized_group_context = group_context
            .tls_serialize_detached()
            .map_err(LibraryError::missing_bound_check)?;

        // Prepare the PskSecret
        let psk_secret = PskSecret::new(ciphersuite, backend, &self.psk_ids)?;

        let mut key_schedule = KeySchedule::init(ciphersuite, backend, joiner_secret, psk_secret)?;
        key_schedule
            .add_context(backend, &serialized_group_context)
            .map_err(|_| LibraryError::custom("Using the key schedule in the wrong state"))?;

        let epoch_secrets = key_schedule
            .epoch_secrets(backend)
            .map_err(|_| LibraryError::custom("Using the key schedule in the wrong state"))?;

        let (group_epoch_secrets, message_secrets) =
            epoch_secrets.split_secrets(serialized_group_context, 1u32, 0u32);
        let message_secrets_store =
            MessageSecretsStore::new_with_secret(self.max_past_epochs, message_secrets);

        let interim_transcript_hash = vec![];

        Ok(CoreGroup {
            ciphersuite,
            group_context,
            group_epoch_secrets,
            tree,
            interim_transcript_hash,
            use_ratchet_tree_extension: config.add_ratchet_tree_extension,
            mls_version: version,
            message_secrets_store,
        })
    }
}

/// Public [`CoreGroup`] functions.
impl CoreGroup {
    /// Get a builder for [`CoreGroup`].
    pub(crate) fn builder(
        group_id: GroupId,
        key_package_bundle: KeyPackageBundle,
    ) -> CoreGroupBuilder {
        CoreGroupBuilder::new(group_id, key_package_bundle)
    }

    // === Create handshake messages ===
    // TODO: share functionality between these.

    // 11.1.1. Add
    // struct {
    //     KeyPackage key_package;
    // } Add;
    pub(crate) fn create_add_proposal(
        &self,
        framing_parameters: FramingParameters,
        credential_bundle: &CredentialBundle,
        joiner_key_package: KeyPackage,
        backend: &impl OpenMlsCryptoProvider,
    ) -> Result<MlsAuthContent, CreateAddProposalError> {
        joiner_key_package
            .leaf_node()
            .validate_required_capabilities(self.required_capabilities())
            .map_err(|_| CreateAddProposalError::UnsupportedExtensions)?;
        let add_proposal = AddProposal {
            key_package: joiner_key_package,
        };
        let proposal = Proposal::Add(add_proposal);
        MlsAuthContent::member_proposal(
            framing_parameters,
            self.own_leaf_index(),
            proposal,
            credential_bundle,
            self.context(),
            backend,
        )
        .map_err(|e| e.into())
    }

    // 11.1.2. Update
    // struct {
    //     KeyPackage key_package;
    // } Update;
    pub(crate) fn create_update_proposal(
        &self,
        framing_parameters: FramingParameters,
        credential_bundle: &CredentialBundle,
        // XXX: There's no need to own this. The [`UpdateProposal`] should
        //      operate on a reference to make this more efficient.
        leaf_node: LeafNode,
        backend: &impl OpenMlsCryptoProvider,
    ) -> Result<MlsAuthContent, LibraryError> {
        let update_proposal = UpdateProposal { leaf_node };
        let proposal = Proposal::Update(update_proposal);
        MlsAuthContent::member_proposal(
            framing_parameters,
            self.own_leaf_index(),
            proposal,
            credential_bundle,
            self.context(),
            backend,
        )
    }

    // 11.1.3. Remove
    // struct {
    //     KeyPackageRef removed;
    // } Remove;
    pub(crate) fn create_remove_proposal(
        &self,
        framing_parameters: FramingParameters,
        credential_bundle: &CredentialBundle,
        removed: u32,
        backend: &impl OpenMlsCryptoProvider,
    ) -> Result<MlsAuthContent, ValidationError> {
        if self.treesync().leaf_is_in_tree(removed).is_err() {
            return Err(ValidationError::UnknownMember);
        }
        let remove_proposal = RemoveProposal { removed };
        let proposal = Proposal::Remove(remove_proposal);
        MlsAuthContent::member_proposal(
            framing_parameters,
            self.own_leaf_index(),
            proposal,
            credential_bundle,
            self.context(),
            backend,
        )
        .map_err(ValidationError::LibraryError)
    }

    // 11.1.4. PreSharedKey
    // struct {
    //     PreSharedKeyID psk;
    // } PreSharedKey;
    // TODO: #751
    #[cfg(test)]
    pub(crate) fn create_presharedkey_proposal(
        &self,
        framing_parameters: FramingParameters,
        credential_bundle: &CredentialBundle,
        psk: PreSharedKeyId,
        backend: &impl OpenMlsCryptoProvider,
    ) -> Result<MlsAuthContent, LibraryError> {
        let presharedkey_proposal = PreSharedKeyProposal::new(psk);
        let proposal = Proposal::PreSharedKey(presharedkey_proposal);
        MlsAuthContent::member_proposal(
            framing_parameters,
            self.own_leaf_index(),
            proposal,
            credential_bundle,
            self.context(),
            backend,
        )
    }

    /// Create a `GroupContextExtensions` proposal.
    #[cfg(test)]
    pub(crate) fn create_group_context_ext_proposal(
        &self,
        framing_parameters: FramingParameters,
        credential_bundle: &CredentialBundle,
        extensions: &[Extension],
        backend: &impl OpenMlsCryptoProvider,
    ) -> Result<MlsAuthContent, CreateGroupContextExtProposalError> {
        // Ensure that the group supports all the extensions that are wanted.

        let required_extension = extensions
            .iter()
            .find(|extension| extension.extension_type() == ExtensionType::RequiredCapabilities);
        if let Some(required_extension) = required_extension {
            let required_capabilities = required_extension.as_required_capabilities_extension()?;
            // Ensure we support all the capabilities.
            required_capabilities.check_support()?;
            self.treesync()
                .own_leaf_node()
                .map_err(|_| LibraryError::custom("Expected own leaf"))?
                .validate_required_capabilities(required_capabilities)?;
            // Ensure that all other leaf nodes support all the required
            // extensions as well.
            self.treesync()
                .check_extension_support(required_capabilities.extensions())?;
        }
        let proposal = GroupContextExtensionProposal::new(extensions);
        let proposal = Proposal::GroupContextExtensions(proposal);
        MlsAuthContent::member_proposal(
            framing_parameters,
            self.own_leaf_index(),
            proposal,
            credential_bundle,
            self.context(),
            backend,
        )
        .map_err(|e| e.into())
    }

    // Create application message
    pub(crate) fn create_application_message(
        &mut self,
        aad: &[u8],
        msg: &[u8],
        credential_bundle: &CredentialBundle,
        padding_size: usize,
        backend: &impl OpenMlsCryptoProvider,
    ) -> Result<MlsCiphertext, MessageEncryptionError> {
        let mls_plaintext = MlsAuthContent::new_application(
            self.own_leaf_index(),
            aad,
            msg,
            credential_bundle,
            self.context(),
            backend,
        )?;
        self.encrypt(mls_plaintext, padding_size, backend)
    }

    // Encrypt an MlsPlaintext into an MlsCiphertext
    pub(crate) fn encrypt(
        &mut self,
        mls_plaintext: MlsAuthContent,
        padding_size: usize,
        backend: &impl OpenMlsCryptoProvider,
    ) -> Result<MlsCiphertext, MessageEncryptionError> {
        log::trace!("{:?}", mls_plaintext.confirmation_tag());
        MlsCiphertext::try_from_plaintext(
            &mls_plaintext,
            self.ciphersuite,
            backend,
            self.message_secrets_store.message_secrets_mut(),
            padding_size,
        )
    }

    /// Decrypt an MlsCiphertext into an MlsPlaintext
    #[cfg(any(feature = "test-utils", test))]
    pub(crate) fn decrypt(
        &mut self,
        mls_ciphertext: &MlsCiphertext,
        backend: &impl OpenMlsCryptoProvider,
        sender_ratchet_configuration: &SenderRatchetConfiguration,
    ) -> Result<VerifiableMlsAuthContent, MessageDecryptionError> {
        use crate::tree::index::SecretTreeLeafIndex;

        let ciphersuite = self.ciphersuite();
        let message_secrets = self
            .message_secrets_mut(mls_ciphertext.epoch())
            .map_err(|_| MessageDecryptionError::AeadError)?;
        let sender_data = mls_ciphertext.sender_data(message_secrets, backend, ciphersuite)?;
        if self
            .treesync()
            .leaf_is_in_tree(sender_data.leaf_index)
            .is_err()
        {
            return Err(MessageDecryptionError::SenderError(
                SenderError::UnknownSender,
            ));
        }
        let sender_index = SecretTreeLeafIndex(sender_data.leaf_index);
        let message_secrets = self
            .message_secrets_mut(mls_ciphertext.epoch())
            .map_err(|_| MessageDecryptionError::AeadError)?;
        mls_ciphertext.to_plaintext(
            ciphersuite,
            backend,
            message_secrets,
            sender_index,
            sender_ratchet_configuration,
            sender_data,
        )
    }

    /// Exporter
    pub(crate) fn export_secret(
        &self,
        backend: &impl OpenMlsCryptoProvider,
        label: &str,
        context: &[u8],
        key_length: usize,
    ) -> Result<Vec<u8>, ExporterError> {
        if key_length > u16::MAX.into() {
            log::error!("Got a key that is larger than u16::MAX");
            return Err(ExporterError::KeyLengthTooLong);
        }
        Ok(self
            .group_epoch_secrets
            .exporter_secret()
            .derive_exported_secret(self.ciphersuite(), backend, label, context, key_length)
            .map_err(LibraryError::unexpected_crypto_error)?)
    }

    pub(crate) fn export_group_info(
        &self,
        backend: &impl OpenMlsCryptoProvider,
        credential_bundle: &CredentialBundle,
        with_ratchet_tree: bool,
    ) -> Result<GroupInfo, LibraryError> {
        let extensions = {
            let ratchet_tree_extension = || {
                Extension::RatchetTree(RatchetTreeExtension::new(self.treesync().export_nodes()))
            };

            let external_pub_extension = || {
                let external_pub = self
                    .group_epoch_secrets()
                    .external_secret()
                    .derive_external_keypair(backend.crypto(), self.ciphersuite)
                    .public;
                Extension::ExternalPub(ExternalPubExtension::new(HpkePublicKey::from(external_pub)))
            };

            if with_ratchet_tree {
                vec![ratchet_tree_extension(), external_pub_extension()]
            } else {
                vec![external_pub_extension()]
            }
        };

        // Create to-be-signed group info.
        let group_info_tbs = GroupInfoTBS::new(
            self.group_context.clone(),
            &extensions,
            self.message_secrets()
                .confirmation_key()
                .tag(backend, self.context().confirmed_transcript_hash())
                .map_err(LibraryError::unexpected_crypto_error)?,
            self.own_leaf_index(),
        );

        // Sign to-be-signed group info.
        group_info_tbs.sign(backend, credential_bundle)
    }

    /// Returns the epoch authenticator
    pub(crate) fn epoch_authenticator(&self) -> &EpochAuthenticator {
        self.group_epoch_secrets().epoch_authenticator()
    }

    /// Returns the resumption PSK secret
    pub(crate) fn resumption_psk_secret(&self) -> &ResumptionPskSecret {
        self.group_epoch_secrets().resumption_psk()
    }

    /// Loads the state from persisted state
    #[cfg(test)]
    pub(crate) fn load<R: Read>(reader: R) -> Result<CoreGroup, Error> {
        serde_json::from_reader(reader).map_err(|e| e.into())
    }

    /// Persists the state
    #[cfg(test)]
    pub(crate) fn save<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        let serialized_core_group = serde_json::to_string_pretty(self)?;
        writer.write_all(&serialized_core_group.into_bytes())
    }

    /// Returns a reference to the ratchet tree
    pub(crate) fn treesync(&self) -> &TreeSync {
        &self.tree
    }

    /// Get the ciphersuite implementation used in this group.
    pub(crate) fn ciphersuite(&self) -> Ciphersuite {
        self.ciphersuite
    }

    /// Get the MLS version used in this group.
    pub(crate) fn version(&self) -> ProtocolVersion {
        self.mls_version
    }

    /// Get the group context
    pub(crate) fn context(&self) -> &GroupContext {
        &self.group_context
    }

    /// Get the group ID
    pub(crate) fn group_id(&self) -> &GroupId {
        self.group_context.group_id()
    }

    /// Get the group context extensions.
    pub(crate) fn group_context_extensions(&self) -> &[Extension] {
        self.group_context.extensions()
    }

    /// Get the required capabilities extension of this group.
    pub(crate) fn required_capabilities(&self) -> Option<&RequiredCapabilitiesExtension> {
        self.group_context.required_capabilities()
    }

    /// Returns `true` if the group uses the ratchet tree extension anf `false
    /// otherwise
    #[cfg(test)]
    pub(crate) fn use_ratchet_tree_extension(&self) -> bool {
        self.use_ratchet_tree_extension
    }
}

// Private and crate functions
impl CoreGroup {
    /// Get the leaf index of this client.
    pub(crate) fn own_leaf_index(&self) -> u32 {
        self.treesync().own_leaf_index()
    }

    /// Get the identity of the client's [`Credential`] owning this group.
    pub(crate) fn own_identity(&self) -> Option<&[u8]> {
        self.treesync()
            .own_leaf_node()
            .ok()
            .map(|node| node.credential().identity())
    }

    /// Get a reference to the group epoch secrets from the group
    pub(crate) fn group_epoch_secrets(&self) -> &GroupEpochSecrets {
        &self.group_epoch_secrets
    }

    /// Get a reference to the message secrets from a group
    pub(crate) fn message_secrets(&self) -> &MessageSecrets {
        self.message_secrets_store.message_secrets()
    }

    /// Sets the size of the [`MessageSecretsStore`], i.e. the number of past
    /// epochs to keep.
    /// This allows application messages from previous epochs to be decrypted.
    pub(crate) fn set_max_past_epochs(&mut self, max_past_epochs: usize) {
        self.message_secrets_store.resize(max_past_epochs);
    }

    /// Get the message secrets. Either from the secrets store or from the group.
    pub(crate) fn message_secrets_mut(
        &mut self,
        epoch: GroupEpoch,
    ) -> Result<&mut MessageSecrets, SecretTreeError> {
        if epoch < self.context().epoch() {
            self.message_secrets_store
                .secrets_for_epoch_mut(epoch)
                .ok_or(SecretTreeError::TooDistantInThePast)
        } else {
            Ok(self.message_secrets_store.message_secrets_mut())
        }
    }

    /// Get the message secrets. Either from the secrets store or from the group.
    pub(crate) fn message_secrets_for_epoch(
        &self,
        epoch: GroupEpoch,
    ) -> Result<&MessageSecrets, SecretTreeError> {
        if epoch < self.context().epoch() {
            self.message_secrets_store
                .secrets_for_epoch(epoch)
                .ok_or(SecretTreeError::TooDistantInThePast)
        } else {
            Ok(self.message_secrets_store.message_secrets())
        }
    }

    /// Get the message secrets and leaves for the given epoch. Either from the
    /// secrets store or from the group.
    ///
    /// Note that the leaves vector is empty for message secrets of the current
    /// epoch. The caller can use treesync in this case.
    pub(crate) fn message_secrets_and_leaves_mut(
        &mut self,
        epoch: GroupEpoch,
    ) -> Result<(&mut MessageSecrets, &[Member]), MessageDecryptionError> {
        if epoch < self.context().epoch() {
            self.message_secrets_store
                .secrets_and_leaves_for_epoch_mut(epoch)
                .ok_or({
                    MessageDecryptionError::SecretTreeError(SecretTreeError::TooDistantInThePast)
                })
        } else {
            // No need for leaves here. The tree of the current epoch is
            // available to the caller.
            Ok((self.message_secrets_store.message_secrets_mut(), &[]))
        }
    }

    #[cfg(any(feature = "test-utils", test))]
    pub(crate) fn message_secrets_test_mut(&mut self) -> &mut MessageSecrets {
        self.message_secrets_store.message_secrets_mut()
    }

    #[cfg(any(feature = "test-utils", test))]
    pub(crate) fn context_mut(&mut self) -> &mut GroupContext {
        &mut self.group_context
    }

    #[cfg(any(feature = "test-utils", test))]
    pub(crate) fn print_tree(&self, message: &str) {
        use super::tests::tree_printing::print_tree;

        print_tree(self, message);
    }
}

// Helper functions

pub(crate) fn update_confirmed_transcript_hash(
    ciphersuite: Ciphersuite,
    backend: &impl OpenMlsCryptoProvider,
    mls_plaintext_commit_content: &ConfirmedTranscriptHashInput,
    interim_transcript_hash: &[u8],
) -> Result<Vec<u8>, LibraryError> {
    let commit_content_bytes = mls_plaintext_commit_content
        .tls_serialize_detached()
        .map_err(LibraryError::missing_bound_check)?;
    backend
        .crypto()
        .hash(
            ciphersuite.hash_algorithm(),
            &[interim_transcript_hash, &commit_content_bytes].concat(),
        )
        .map_err(LibraryError::unexpected_crypto_error)
}

pub(crate) fn update_interim_transcript_hash(
    ciphersuite: Ciphersuite,
    backend: &impl OpenMlsCryptoProvider,
    mls_plaintext_commit_auth_data: &InterimTranscriptHashInput,
    confirmed_transcript_hash: &[u8],
) -> Result<Vec<u8>, LibraryError> {
    let commit_auth_data_bytes = mls_plaintext_commit_auth_data
        .tls_serialize_detached()
        .map_err(LibraryError::missing_bound_check)?;
    backend
        .crypto()
        .hash(
            ciphersuite.hash_algorithm(),
            &[confirmed_transcript_hash, &commit_auth_data_bytes].concat(),
        )
        .map_err(LibraryError::unexpected_crypto_error)
}

/// Configuration for core group.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct CoreGroupConfig {
    /// Flag whether to send the ratchet tree along with the `GroupInfo` or not.
    /// Defaults to false.
    pub(crate) add_ratchet_tree_extension: bool,
}
