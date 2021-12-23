use crate::treesync::diff::StagedTreeSyncDiff;

use super::proposals::{ProposalStore, StagedProposal, StagedProposalQueue};

use super::super::errors::*;
use super::proposals::{
    StagedAddProposal, StagedPskProposal, StagedRemoveProposal, StagedUpdateProposal,
};
use super::*;
use core::fmt::Debug;
use std::mem;

impl CoreGroup {
    /// Stages a commit message.
    /// This function does the following:
    ///  - Applies the proposals covered by the commit to the tree
    ///  - Applies the (optional) update path to the tree
    ///  - Calculates the path secrets
    ///  - Initializes the key schedule for epoch rollover
    ///  - Verifies the confirmation tag/membership tag
    /// Returns a [StagedCommit] that can be inspected and later merged
    /// into the group state with [CoreGroup::merge_commit()]
    /// This function does the following checks:
    ///  - ValSem100
    ///  - ValSem101
    ///  - ValSem102
    ///  - ValSem103
    ///  - ValSem104
    ///  - ValSem105
    ///  - ValSem106
    ///  - ValSem107
    ///  - ValSem108
    ///  - ValSem109
    ///  - ValSem110
    ///  - ValSem201
    ///  - ValSem205
    pub fn stage_commit(
        &mut self,
        mls_plaintext: &MlsPlaintext,
        proposal_store: &ProposalStore,
        own_key_packages: &[KeyPackageBundle],
        backend: &impl OpenMlsCryptoProvider,
    ) -> Result<StagedCommit, CoreGroupError> {
        let ciphersuite = self.ciphersuite();

        // Extract the sender of the Commit message
        let sender = *mls_plaintext.sender();

        // Verify epoch
        if mls_plaintext.epoch() != self.group_context.epoch() {
            log::error!(
                "Epoch mismatch. Got {:?}, expected {:?}",
                mls_plaintext.epoch(),
                self.group_context.epoch()
            );
            return Err(StageCommitError::EpochMismatch.into());
        }

        // Extract Commit & Confirmation Tag from MlsPlaintext
        let commit = match mls_plaintext.content() {
            MlsPlaintextContentType::Commit(commit) => commit,
            _ => return Err(StageCommitError::WrongPlaintextContentType.into()),
        };

        let received_confirmation_tag = mls_plaintext
            .confirmation_tag()
            .ok_or(StageCommitError::ConfirmationTagMissing)?;

        // Build a queue with all proposals from the Commit and check that we have all
        // of the proposals by reference locally
        let mut proposal_queue = StagedProposalQueue::from_committed_proposals(
            ciphersuite,
            backend,
            commit.proposals.as_slice().to_vec(),
            proposal_store,
            sender,
        )
        .map_err(|_| StageCommitError::MissingProposal)?;

        // TODO #424: This won't be necessary anymore, we can just apply the proposals first
        // and add a new fake Update proposal to the queue after that
        let path_key_package = commit
            .path()
            .as_ref()
            .map(|update_path| update_path.leaf_key_package().clone());

        let sender_key_package_tuple = path_key_package
            .as_ref()
            .map(|key_package| (sender, key_package));

        // Validate the staged proposals by doing the following checks:

        // ValSem100
        // ValSem101
        // ValSem102
        // ValSem103
        // ValSem104
        // ValSem105
        // ValSem106
        self.validate_add_proposals(&proposal_queue)?;
        // ValSem107
        // ValSem108
        self.validate_remove_proposals(&proposal_queue)?;
        // ValSem109
        // ValSem110
        self.validate_update_proposals(&proposal_queue, sender_key_package_tuple)?;

        // Create provisional tree and apply proposals
        let mut diff = self.treesync().empty_diff()?;

        let apply_proposals_values = self
            .apply_staged_proposals(&mut diff, backend, &proposal_queue, own_key_packages)
            .map_err(|_| StageCommitError::OwnKeyNotFound)?;

        // Check if we were removed from the group
        if apply_proposals_values.self_removed {
            return Ok(StagedCommit {
                staged_proposal_queue: proposal_queue,
                state: None,
            });
        }

        // Determine if Commit is own Commit
        let sender = mls_plaintext.sender_index();
        let is_own_commit = sender == self.treesync().own_leaf_index();

        // Determine if Commit has a path
        let commit_secret = if let Some(path) = commit.path.clone() {
            // Verify KeyPackage and MlsPlaintext membership tag
            // Note that the signature must have been verified already.
            // TODO #106: Support external members
            let kp = path.leaf_key_package();
            if kp.verify(backend).is_err() {
                return Err(StageCommitError::PathKeyPackageVerificationFailure.into());
            }
            let serialized_context = self.group_context.tls_serialize_detached()?;

            if is_own_commit {
                // Find the right KeyPackageBundle among the pending bundles and
                // clone out the one that we need.
                let own_kpb = own_key_packages
                    .iter()
                    .find(|kpb| kpb.key_package() == kp)
                    .ok_or(StageCommitError::MissingOwnKeyPackage)?;
                diff.re_apply_own_update_path(backend, ciphersuite, own_kpb)?
            } else {
                let (key_package, update_path_nodes) = path.into_parts();

                // If the committer is a `NewMember`, we have to add the leaf to
                // the tree before we can apply or even decrypt an update path.
                // While `apply_received_update_path` will happily update a
                // blank leaf, we still have to call `add_leaf` here in case
                // there are no blanks and the new member extended the tree to
                // fit in.
                if apply_proposals_values.external_init_secret_option.is_some() {
                    diff.add_leaf(key_package.clone())?;
                }

                // Decrypt the UpdatePath
                let (plain_path, commit_secret) = diff.decrypt_path(
                    backend,
                    ciphersuite,
                    self.mls_version,
                    update_path_nodes,
                    sender,
                    &apply_proposals_values.exclusion_list(),
                    &serialized_context,
                )?;

                diff.apply_received_update_path(
                    backend,
                    ciphersuite,
                    sender,
                    key_package,
                    plain_path,
                )?;
                commit_secret
            }
        } else {
            if apply_proposals_values.path_required {
                // ValSem201
                return Err(StageCommitError::RequiredPathNotFound.into());
            }
            CommitSecret::zero_secret(ciphersuite, self.mls_version)
        };

        // Check if we need to include the init secret from an external commit
        // we applied earlier or if we use the one from the previous epoch.
        let init_secret =
            if let Some(ref init_secret) = apply_proposals_values.external_init_secret_option {
                init_secret
            } else {
                self.group_epoch_secrets
                    .init_secret()
                    .ok_or(StageCommitError::InitSecretNotFound)?
            };

        let joiner_secret = JoinerSecret::new(backend, commit_secret, init_secret)?;

        // Create provisional group state
        let mut provisional_epoch = self.group_context.epoch();
        provisional_epoch.increment();

        let confirmed_transcript_hash = update_confirmed_transcript_hash(
            ciphersuite,
            backend,
            // It is ok to use return a library error here, because we know the MlsPlaintext contains a Commit
            &MlsPlaintextCommitContent::try_from(mls_plaintext)
                .map_err(|_| CoreGroupError::LibraryError)?,
            &self.interim_transcript_hash,
        )?;

        let provisional_group_context = GroupContext::new(
            self.group_context.group_id().clone(),
            provisional_epoch,
            diff.compute_tree_hashes(backend, ciphersuite)?,
            confirmed_transcript_hash.clone(),
            self.group_context.extensions(),
        )?;

        // Prepare the PskSecret
        let psk_secret = PskSecret::new(
            ciphersuite,
            backend,
            apply_proposals_values.presharedkeys.psks(),
        )?;

        // Create key schedule
        let mut key_schedule = KeySchedule::init(ciphersuite, backend, joiner_secret, psk_secret)?;

        let serialized_provisional_group_context =
            provisional_group_context.tls_serialize_detached()?;

        key_schedule.add_context(backend, &serialized_provisional_group_context)?;
        let provisional_epoch_secrets = key_schedule.epoch_secrets(backend, true)?;

        let mls_plaintext_commit_auth_data = MlsPlaintextCommitAuthData::try_from(mls_plaintext)
            .map_err(|_| {
                log::error!("Confirmation tag is missing in commit. This should be unreachable because we verified the tag before.");
                StageCommitError::ConfirmationTagMissing
            })?;

        let interim_transcript_hash = update_interim_transcript_hash(
            ciphersuite,
            backend,
            &mls_plaintext_commit_auth_data,
            &confirmed_transcript_hash,
        )?;

        // Verify confirmation tag
        // ValSem205
        let own_confirmation_tag = provisional_epoch_secrets
            .confirmation_key()
            .tag(backend, &confirmed_transcript_hash)?;
        if &own_confirmation_tag != received_confirmation_tag {
            log::error!("Confirmation tag mismatch");
            log_crypto!(trace, "  Got:      {:x?}", received_confirmation_tag);
            log_crypto!(trace, "  Expected: {:x?}", own_confirmation_tag);
            return Err(StageCommitError::ConfirmationTagMismatch.into());
        }

        // If there is a key package from the Commit's update path, add it to the proposal queue
        // TODO #424: This won't be necessary anymore, we can just apply the proposals first
        // and add a new fake Update proposal to the queue after that
        if let Some(key_package) = path_key_package {
            let proposal = Proposal::Update(UpdateProposal { key_package });
            let staged_proposal = StagedProposal::from_proposal_and_sender(
                ciphersuite,
                backend,
                proposal,
                *mls_plaintext.sender(),
            )
            .map_err(|_| CoreGroupError::LibraryError)?;
            proposal_queue.add(staged_proposal);
        }

        let (provisional_group_epoch_secrets, provisional_message_secrets) =
            provisional_epoch_secrets
                .split_secrets(serialized_provisional_group_context, diff.leaf_count());

        // Make the diff a staged diff. This finalizes the diff and no more changes can be applied to it.
        let staged_diff = diff.into_staged_diff(backend, ciphersuite)?;

        Ok(StagedCommit {
            staged_proposal_queue: proposal_queue,
            state: Some(StagedCommitState {
                group_context: provisional_group_context,
                group_epoch_secrets: provisional_group_epoch_secrets,
                message_secrets: provisional_message_secrets,
                interim_transcript_hash,
                staged_diff,
            }),
        })
    }

    /// Merges a [StagedCommit] into the group state.
    ///
    /// This function should not fail and only returns a [`Result`], because it
    /// might throw a `LibraryError`.
    #[cfg(any(feature = "test-utils", test))]
    pub fn merge_commit(&mut self, staged_commit: StagedCommit) -> Result<(), CoreGroupError> {
        if let Some(state) = staged_commit.state {
            self.group_context = state.group_context;
            self.group_epoch_secrets = state.group_epoch_secrets;
            self.message_secrets = state.message_secrets;
            self.interim_transcript_hash = state.interim_transcript_hash;
            self.tree.merge_diff(state.staged_diff)?;
        };
        Ok(())
    }

    /// Merges a [StagedCommit] into the group state and optionally return a [`SecretTree`]
    /// from the previous epoch. The secret tree is returned if the Commit does not contain a self removal.
    ///
    /// This function should not fail and only returns a [`Result`], because it
    /// might throw a `LibraryError`.
    pub fn merge_commit_take_message_secrets(
        &mut self,
        staged_commit: StagedCommit,
    ) -> Result<Option<MessageSecrets>, CoreGroupError> {
        Ok(if let Some(state) = staged_commit.state {
            self.group_context = state.group_context;
            self.group_epoch_secrets = state.group_epoch_secrets;

            // Replace the previous message secrets with the new ones and return the previous message secrets
            let mut message_secrets = state.message_secrets;
            mem::swap(&mut message_secrets, &mut self.message_secrets);

            self.interim_transcript_hash = state.interim_transcript_hash;

            self.tree.merge_diff(state.staged_diff)?;
            Some(message_secrets)
        } else {
            None
        })
    }
}

/// Contains the changes from a commit to the group state.
#[derive(Debug)]
pub struct StagedCommit {
    staged_proposal_queue: StagedProposalQueue,
    state: Option<StagedCommitState>,
}

impl StagedCommit {
    /// Create a new [`StagedCommit`] from the provisional group state created
    /// during the commit process.
    pub(crate) fn new(
        staged_proposal_queue: StagedProposalQueue,
        state: Option<StagedCommitState>,
    ) -> Self {
        StagedCommit {
            staged_proposal_queue,
            state,
        }
    }

    /// Returns the Add proposals that are covered by the Commit message as in iterator over [StagedAddProposal].
    pub fn add_proposals(&self) -> impl Iterator<Item = StagedAddProposal> {
        self.staged_proposal_queue.add_proposals()
    }

    /// Returns the Remove proposals that are covered by the Commit message as in iterator over [StagedRemoveProposal].
    pub fn remove_proposals(&self) -> impl Iterator<Item = StagedRemoveProposal> {
        self.staged_proposal_queue.remove_proposals()
    }

    /// Returns the Update proposals that are covered by the Commit message as in iterator over [StagedUpdateProposal].
    pub fn update_proposals(&self) -> impl Iterator<Item = StagedUpdateProposal> {
        self.staged_proposal_queue.update_proposals()
    }

    /// Returns the PresharedKey proposals that are covered by the Commit message as in iterator over [StagedPskProposal].
    pub fn psk_proposals(&self) -> impl Iterator<Item = StagedPskProposal> {
        self.staged_proposal_queue.psk_proposals()
    }

    /// Returns `true` if the member was removed through a proposal covered by this Commit message
    /// and `false` otherwise.
    pub fn self_removed(&self) -> bool {
        self.state.is_none()
    }
}

/// This struct is used internally by [StagedCommit] to encapsulate all the modified group state.
#[derive(Debug)]
pub(crate) struct StagedCommitState {
    group_context: GroupContext,
    group_epoch_secrets: GroupEpochSecrets,
    message_secrets: MessageSecrets,
    interim_transcript_hash: Vec<u8>,
    staged_diff: StagedTreeSyncDiff,
}

impl StagedCommitState {
    pub(super) fn new(
        group_context: GroupContext,
        group_epoch_secrets: GroupEpochSecrets,
        message_secrets: MessageSecrets,
        interim_transcript_hash: Vec<u8>,
        staged_diff: StagedTreeSyncDiff,
    ) -> Self {
        Self {
            group_context,
            group_epoch_secrets,
            message_secrets,
            interim_transcript_hash,
            staged_diff,
        }
    }
}