// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

// Copyright 2015 The etcd Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp;

use crate::eraftpb::{
    ConfChange, ConfChangeType, Entry, EntryType, HardState, Message, MessageType, Snapshot,
};
use hashbrown::{HashMap, HashSet};
use prost::Message as ProstMsg;
use rand::{self, Rng};

use super::errors::{Error, Result, StorageError};
use super::progress::progress_set::{CandidacyStatus, Configuration, ProgressSet};
use super::progress::{Progress, ProgressState};
use super::raft_log::RaftLog;
use super::read_only::{ReadOnly, ReadOnlyOption, ReadState};
use super::storage::Storage;
use super::Config;
use crate::util;

// CAMPAIGN_PRE_ELECTION represents the first phase of a normal election when
// Config.pre_vote is true.
const CAMPAIGN_PRE_ELECTION: &[u8] = b"CampaignPreElection";
// CAMPAIGN_ELECTION represents a normal (time-based) election (the second phase
// of the election when Config.pre_vote is true).
const CAMPAIGN_ELECTION: &[u8] = b"CampaignElection";
// CAMPAIGN_TRANSFER represents the type of leader transfer.
const CAMPAIGN_TRANSFER: &[u8] = b"CampaignTransfer";

/// The role of the node.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum StateRole {
    /// The node is a follower of the leader.
    Follower,
    /// The node could become a leader.
    Candidate,
    /// The node is a leader.
    Leader,
    /// The node could become a candidate, if `prevote` is enabled.
    PreCandidate,
}

impl Default for StateRole {
    fn default() -> StateRole {
        StateRole::Follower
    }
}

/// A constant represents invalid id of raft.
pub const INVALID_ID: u64 = 0;
/// A constant represents invalid index of raft log.
pub const INVALID_INDEX: u64 = 0;

/// SoftState provides state that is useful for logging and debugging.
/// The state is volatile and does not need to be persisted to the WAL.
#[derive(Default, PartialEq, Debug)]
pub struct SoftState {
    /// The potential leader of the cluster.
    pub leader_id: u64,
    /// The soft role this node may take.
    pub raft_state: StateRole,
}

/// A struct that represents the raft consensus itself. Stores details concerning the current
/// and possible state the system can take.
#[derive(Default, Getters)]
pub struct Raft<T: Storage> {
    /// The current election term.
    pub term: u64,

    /// Which peer this raft is voting for.
    pub vote: u64,

    /// The ID of this node.
    pub id: u64,

    /// The current read states.
    pub read_states: Vec<ReadState>,

    /// The persistent log.
    pub raft_log: RaftLog<T>,

    /// The maximum number of messages that can be inflight.
    pub max_inflight: usize,

    /// The maximum length (in bytes) of all the entries.
    pub max_msg_size: u64,

    prs: Option<ProgressSet>,

    /// The current role of this node.
    pub state: StateRole,

    /// Whether this is a learner node.
    ///
    /// Learners are not permitted to vote in elections, and are not counted for commit quorums.
    /// They do replicate data from the leader.
    pub is_learner: bool,

    /// The current votes for this node in an election.
    ///
    /// Reset when changing role.
    pub votes: HashMap<u64, bool>,

    /// The list of messages.
    pub msgs: Vec<Message>,

    /// The leader id
    pub leader_id: u64,

    /// ID of the leader transfer target when its value is not None.
    ///
    /// If this is Some(id), we follow the procedure defined in raft thesis 3.10.
    pub lead_transferee: Option<u64>,

    /// Only one conf change may be pending (in the log, but not yet
    /// applied) at a time. This is enforced via `pending_conf_index`, which
    /// is set to a value >= the log index of the latest pending
    /// configuration change (if any). Config changes are only allowed to
    /// be proposed if the leader's applied index is greater than this
    /// value.
    ///
    /// This value is conservatively set in cases where there may be a configuration change pending,
    /// but scanning the log is possibly expensive. This implies that the index stated here may not
    /// necessarily be a config change entry, and it may not be a `BeginMembershipChange` entry, even if
    /// we set this to one.
    pub pending_conf_index: u64,

    /// The last `BeginMembershipChange` entry. Once we make this change we exit the joint state.
    ///
    /// This is different than `pending_conf_index` since it is more specific, and also exact.
    /// While `pending_conf_index` is conservatively set at times to ensure safety in the
    /// one-by-one change method, in joint consensus based changes we track the state exactly. The
    /// index here **must** only be set when a `BeginMembershipChange` is present at that index.
    ///
    /// # Caveats
    ///
    /// It is important that whenever this is set that `pending_conf_index` is also set to the
    /// value if it is greater than the existing value.
    ///
    /// **Use `Raft::set_pending_membership_change()` to change this value.**
    #[get = "pub"]
    pending_membership_change: Option<ConfChange>,

    /// The queue of read-only requests.
    pub read_only: ReadOnly,

    /// Ticks since it reached last electionTimeout when it is leader or candidate.
    /// Number of ticks since it reached last electionTimeout or received a
    /// valid message from current leader when it is a follower.
    pub election_elapsed: usize,

    /// Number of ticks since it reached last heartbeatTimeout.
    /// only leader keeps heartbeatElapsed.
    heartbeat_elapsed: usize,

    /// Whether to check the quorum
    pub check_quorum: bool,

    /// Enable the prevote algorithm.
    ///
    /// This enables a pre-election vote round on Candidates prior to disrupting the cluster.
    ///
    /// Enable this if greater cluster stability is preferred over faster elections.
    pub pre_vote: bool,

    skip_bcast_commit: bool,
    batch_append: bool,

    heartbeat_timeout: usize,
    election_timeout: usize,

    // randomized_election_timeout is a random number between
    // [min_election_timeout, max_election_timeout - 1]. It gets reset
    // when raft changes its state to follower or candidate.
    randomized_election_timeout: usize,
    min_election_timeout: usize,
    max_election_timeout: usize,

    /// Tag is only used for logging
    tag: String,
}

trait AssertSend: Send {}

impl<T: Storage + Send> AssertSend for Raft<T> {}

fn new_message(to: u64, field_type: MessageType, from: Option<u64>) -> Message {
    let mut m = Message::default();
    m.set_to(to);
    if let Some(id) = from {
        m.set_from(id);
    }
    m.set_msg_type(field_type);
    m
}

/// Maps vote and pre_vote message types to their correspond responses.
pub fn vote_resp_msg_type(t: MessageType) -> MessageType {
    match t {
        MessageType::MsgRequestVote => MessageType::MsgRequestVoteResponse,
        MessageType::MsgRequestPreVote => MessageType::MsgRequestPreVoteResponse,
        _ => panic!("Not a vote message: {:?}", t),
    }
}

impl<T: Storage> Raft<T> {
    /// Creates a new raft for use on the node.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(c: &Config, store: T) -> Result<Raft<T>> {
        c.validate()?;
        let raft_state = store.initial_state()?;
        let conf_state = &raft_state.conf_state;
        let peers = conf_state.get_nodes();
        let learners = conf_state.get_learners();

        let mut r = Raft {
            id: c.id,
            read_states: Default::default(),
            raft_log: RaftLog::new(store, c.tag.clone()),
            max_inflight: c.max_inflight_msgs,
            max_msg_size: c.max_size_per_msg,
            prs: Some(ProgressSet::with_capacity(peers.len(), learners.len())),
            state: StateRole::Follower,
            is_learner: false,
            check_quorum: c.check_quorum,
            pre_vote: c.pre_vote,
            read_only: ReadOnly::new(c.read_only_option),
            heartbeat_timeout: c.heartbeat_tick,
            election_timeout: c.election_tick,
            votes: Default::default(),
            msgs: Default::default(),
            leader_id: Default::default(),
            lead_transferee: None,
            term: Default::default(),
            election_elapsed: Default::default(),
            pending_conf_index: Default::default(),
            pending_membership_change: Default::default(),
            vote: Default::default(),
            heartbeat_elapsed: Default::default(),
            randomized_election_timeout: 0,
            min_election_timeout: c.min_election_tick(),
            max_election_timeout: c.max_election_tick(),
            skip_bcast_commit: c.skip_bcast_commit,
            tag: c.tag.to_owned(),
            batch_append: c.batch_append,
        };
        for p in peers {
            let pr = Progress::new(1, r.max_inflight);
            if let Err(e) = r.mut_prs().insert_voter(*p, pr) {
                panic!("{}", e);
            }
        }
        for p in learners {
            let pr = Progress::new(1, r.max_inflight);
            if let Err(e) = r.mut_prs().insert_learner(*p, pr) {
                panic!("{}", e);
            };
            if *p == r.id {
                r.is_learner = true;
            }
        }

        if raft_state.hard_state != HardState::default() {
            r.load_state(&raft_state.hard_state);
        }
        if c.applied > 0 {
            r.commit_apply(c.applied);
        }
        let term = r.term;
        r.become_follower(term, INVALID_ID);

        // Used to resume Joint Consensus Changes
        let pending_conf_state = raft_state.pending_conf_state();
        let pending_conf_state_start_index = raft_state.pending_conf_state_start_index();
        match (pending_conf_state, pending_conf_state_start_index) {
            (Some(state), Some(idx)) => {
                r.begin_membership_change(&ConfChange::from((*idx, state.clone())))?;
            }
            (None, None) => (),
            _ => unreachable!("Should never find pending_conf_change without an index."),
        };

        info!(
            "{} newRaft [peers: {:?}, term: {:?}, commit: {}, applied: {}, last_index: {}, \
             last_term: {}, pending_membership_change: {:?}]",
            r.tag,
            r.prs().voters().collect::<Vec<_>>(),
            r.term,
            r.raft_log.committed,
            r.raft_log.get_applied(),
            r.raft_log.last_index(),
            r.raft_log.last_term(),
            r.pending_membership_change(),
        );
        Ok(r)
    }

    /// Grabs an immutable reference to the store.
    #[inline]
    pub fn get_store(&self) -> &T {
        self.raft_log.get_store()
    }

    /// Grabs a mutable reference to the store.
    #[inline]
    pub fn mut_store(&mut self) -> &mut T {
        self.raft_log.mut_store()
    }

    /// Grabs a reference to the snapshot
    #[inline]
    pub fn get_snap(&self) -> Option<&Snapshot> {
        self.raft_log.get_unstable().snapshot.as_ref()
    }

    /// Returns the number of pending read-only messages.
    #[inline]
    pub fn pending_read_count(&self) -> usize {
        self.read_only.pending_read_count()
    }

    /// Returns how many read states exist.
    #[inline]
    pub fn ready_read_count(&self) -> usize {
        self.read_states.len()
    }

    /// Returns a value representing the softstate at the time of calling.
    pub fn soft_state(&self) -> SoftState {
        SoftState {
            leader_id: self.leader_id,
            raft_state: self.state,
        }
    }

    /// Returns a value representing the hardstate at the time of calling.
    pub fn hard_state(&self) -> HardState {
        let mut hs = HardState::default();
        hs.set_term(self.term);
        hs.set_vote(self.vote);
        hs.set_commit(self.raft_log.committed);
        hs
    }

    /// Returns whether the current raft is in lease.
    pub fn in_lease(&self) -> bool {
        self.state == StateRole::Leader && self.check_quorum
    }

    /// For testing leader lease
    #[doc(hidden)]
    pub fn set_randomized_election_timeout(&mut self, t: usize) {
        assert!(self.min_election_timeout <= t && t < self.max_election_timeout);
        self.randomized_election_timeout = t;
    }

    /// Fetch the length of the election timeout.
    pub fn get_election_timeout(&self) -> usize {
        self.election_timeout
    }

    /// Fetch the length of the heartbeat timeout
    pub fn get_heartbeat_timeout(&self) -> usize {
        self.heartbeat_timeout
    }

    /// Fetch the number of ticks elapsed since last heartbeat.
    pub fn get_heartbeat_elapsed(&self) -> usize {
        self.heartbeat_elapsed
    }

    /// Return the length of the current randomized election timeout.
    pub fn get_randomized_election_timeout(&self) -> usize {
        self.randomized_election_timeout
    }

    /// Set whether skip broadcast empty commit messages at runtime.
    #[inline]
    pub fn skip_bcast_commit(&mut self, skip: bool) {
        self.skip_bcast_commit = skip;
    }

    /// Set when the peer began a joint consensus change.
    ///
    /// This will also set `pending_conf_index` if it is larger than the existing number.
    #[inline]
    fn set_pending_membership_change(&mut self, maybe_change: impl Into<Option<ConfChange>>) {
        let maybe_change = maybe_change.into();
        if let Some(ref change) = maybe_change {
            let index = change.get_start_index();
            assert!(self.pending_membership_change.is_none() || index == self.pending_conf_index);
            if index > self.pending_conf_index {
                self.pending_conf_index = index;
            }
        }
        self.pending_membership_change = maybe_change.clone();
    }

    /// Get the index which the pending membership change started at.
    ///
    /// > **Note:** This is an experimental feature.
    #[inline]
    pub fn began_membership_change_at(&self) -> Option<u64> {
        self.pending_membership_change
            .as_ref()
            .map(ConfChange::get_start_index)
    }

    /// Set whether batch append msg at runtime.
    #[inline]
    pub fn set_batch_append(&mut self, batch_append: bool) {
        self.batch_append = batch_append;
    }

    // send persists state to stable storage and then sends to its mailbox.
    fn send(&mut self, mut m: Message) {
        debug!("Sending from {} to {}: {:?}", self.id, m.get_to(), m);
        m.set_from(self.id);
        if m.get_msg_type() == MessageType::MsgRequestVote
            || m.get_msg_type() == MessageType::MsgRequestPreVote
            || m.get_msg_type() == MessageType::MsgRequestVoteResponse
            || m.get_msg_type() == MessageType::MsgRequestPreVoteResponse
        {
            if m.get_term() == 0 {
                // All {pre-,}campaign messages need to have the term set when
                // sending.
                // - MsgVote: m.Term is the term the node is campaigning for,
                //   non-zero as we increment the term when campaigning.
                // - MsgVoteResp: m.Term is the new r.Term if the MsgVote was
                //   granted, non-zero for the same reason MsgVote is
                // - MsgPreVote: m.Term is the term the node will campaign,
                //   non-zero as we use m.Term to indicate the next term we'll be
                //   campaigning for
                // - MsgPreVoteResp: m.Term is the term received in the original
                //   MsgPreVote if the pre-vote was granted, non-zero for the
                //   same reasons MsgPreVote is
                panic!(
                    "{} term should be set when sending {:?}",
                    self.tag,
                    m.get_msg_type()
                );
            }
        } else {
            if m.get_term() != 0 {
                panic!(
                    "{} term should not be set when sending {:?} (was {})",
                    self.tag,
                    m.get_msg_type(),
                    m.get_term()
                );
            }
            // do not attach term to MsgPropose, MsgReadIndex
            // proposals are a way to forward to the leader and
            // should be treated as local message.
            // MsgReadIndex is also forwarded to leader.
            if m.get_msg_type() != MessageType::MsgPropose
                && m.get_msg_type() != MessageType::MsgReadIndex
            {
                m.set_term(self.term);
            }
        }
        self.msgs.push(m);
    }

    fn prepare_send_snapshot(&mut self, m: &mut Message, pr: &mut Progress, to: u64) -> bool {
        if !pr.recent_active {
            debug!(
                "{} ignore sending snapshot to {} since it is not recently active",
                self.tag, to
            );
            return false;
        }

        m.set_msg_type(MessageType::MsgSnapshot);
        let snapshot_r = self.raft_log.snapshot();
        if let Err(e) = snapshot_r {
            if e == Error::Store(StorageError::SnapshotTemporarilyUnavailable) {
                debug!(
                    "{} failed to send snapshot to {} because snapshot is temporarily \
                     unavailable",
                    self.tag, to
                );
                return false;
            }
            panic!("{} unexpected error: {:?}", self.tag, e);
        }
        let snapshot = snapshot_r.unwrap();
        if snapshot.get_metadata().get_index() == 0 {
            panic!("{} need non-empty snapshot", self.tag);
        }
        let (sindex, sterm) = (
            snapshot.get_metadata().get_index(),
            snapshot.get_metadata().get_term(),
        );
        m.set_snapshot(snapshot);
        debug!(
            "{} [firstindex: {}, commit: {}] sent snapshot[index: {}, term: {}] to {} \
             [{:?}]",
            self.tag,
            self.raft_log.first_index(),
            self.raft_log.committed,
            sindex,
            sterm,
            to,
            pr
        );
        pr.become_snapshot(sindex);
        debug!(
            "{} paused sending replication messages to {} [{:?}]",
            self.tag, to, pr
        );
        true
    }

    fn prepare_send_entries(
        &mut self,
        m: &mut Message,
        pr: &mut Progress,
        term: u64,
        ents: Vec<Entry>,
    ) {
        m.set_msg_type(MessageType::MsgAppend);
        m.set_index(pr.next_idx - 1);
        m.set_log_term(term);
        m.set_entries(ents);
        m.set_commit(self.raft_log.committed);
        if !m.get_entries().is_empty() {
            let last = m.get_entries().last().unwrap().get_index();
            pr.update_state(last);
        }
    }

    fn try_batching(&mut self, to: u64, pr: &mut Progress, ents: &mut Vec<Entry>) -> bool {
        // if MsgAppend for the reciver already exists, try_batching
        // will append the entries to the existing MsgAppend
        let mut is_batched = false;
        for msg in &mut self.msgs {
            if msg.get_msg_type() == MessageType::MsgAppend && msg.get_to() == to {
                if !ents.is_empty() {
                    if !util::is_continuous_ents(msg, ents) {
                        return is_batched;
                    }
                    let mut batched_entries = msg.take_entries();
                    batched_entries.append(ents);
                    msg.set_entries(batched_entries);
                    let last_idx = msg.get_entries().last().unwrap().get_index();
                    pr.update_state(last_idx);
                }
                msg.set_commit(self.raft_log.committed);
                is_batched = true;
                break;
            }
        }
        is_batched
    }

    /// Sends RPC, with entries to the given peer.
    pub fn send_append(&mut self, to: u64, pr: &mut Progress) {
        if pr.is_paused() {
            trace!(
                "Skipping sending to {}, it's paused. Progress: {:?}",
                to,
                pr
            );
            return;
        }
        let term = self.raft_log.term(pr.next_idx - 1);
        let ents = self.raft_log.entries(pr.next_idx, self.max_msg_size);
        let mut m = Message::default();
        m.set_to(to);
        if term.is_err() || ents.is_err() {
            // send snapshot if we failed to get term or entries
            trace!(
                "{} Skipping sending to {}, term: {:?}, index: {}, ents: {:?}",
                self.tag,
                to,
                term,
                pr.next_idx,
                ents,
            );
            if !self.prepare_send_snapshot(&mut m, pr, to) {
                return;
            }
        } else {
            let mut ents = ents.unwrap();
            if self.batch_append {
                let batched = self.try_batching(to, pr, &mut ents);
                if batched {
                    return;
                }
            }
            let term = term.unwrap();
            self.prepare_send_entries(&mut m, pr, term, ents);
        }
        self.send(m);
    }

    // send_heartbeat sends an empty MsgAppend
    fn send_heartbeat(&mut self, to: u64, pr: &Progress, ctx: Option<Vec<u8>>) {
        // Attach the commit as min(to.matched, self.raft_log.committed).
        // When the leader sends out heartbeat message,
        // the receiver(follower) might not be matched with the leader
        // or it might not have all the committed entries.
        // The leader MUST NOT forward the follower's commit to
        // an unmatched index.
        let mut m = Message::default();
        m.set_to(to);
        m.set_msg_type(MessageType::MsgHeartbeat);
        let commit = cmp::min(pr.matched, self.raft_log.committed);
        m.set_commit(commit);
        if let Some(context) = ctx {
            m.set_context(context);
        }
        self.send(m);
    }

    /// Sends RPC, with entries to all peers that are not up-to-date
    /// according to the progress recorded in r.prs().
    pub fn bcast_append(&mut self) {
        let self_id = self.id;
        let mut prs = self.take_prs();
        prs.iter_mut()
            .filter(|&(id, _)| *id != self_id)
            .for_each(|(id, pr)| self.send_append(*id, pr));
        self.set_prs(prs);
    }

    /// Sends RPC, without entries to all the peers.
    pub fn bcast_heartbeat(&mut self) {
        let ctx = self.read_only.last_pending_request_ctx();
        self.bcast_heartbeat_with_ctx(ctx)
    }

    #[cfg_attr(feature = "cargo-clippy", allow(clippy::needless_pass_by_value))]
    fn bcast_heartbeat_with_ctx(&mut self, ctx: Option<Vec<u8>>) {
        let self_id = self.id;
        let mut prs = self.take_prs();
        prs.iter_mut()
            .filter(|&(id, _)| *id != self_id)
            .for_each(|(id, pr)| self.send_heartbeat(*id, pr, ctx.clone()));
        self.set_prs(prs);
    }

    /// Attempts to advance the commit index. Returns true if the commit index
    /// changed (in which case the caller should call `r.bcast_append`).
    pub fn maybe_commit(&mut self) -> bool {
        let mci = self.prs().maximal_committed_index();
        self.raft_log.maybe_commit(mci, self.term)
    }

    /// Commit that the Raft peer has applied up to the given index.
    ///
    /// Registers the new applied index to the Raft log.
    ///
    /// # Hooks
    ///
    /// * Post: Checks to see if it's time to finalize a Joint Consensus state.
    pub fn commit_apply(&mut self, applied: u64) {
        #[allow(deprecated)]
        self.raft_log.applied_to(applied);

        // Check to see if we need to finalize a Joint Consensus state now.
        let start_index = self
            .pending_membership_change
            .as_ref()
            .map(|v| Some(v.get_start_index()))
            .unwrap_or(None);

        if let Some(index) = start_index {
            // Invariant: We know that if we have commited past some index, we can also commit that index.
            if applied >= index && self.state == StateRole::Leader {
                // We must replicate the commit entry.
                self.append_finalize_conf_change_entry();
            }
        }
    }

    fn append_finalize_conf_change_entry(&mut self) {
        let mut conf_change = ConfChange::default();
        conf_change.set_change_type(ConfChangeType::FinalizeMembershipChange);
        let mut data = Vec::with_capacity(ProstMsg::encoded_len(&conf_change));
        conf_change.encode(&mut data).unwrap();
        let mut entry = Entry::default();
        entry.set_entry_type(EntryType::EntryConfChange);
        entry.set_data(data);
        // Index/Term set here.
        self.append_entry(&mut [entry]);
        self.bcast_append();
    }

    /// Resets the current node to a given term.
    pub fn reset(&mut self, term: u64) {
        if self.term != term {
            self.term = term;
            self.vote = INVALID_ID;
        }
        self.leader_id = INVALID_ID;
        self.reset_randomized_election_timeout();
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;

        self.abort_leader_transfer();

        self.votes.clear();

        self.pending_conf_index = 0;
        self.read_only = ReadOnly::new(self.read_only.option);

        let last_index = self.raft_log.last_index();
        let self_id = self.id;
        for (&id, pr) in self.mut_prs().iter_mut() {
            pr.reset(last_index + 1);
            if id == self_id {
                pr.matched = last_index;
            }
        }
    }

    /// Appends a slice of entries to the log. The entries are updated to match
    /// the current index and term.
    pub fn append_entry(&mut self, es: &mut [Entry]) {
        let mut li = self.raft_log.last_index();
        for (i, e) in es.iter_mut().enumerate() {
            e.set_term(self.term);
            e.set_index(li + 1 + i as u64);
        }
        // use latest "last" index after truncate/append
        li = self.raft_log.append(es);

        let self_id = self.id;
        self.mut_prs().get_mut(self_id).unwrap().maybe_update(li);

        // Regardless of maybe_commit's return, our caller will call bcastAppend.
        self.maybe_commit();
    }

    /// Returns true to indicate that there will probably be some readiness need to be handled.
    pub fn tick(&mut self) -> bool {
        match self.state {
            StateRole::Follower | StateRole::PreCandidate | StateRole::Candidate => {
                self.tick_election()
            }
            StateRole::Leader => self.tick_heartbeat(),
        }
    }

    // TODO: revoke pub when there is a better way to test.
    /// Run by followers and candidates after self.election_timeout.
    ///
    /// Returns true to indicate that there will probably be some readiness need to be handled.
    pub fn tick_election(&mut self) -> bool {
        self.election_elapsed += 1;
        if !self.pass_election_timeout() || !self.promotable() {
            return false;
        }

        self.election_elapsed = 0;
        let m = new_message(INVALID_ID, MessageType::MsgHup, Some(self.id));
        let _ = self.step(m);
        true
    }

    // tick_heartbeat is run by leaders to send a MsgBeat after self.heartbeat_timeout.
    // Returns true to indicate that there will probably be some readiness need to be handled.
    fn tick_heartbeat(&mut self) -> bool {
        self.heartbeat_elapsed += 1;
        self.election_elapsed += 1;

        let mut has_ready = false;
        if self.election_elapsed >= self.election_timeout {
            self.election_elapsed = 0;
            if self.check_quorum {
                let m = new_message(INVALID_ID, MessageType::MsgCheckQuorum, Some(self.id));
                has_ready = true;
                let _ = self.step(m);
            }
            if self.state == StateRole::Leader && self.lead_transferee.is_some() {
                self.abort_leader_transfer()
            }
        }

        if self.state != StateRole::Leader {
            return has_ready;
        }

        if self.heartbeat_elapsed >= self.heartbeat_timeout {
            self.heartbeat_elapsed = 0;
            has_ready = true;
            let m = new_message(INVALID_ID, MessageType::MsgBeat, Some(self.id));
            let _ = self.step(m);
        }
        has_ready
    }

    /// Converts this node to a follower.
    pub fn become_follower(&mut self, term: u64, leader_id: u64) {
        self.reset(term);
        self.leader_id = leader_id;
        self.state = StateRole::Follower;
        info!("{} became follower at term {}", self.tag, self.term);
    }

    // TODO: revoke pub when there is a better way to test.
    /// Converts this node to a candidate
    ///
    /// # Panics
    ///
    /// Panics if a leader already exists.
    pub fn become_candidate(&mut self) {
        assert_ne!(
            self.state,
            StateRole::Leader,
            "invalid transition [leader -> candidate]"
        );
        let term = self.term + 1;
        self.reset(term);
        let id = self.id;
        self.vote = id;
        self.state = StateRole::Candidate;
        info!("{} became candidate at term {}", self.tag, self.term);
    }

    /// Converts this node to a pre-candidate
    ///
    /// # Panics
    ///
    /// Panics if a leader already exists.
    pub fn become_pre_candidate(&mut self) {
        assert_ne!(
            self.state,
            StateRole::Leader,
            "invalid transition [leader -> pre-candidate]"
        );
        // Becoming a pre-candidate changes our state.
        // but doesn't change anything else. In particular it does not increase
        // self.term or change self.vote.
        self.state = StateRole::PreCandidate;
        self.votes = HashMap::default();
        // If a network partition happens, and leader is in minority partition,
        // it will step down, and become follower without notifying others.
        self.leader_id = INVALID_ID;
        info!("{} became pre-candidate at term {}", self.tag, self.term);
    }

    // TODO: revoke pub when there is a better way to test.
    /// Makes this raft the leader.
    ///
    /// # Panics
    ///
    /// Panics if this is a follower node.
    pub fn become_leader(&mut self) {
        trace!("ENTER become_leader");
        assert_ne!(
            self.state,
            StateRole::Follower,
            "invalid transition [follower -> leader]"
        );
        let term = self.term;
        self.reset(term);
        self.leader_id = self.id;
        self.state = StateRole::Leader;

        // Followers enter replicate mode when they've been successfully probed
        // (perhaps after having received a snapshot as a result). The leader is
        // trivially in this state. Note that r.reset() has initialized this
        // progress with the last index already.
        let id = self.id;
        self.mut_prs().get_mut(id).unwrap().become_replicate();

        // Conservatively set the pending_conf_index to the last index in the
        // log. There may or may not be a pending config change, but it's
        // safe to delay any future proposals until we commit all our
        // pending log entries, and scanning the entire tail of the log
        // could be expensive.
        self.pending_conf_index = self.raft_log.last_index();

        self.append_entry(&mut [Entry::default()]);

        // In most cases, we append only a new entry marked with an index and term.
        // In the specific case of a node recovering while in the middle of a membership change,
        // and the finalization entry may have been lost, we must also append that, since it
        // would be overwritten by the term change.
        let change_start_index = self
            .pending_membership_change
            .as_ref()
            .map(|v| Some(v.get_start_index()))
            .unwrap_or(None);
        if let Some(index) = change_start_index {
            trace!(
                "Checking if we need to finalize again..., began: {}, applied: {}, committed: {}",
                index,
                self.raft_log.applied,
                self.raft_log.committed
            );
            if index <= self.raft_log.committed {
                self.append_finalize_conf_change_entry();
            }
        }

        info!("{} became leader at term {}", self.tag, self.term);
        trace!("EXIT become_leader");
    }

    fn num_pending_conf(&self, ents: &[Entry]) -> usize {
        ents.iter()
            .filter(|e| e.get_entry_type() == EntryType::EntryConfChange)
            .count()
    }

    /// Campaign to attempt to become a leader.
    ///
    /// If prevote is enabled, this is handled as well.
    pub fn campaign(&mut self, campaign_type: &[u8]) {
        let (vote_msg, term) = if campaign_type == CAMPAIGN_PRE_ELECTION {
            self.become_pre_candidate();
            // Pre-vote RPCs are sent for next term before we've incremented self.term.
            (MessageType::MsgRequestPreVote, self.term + 1)
        } else {
            self.become_candidate();
            (MessageType::MsgRequestVote, self.term)
        };
        let self_id = self.id;
        let acceptance = true;
        info!(
            "{} received {:?} from {} at term {}",
            self.id, vote_msg, self_id, self.term
        );
        self.register_vote(self_id, acceptance);
        if let CandidacyStatus::Elected = self.prs().candidacy_status(&self.votes) {
            // We won the election after voting for ourselves (which must mean that
            // this is a single-node cluster). Advance to the next state.
            if campaign_type == CAMPAIGN_PRE_ELECTION {
                self.campaign(CAMPAIGN_ELECTION);
            } else {
                self.become_leader();
            }
            return;
        }

        // Only send vote request to voters.
        let prs = self.take_prs();
        prs.voter_ids()
            .iter()
            .filter(|&id| *id != self_id)
            .for_each(|&id| {
                info!(
                    "{} [logterm: {}, index: {}] sent {:?} request to {} at term {}",
                    self.tag,
                    self.raft_log.last_term(),
                    self.raft_log.last_index(),
                    vote_msg,
                    id,
                    self.term
                );
                let mut m = new_message(id, vote_msg, None);
                m.set_term(term);
                m.set_index(self.raft_log.last_index());
                m.set_log_term(self.raft_log.last_term());
                if campaign_type == CAMPAIGN_TRANSFER {
                    m.set_context(campaign_type.to_vec());
                }
                self.send(m);
            });
        self.set_prs(prs);
    }

    /// Sets the vote of `id` to `vote`.
    fn register_vote(&mut self, id: u64, vote: bool) {
        self.votes.entry(id).or_insert(vote);
    }

    /// Steps the raft along via a message. This should be called everytime your raft receives a
    /// message from a peer.
    pub fn step(&mut self, m: Message) -> Result<()> {
        // Handle the message term, which may result in our stepping down to a follower.
        if m.get_term() == 0 {
            // local message
        } else if m.get_term() > self.term {
            if m.get_msg_type() == MessageType::MsgRequestVote
                || m.get_msg_type() == MessageType::MsgRequestPreVote
            {
                let force = m.get_context() == CAMPAIGN_TRANSFER;
                let in_lease = self.check_quorum
                    && self.leader_id != INVALID_ID
                    && self.election_elapsed < self.election_timeout;
                if !force && in_lease {
                    // if a server receives RequestVote request within the minimum election
                    // timeout of hearing from a current leader, it does not update its term
                    // or grant its vote
                    //
                    // This is included in the 3rd concern for Joint Consensus, where if another
                    // peer is removed from the cluster it may try to hold elections and disrupt
                    // stability.
                    info!(
                        "{} [logterm: {}, index: {}, vote: {}] ignored {:?} vote from \
                         {} [logterm: {}, index: {}] at term {}: lease is not expired \
                         (remaining ticks: {})",
                        self.tag,
                        self.raft_log.last_term(),
                        self.raft_log.last_index(),
                        self.vote,
                        m.get_msg_type(),
                        m.get_from(),
                        m.get_log_term(),
                        m.get_index(),
                        self.term,
                        self.election_timeout - self.election_elapsed
                    );

                    return Ok(());
                }
            }

            if m.get_msg_type() == MessageType::MsgRequestPreVote
                || (m.get_msg_type() == MessageType::MsgRequestPreVoteResponse && !m.get_reject())
            {
                // For a pre-vote request:
                // Never change our term in response to a pre-vote request.
                //
                // For a pre-vote response with pre-vote granted:
                // We send pre-vote requests with a term in our future. If the
                // pre-vote is granted, we will increment our term when we get a
                // quorum. If it is not, the term comes from the node that
                // rejected our vote so we should become a follower at the new
                // term.
            } else {
                info!(
                    "{} [term: {}] received a {:?} message with higher term from {} [term: {}]",
                    self.tag,
                    self.term,
                    m.get_msg_type(),
                    m.get_from(),
                    m.get_term()
                );
                if m.get_msg_type() == MessageType::MsgAppend
                    || m.get_msg_type() == MessageType::MsgHeartbeat
                    || m.get_msg_type() == MessageType::MsgSnapshot
                {
                    self.become_follower(m.get_term(), m.get_from());
                } else {
                    self.become_follower(m.get_term(), INVALID_ID);
                }
            }
        } else if m.get_term() < self.term {
            if (self.check_quorum || self.pre_vote)
                && (m.get_msg_type() == MessageType::MsgHeartbeat
                    || m.get_msg_type() == MessageType::MsgAppend)
            {
                // We have received messages from a leader at a lower term. It is possible
                // that these messages were simply delayed in the network, but this could
                // also mean that this node has advanced its term number during a network
                // partition, and it is now unable to either win an election or to rejoin
                // the majority on the old term. If checkQuorum is false, this will be
                // handled by incrementing term numbers in response to MsgVote with a higher
                // term, but if checkQuorum is true we may not advance the term on MsgVote and
                // must generate other messages to advance the term. The net result of these
                // two features is to minimize the disruption caused by nodes that have been
                // removed from the cluster's configuration: a removed node will send MsgVotes
                // which will be ignored, but it will not receive MsgApp or MsgHeartbeat, so it
                // will not create disruptive term increases, by notifying leader of this node's
                // activeness.
                // The above comments also true for Pre-Vote
                //
                // When follower gets isolated, it soon starts an election ending
                // up with a higher term than leader, although it won't receive enough
                // votes to win the election. When it regains connectivity, this response
                // with "pb.MsgAppResp" of higher term would force leader to step down.
                // However, this disruption is inevitable to free this stuck node with
                // fresh election. This can be prevented with Pre-Vote phase.
                let to_send = new_message(m.get_from(), MessageType::MsgAppendResponse, None);
                self.send(to_send);
            } else if m.get_msg_type() == MessageType::MsgRequestPreVote {
                // Before pre_vote enable, there may be a recieving candidate with higher term,
                // but less log. After update to pre_vote, the cluster may deadlock if
                // we drop messages with a lower term.
                info!(
                    "{} [log_term: {}, index: {}, vote: {}] rejected {:?} from {} [log_term: {}, index: {}] at term {}",
                    self.id,
                    self.raft_log.last_term(),
                    self.raft_log.last_index(),
                    self.vote,
                    m.get_msg_type(),
                    m.get_from(),
                    m.get_log_term(),
                    m.get_index(),
                    self.term,
                );

                let mut to_send =
                    new_message(m.get_from(), MessageType::MsgRequestPreVoteResponse, None);
                to_send.set_term(self.term);
                to_send.set_reject(true);
                self.send(to_send);
            } else {
                // ignore other cases
                info!(
                    "{} [term: {}] ignored a {:?} message with lower term from {} [term: {}]",
                    self.tag,
                    self.term,
                    m.get_msg_type(),
                    m.get_from(),
                    m.get_term()
                );
            }
            return Ok(());
        }

        #[cfg(feature = "failpoint")]
        fail_point!("before_step");

        match m.get_msg_type() {
            MessageType::MsgHup => {
                if self.state != StateRole::Leader {
                    let ents = self
                        .raft_log
                        .slice(self.raft_log.applied + 1, self.raft_log.committed + 1, None)
                        .expect("unexpected error getting unapplied entries");
                    let n = self.num_pending_conf(&ents);
                    if n != 0 && self.raft_log.committed > self.raft_log.applied {
                        warn!(
                            "{} cannot campaign at term {} since there are still {} pending \
                             configuration changes to apply",
                            self.tag, self.term, n
                        );
                        return Ok(());
                    }
                    info!(
                        "{} is starting a new election at term {}",
                        self.tag, self.term
                    );
                    if self.pre_vote {
                        self.campaign(CAMPAIGN_PRE_ELECTION);
                    } else {
                        self.campaign(CAMPAIGN_ELECTION);
                    }
                } else {
                    debug!("{} ignoring MsgHup because already leader", self.tag);
                }
            }
            MessageType::MsgRequestVote | MessageType::MsgRequestPreVote => {
                debug_assert!(m.get_log_term() != 0, "{:?} log term can't be 0", m);

                // We can vote if this is a repeat of a vote we've already cast...
                let can_vote = (self.vote == m.get_from()) ||
                    // ...we haven't voted and we don't think there's a leader yet in this term...
                    (self.vote == INVALID_ID && self.leader_id == INVALID_ID) ||
                    // ...or this is a PreVote for a future term...
                    (m.msg_type == MessageType::MsgRequestPreVote as i32 && m.get_term() > self.term);
                // ...and we believe the candidate is up to date.
                if can_vote && self.raft_log.is_up_to_date(m.get_index(), m.get_log_term()) {
                    // When responding to Msg{Pre,}Vote messages we include the term
                    // from the message, not the local term. To see why consider the
                    // case where a single node was previously partitioned away and
                    // it's local term is now of date. If we include the local term
                    // (recall that for pre-votes we don't update the local term), the
                    // (pre-)campaigning node on the other end will proceed to ignore
                    // the message (it ignores all out of date messages).
                    // The term in the original message and current local term are the
                    // same in the case of regular votes, but different for pre-votes.
                    self.log_vote_approve(&m);
                    let mut to_send =
                        new_message(m.get_from(), vote_resp_msg_type(m.get_msg_type()), None);
                    to_send.set_reject(false);
                    to_send.set_term(m.get_term());
                    self.send(to_send);
                    if m.get_msg_type() == MessageType::MsgRequestVote {
                        // Only record real votes.
                        self.election_elapsed = 0;
                        self.vote = m.get_from();
                    }
                } else {
                    self.log_vote_reject(&m);
                    let mut to_send =
                        new_message(m.get_from(), vote_resp_msg_type(m.get_msg_type()), None);
                    to_send.set_reject(true);
                    to_send.set_term(self.term);
                    self.send(to_send);
                }
            }
            _ => match self.state {
                StateRole::PreCandidate | StateRole::Candidate => self.step_candidate(m)?,
                StateRole::Follower => self.step_follower(m)?,
                StateRole::Leader => self.step_leader(m)?,
            },
        }
        Ok(())
    }

    /// Apply a `BeginMembershipChange` variant `ConfChange`.
    ///
    /// > **Note:** This is an experimental feature.
    ///
    /// When a Raft node applies this variant of a configuration change it will adopt a joint
    /// configuration state until the membership change is finalized.
    ///
    /// During this time the `Raft` will have two, possibly overlapping, cooperating quorums for
    /// both elections and log replication.
    ///
    /// # Errors
    ///
    /// * `ConfChange.change_type` is not `BeginMembershipChange`
    /// * `ConfChange.configuration` does not exist.
    /// * `ConfChange.start_index` does not exist. It **must** equal the index of the
    ///   corresponding entry.
    #[inline(always)]
    pub fn begin_membership_change(&mut self, conf_change: &ConfChange) -> Result<()> {
        if conf_change.get_change_type() != ConfChangeType::BeginMembershipChange {
            return Err(Error::ViolatesContract(format!(
                "{:?} != BeginMembershipChange",
                conf_change.get_change_type()
            )));
        }
        let configuration = if conf_change.has_configuration() {
            conf_change.get_configuration().clone()
        } else {
            return Err(Error::ViolatesContract(
                "!ConfChange::has_configuration()".into(),
            ));
        };
        if conf_change.get_start_index() == 0 {
            return Err(Error::ViolatesContract(
                "!ConfChange::has_start_index()".into(),
            ));
        };

        self.set_pending_membership_change(conf_change.clone());
        let pr = Progress::new(self.raft_log.last_index() + 1, self.max_inflight);
        self.mut_prs().begin_membership_change(configuration, pr)?;
        Ok(())
    }

    /// Apply a `FinalizeMembershipChange` variant `ConfChange`.
    ///
    /// > **Note:** This is an experimental feature.
    ///
    /// When a Raft node applies this variant of a configuration change it will finalize the
    /// transition begun by [`begin_membership_change`].
    ///
    /// Once this is called the Raft will no longer have two, possibly overlapping, cooperating
    /// qourums.
    ///
    /// # Errors
    ///
    /// * This Raft is not in a configuration change via `begin_membership_change`.
    /// * `ConfChange.change_type` is not a `FinalizeMembershipChange`.
    /// * `ConfChange.configuration` value should not exist.
    /// * `ConfChange.start_index` value should not exist.
    #[inline(always)]
    pub fn finalize_membership_change(&mut self, conf_change: &ConfChange) -> Result<()> {
        if conf_change.get_change_type() != ConfChangeType::FinalizeMembershipChange {
            return Err(Error::ViolatesContract(format!(
                "{:?} != BeginMembershipChange",
                conf_change.get_change_type()
            )));
        }
        if conf_change.has_configuration() {
            return Err(Error::ViolatesContract(
                "ConfChange::has_configuration()".into(),
            ));
        };
        let leader_in_new_set = self
            .prs()
            .next_configuration()
            .as_ref()
            .map(|config| config.contains(self.leader_id))
            .ok_or_else(|| Error::NoPendingMembershipChange)?;

        // Joint Consensus, in the Raft paper, states the leader should step down and become a
        // follower if it is removed during a transition.
        if !leader_in_new_set {
            let last_term = self.raft_log.last_term();
            if self.state == StateRole::Leader {
                self.become_follower(last_term, INVALID_ID);
            } else {
                // It's no longer safe to lookup the ID in the ProgressSet, remove it.
                self.leader_id = INVALID_ID;
            }
        }

        self.mut_prs().finalize_membership_change()?;
        // Ensure we reset this on *any* node, since the leader might have failed
        // and we don't want to finalize twice.
        self.set_pending_membership_change(None);
        Ok(())
    }

    fn log_vote_approve(&self, m: &Message) {
        info!(
            "{} [logterm: {}, index: {}, vote: {}] cast {:?} for {} [logterm: {}, index: {}] \
             at term {}",
            self.tag,
            self.raft_log.last_term(),
            self.raft_log.last_index(),
            self.vote,
            m.get_msg_type(),
            m.get_from(),
            m.get_log_term(),
            m.get_index(),
            self.term
        );
    }

    fn log_vote_reject(&self, m: &Message) {
        info!(
            "{} [logterm: {}, index: {}, vote: {}] rejected {:?} from {} [logterm: {}, index: \
             {}] at term {}",
            self.tag,
            self.raft_log.last_term(),
            self.raft_log.last_index(),
            self.vote,
            m.get_msg_type(),
            m.get_from(),
            m.get_log_term(),
            m.get_index(),
            self.term
        );
    }

    fn handle_append_response(
        &mut self,
        m: &Message,
        prs: &mut ProgressSet,
        old_paused: &mut bool,
        send_append: &mut bool,
        maybe_commit: &mut bool,
    ) {
        let pr = prs.get_mut(m.get_from()).unwrap();
        pr.recent_active = true;

        if m.get_reject() {
            debug!(
                "{} received msgAppend rejection(lastindex: {}) from {} for index {}",
                self.tag,
                m.get_reject_hint(),
                m.get_from(),
                m.get_index()
            );

            if pr.maybe_decr_to(m.get_index(), m.get_reject_hint()) {
                debug!(
                    "{} decreased progress of {} to [{:?}]",
                    self.tag,
                    m.get_from(),
                    pr
                );
                if pr.state == ProgressState::Replicate {
                    pr.become_probe();
                }
                *send_append = true;
            }
            return;
        }

        *old_paused = pr.is_paused();
        if !pr.maybe_update(m.get_index()) {
            return;
        }

        // Transfer leadership is in progress.
        if let Some(lead_transferee) = self.lead_transferee {
            let last_index = self.raft_log.last_index();
            if m.get_from() == lead_transferee && pr.matched == last_index {
                info!(
                    "{} sent MsgTimeoutNow to {} after received MsgAppResp",
                    self.tag,
                    m.get_from()
                );
                self.send_timeout_now(m.get_from());
            }
        }

        match pr.state {
            ProgressState::Probe => pr.become_replicate(),
            ProgressState::Snapshot => {
                if !pr.maybe_snapshot_abort() {
                    return;
                }
                debug!(
                    "{} snapshot aborted, resumed sending replication messages to {} \
                     [{:?}]",
                    self.tag,
                    m.get_from(),
                    pr
                );
                pr.become_probe();
            }
            ProgressState::Replicate => pr.ins.free_to(m.get_index()),
        }
        *maybe_commit = true;
    }

    fn handle_heartbeat_response(
        &mut self,
        m: &Message,
        prs: &mut ProgressSet,
        send_append: &mut bool,
        more_to_send: &mut Vec<Message>,
    ) {
        // Update the node. Drop the value explicitly since we'll check the qourum after.
        {
            let pr = prs.get_mut(m.get_from()).unwrap();
            pr.recent_active = true;
            pr.resume();

            // free one slot for the full inflights window to allow progress.
            if pr.state == ProgressState::Replicate && pr.ins.full() {
                pr.ins.free_first_one();
            }
            if pr.matched < self.raft_log.last_index() {
                *send_append = true;
            }

            if self.read_only.option != ReadOnlyOption::Safe || m.get_context().is_empty() {
                return;
            }
        }

        if !prs.has_quorum(&self.read_only.recv_ack(m)) {
            return;
        }

        let rss = self.read_only.advance(m);
        for rs in rss {
            let mut req = rs.req;
            if req.get_from() == INVALID_ID || req.get_from() == self.id {
                // from local member
                let rs = ReadState {
                    index: rs.index,
                    request_ctx: req.take_entries()[0].take_data(),
                };
                self.read_states.push(rs);
            } else {
                let mut to_send = Message::default();
                to_send.set_to(req.get_from());
                to_send.set_msg_type(MessageType::MsgReadIndexResp);
                to_send.set_index(rs.index);
                to_send.set_entries(req.take_entries());
                more_to_send.push(to_send);
            }
        }
    }

    fn handle_transfer_leader(&mut self, m: &Message, prs: &mut ProgressSet) {
        let from = m.get_from();
        if prs.learner_ids().contains(&from) {
            debug!("{} is learner. Ignored transferring leadership", from);
            return;
        }
        let lead_transferee = from;
        let last_lead_transferee = self.lead_transferee;
        if last_lead_transferee.is_some() {
            if last_lead_transferee.unwrap() == lead_transferee {
                info!(
                    "{} [term {}] transfer leadership to {} is in progress, ignores request \
                     to same node {}",
                    self.tag, self.term, lead_transferee, lead_transferee
                );
                return;
            }
            self.abort_leader_transfer();
            info!(
                "{} [term {}] abort previous transferring leadership to {}",
                self.tag,
                self.term,
                last_lead_transferee.unwrap()
            );
        }
        if lead_transferee == self.id {
            debug!(
                "{} is already leader. Ignored transferring leadership to self",
                self.tag
            );
            return;
        }
        // Transfer leadership to third party.
        info!(
            "{} [term {}] starts to transfer leadership to {}",
            self.tag, self.term, lead_transferee
        );
        // Transfer leadership should be finished in one electionTimeout
        // so reset r.electionElapsed.
        self.election_elapsed = 0;
        self.lead_transferee = Some(lead_transferee);
        let pr = prs.get_mut(from).unwrap();
        if pr.matched == self.raft_log.last_index() {
            self.send_timeout_now(lead_transferee);
            info!(
                "{} sends MsgTimeoutNow to {} immediately as {} already has up-to-date log",
                self.tag, lead_transferee, lead_transferee
            );
        } else {
            self.send_append(lead_transferee, pr);
        }
    }

    fn handle_snapshot_status(&mut self, m: &Message, pr: &mut Progress) {
        if m.get_reject() {
            pr.snapshot_failure();
            pr.become_probe();
            debug!(
                "{} snapshot failed, resumed sending replication messages to {} [{:?}]",
                self.tag,
                m.get_from(),
                pr
            );
        } else {
            pr.become_probe();
            debug!(
                "{} snapshot succeeded, resumed sending replication messages to {} [{:?}]",
                self.tag,
                m.get_from(),
                pr
            );
        }
        // If snapshot finish, wait for the msgAppResp from the remote node before sending
        // out the next msgAppend.
        // If snapshot failure, wait for a heartbeat interval before next try
        pr.pause();
    }

    /// Check message's progress to decide which action should be taken.
    fn check_message_with_progress(
        &mut self,
        m: &mut Message,
        send_append: &mut bool,
        old_paused: &mut bool,
        maybe_commit: &mut bool,
        more_to_send: &mut Vec<Message>,
    ) {
        if self.prs().get(m.get_from()).is_none() {
            debug!("{} no progress available for {}", self.tag, m.get_from());
            return;
        }

        let mut prs = self.take_prs();
        match m.get_msg_type() {
            MessageType::MsgAppendResponse => {
                self.handle_append_response(m, &mut prs, old_paused, send_append, maybe_commit);
            }
            MessageType::MsgHeartbeatResponse => {
                self.handle_heartbeat_response(m, &mut prs, send_append, more_to_send);
            }
            MessageType::MsgSnapStatus => {
                let pr = prs.get_mut(m.get_from()).unwrap();
                if pr.state == ProgressState::Snapshot {
                    self.handle_snapshot_status(m, pr);
                }
            }
            MessageType::MsgUnreachable => {
                let pr = prs.get_mut(m.get_from()).unwrap();
                // During optimistic replication, if the remote becomes unreachable,
                // there is huge probability that a MsgAppend is lost.
                if pr.state == ProgressState::Replicate {
                    pr.become_probe();
                }
                debug!(
                    "{} failed to send message to {} because it is unreachable [{:?}]",
                    self.tag,
                    m.get_from(),
                    pr
                );
            }
            MessageType::MsgTransferLeader => {
                self.handle_transfer_leader(m, &mut prs);
            }
            _ => {}
        }
        self.set_prs(prs);
    }

    fn step_leader(&mut self, mut m: Message) -> Result<()> {
        // These message types do not require any progress for m.From.
        match m.get_msg_type() {
            MessageType::MsgBeat => {
                self.bcast_heartbeat();
                return Ok(());
            }
            MessageType::MsgCheckQuorum => {
                if !self.check_quorum_active() {
                    warn!(
                        "{} stepped down to follower since quorum is not active",
                        self.tag
                    );
                    let term = self.term;
                    self.become_follower(term, INVALID_ID);
                }
                return Ok(());
            }
            MessageType::MsgPropose => {
                if m.get_entries().is_empty() {
                    panic!("{} stepped empty MsgProp", self.tag);
                }
                if !self.prs().voter_ids().contains(&self.id) {
                    // If we are not currently a member of the range (i.e. this node
                    // was removed from the configuration while serving as leader),
                    // drop any new proposals.
                    return Err(Error::ProposalDropped);
                }
                if self.lead_transferee.is_some() {
                    debug!(
                        "{} [term {}] transfer leadership to {} is in progress; dropping \
                         proposal",
                        self.tag,
                        self.term,
                        self.lead_transferee.unwrap()
                    );
                    return Err(Error::ProposalDropped);
                }

                for (i, e) in m.mut_entries().iter_mut().enumerate() {
                    if e.get_entry_type() == EntryType::EntryConfChange {
                        if self.has_pending_conf() {
                            info!(
                                "propose conf {:?} ignored since pending unapplied \
                                 configuration [index {}, applied {}]",
                                e, self.pending_conf_index, self.raft_log.applied
                            );
                            *e = Entry::default();
                            e.set_entry_type(EntryType::EntryNormal);
                        } else {
                            self.pending_conf_index = self.raft_log.last_index() + i as u64 + 1;
                        }
                    }
                }
                self.append_entry(&mut m.mut_entries());
                self.bcast_append();
                return Ok(());
            }
            MessageType::MsgReadIndex => {
                if self.raft_log.term(self.raft_log.committed).unwrap_or(0) != self.term {
                    // Reject read only request when this leader has not committed any log entry
                    // in its term.
                    return Ok(());
                }

                let mut self_set = HashSet::default();
                self_set.insert(self.id);
                if !self.prs().has_quorum(&self_set) {
                    // thinking: use an interally defined context instead of the user given context.
                    // We can express this in terms of the term and index instead of
                    // a user-supplied value.
                    // This would allow multiple reads to piggyback on the same message.
                    match self.read_only.option {
                        ReadOnlyOption::Safe => {
                            let ctx = m.get_entries()[0].get_data().to_vec();
                            self.read_only.add_request(self.raft_log.committed, m);
                            self.bcast_heartbeat_with_ctx(Some(ctx));
                        }
                        ReadOnlyOption::LeaseBased => {
                            let read_index = self.raft_log.committed;
                            if m.get_from() == INVALID_ID || m.get_from() == self.id {
                                // from local member
                                let rs = ReadState {
                                    index: read_index,
                                    request_ctx: m.take_entries()[0].take_data(),
                                };
                                self.read_states.push(rs);
                            } else {
                                let mut to_send = Message::default();
                                to_send.set_to(m.get_from());
                                to_send.set_msg_type(MessageType::MsgReadIndexResp);
                                to_send.set_index(read_index);
                                to_send.set_entries(m.take_entries());
                                self.send(to_send);
                            }
                        }
                    }
                } else {
                    let rs = ReadState {
                        index: self.raft_log.committed,
                        request_ctx: m.take_entries()[0].take_data(),
                    };
                    self.read_states.push(rs);
                }
                return Ok(());
            }
            _ => {}
        }

        let mut send_append = false;
        let mut maybe_commit = false;
        let mut old_paused = false;
        let mut more_to_send = vec![];
        self.check_message_with_progress(
            &mut m,
            &mut send_append,
            &mut old_paused,
            &mut maybe_commit,
            &mut more_to_send,
        );
        if maybe_commit {
            if self.maybe_commit() {
                if self.should_bcast_commit() {
                    self.bcast_append();
                }
            } else if old_paused {
                // update() reset the wait state on this node. If we had delayed sending
                // an update before, send it now.
                send_append = true;
            }
        }

        if send_append {
            let from = m.get_from();
            let mut prs = self.take_prs();
            self.send_append(from, prs.get_mut(from).unwrap());
            self.set_prs(prs);
        }
        if !more_to_send.is_empty() {
            for to_send in more_to_send.drain(..) {
                self.send(to_send);
            }
        }

        Ok(())
    }

    // step_candidate is shared by state Candidate and PreCandidate; the difference is
    // whether they respond to MsgRequestVote or MsgRequestPreVote.
    fn step_candidate(&mut self, m: Message) -> Result<()> {
        match m.get_msg_type() {
            MessageType::MsgPropose => {
                info!(
                    "{} no leader at term {}; dropping proposal",
                    self.tag, self.term
                );
                return Err(Error::ProposalDropped);
            }
            MessageType::MsgAppend => {
                debug_assert_eq!(self.term, m.get_term());
                self.become_follower(m.get_term(), m.get_from());
                self.handle_append_entries(&m);
            }
            MessageType::MsgHeartbeat => {
                debug_assert_eq!(self.term, m.get_term());
                self.become_follower(m.get_term(), m.get_from());
                self.handle_heartbeat(m);
            }
            MessageType::MsgSnapshot => {
                debug_assert_eq!(self.term, m.get_term());
                self.become_follower(m.get_term(), m.get_from());
                self.handle_snapshot(m);
            }
            MessageType::MsgRequestPreVoteResponse | MessageType::MsgRequestVoteResponse => {
                // Only handle vote responses corresponding to our candidacy (while in
                // state Candidate, we may get stale MsgPreVoteResp messages in this term from
                // our pre-candidate state).
                if (self.state == StateRole::PreCandidate
                    && m.get_msg_type() != MessageType::MsgRequestPreVoteResponse)
                    || (self.state == StateRole::Candidate
                        && m.get_msg_type() != MessageType::MsgRequestVoteResponse)
                {
                    return Ok(());
                }

                let acceptance = !m.get_reject();
                let msg_type = m.get_msg_type();
                let from_id = m.get_from();
                info!(
                    "{} received {:?}{} from {} at term {}",
                    self.id,
                    msg_type,
                    if !acceptance { " rejection" } else { "" },
                    from_id,
                    self.term
                );
                self.register_vote(from_id, acceptance);
                match self.prs().candidacy_status(&self.votes) {
                    CandidacyStatus::Elected => {
                        if self.state == StateRole::PreCandidate {
                            self.campaign(CAMPAIGN_ELECTION);
                        } else {
                            self.become_leader();
                            self.bcast_append();
                        }
                    }
                    CandidacyStatus::Ineligible => {
                        // pb.MsgPreVoteResp contains future term of pre-candidate
                        // m.term > self.term; reuse self.term
                        let term = self.term;
                        self.become_follower(term, INVALID_ID);
                    }
                    CandidacyStatus::Eligible => (),
                };
            }
            MessageType::MsgTimeoutNow => debug!(
                "{} [term {} state {:?}] ignored MsgTimeoutNow from {}",
                self.tag,
                self.term,
                self.state,
                m.get_from()
            ),
            _ => {}
        }
        Ok(())
    }

    fn step_follower(&mut self, mut m: Message) -> Result<()> {
        match m.get_msg_type() {
            MessageType::MsgPropose => {
                if self.leader_id == INVALID_ID {
                    info!(
                        "{} no leader at term {}; dropping proposal",
                        self.tag, self.term
                    );
                    return Err(Error::ProposalDropped);
                }
                m.set_to(self.leader_id);
                self.send(m);
            }
            MessageType::MsgAppend => {
                self.election_elapsed = 0;
                self.leader_id = m.get_from();
                self.handle_append_entries(&m);
            }
            MessageType::MsgHeartbeat => {
                self.election_elapsed = 0;
                self.leader_id = m.get_from();
                self.handle_heartbeat(m);
            }
            MessageType::MsgSnapshot => {
                self.election_elapsed = 0;
                self.leader_id = m.get_from();
                self.handle_snapshot(m);
            }
            MessageType::MsgTransferLeader => {
                if self.leader_id == INVALID_ID {
                    info!(
                        "{} no leader at term {}; dropping leader transfer msg",
                        self.tag, self.term
                    );
                    return Ok(());
                }
                m.set_to(self.leader_id);
                self.send(m);
            }
            MessageType::MsgTimeoutNow => {
                if self.promotable() {
                    info!(
                        "{} [term {}] received MsgTimeoutNow from {} and starts an election to \
                         get leadership.",
                        self.tag,
                        self.term,
                        m.get_from()
                    );
                    // Leadership transfers never use pre-vote even if self.pre_vote is true; we
                    // know we are not recovering from a partition so there is no need for the
                    // extra round trip.
                    self.campaign(CAMPAIGN_TRANSFER);
                } else {
                    info!(
                        "{} received MsgTimeoutNow from {} but is not promotable",
                        self.tag,
                        m.get_from()
                    );
                }
            }
            MessageType::MsgReadIndex => {
                if self.leader_id == INVALID_ID {
                    info!(
                        "{} no leader at term {}; dropping index reading msg",
                        self.tag, self.term
                    );
                    return Ok(());
                }
                m.set_to(self.leader_id);
                self.send(m);
            }
            MessageType::MsgReadIndexResp => {
                if m.get_entries().len() != 1 {
                    error!(
                        "{} invalid format of MsgReadIndexResp from {}, entries count: {}",
                        self.tag,
                        m.get_from(),
                        m.get_entries().len()
                    );
                    return Ok(());
                }
                let rs = ReadState {
                    index: m.get_index(),
                    request_ctx: m.take_entries()[0].take_data(),
                };
                self.read_states.push(rs);
            }
            _ => {}
        }
        Ok(())
    }

    // TODO: revoke pub when there is a better way to test.
    /// For a given message, append the entries to the log.
    pub fn handle_append_entries(&mut self, m: &Message) {
        if m.get_index() < self.raft_log.committed {
            debug!("{} Got message with lower index than committed.", self.tag);
            let mut to_send = Message::default();
            to_send.set_to(m.get_from());
            to_send.set_msg_type(MessageType::MsgAppendResponse);
            to_send.set_index(self.raft_log.committed);
            self.send(to_send);
            return;
        }
        debug_assert!(m.get_log_term() != 0, "{:?} log term can't be 0", m);

        let mut to_send = Message::default();
        to_send.set_to(m.get_from());
        to_send.set_msg_type(MessageType::MsgAppendResponse);
        match self.raft_log.maybe_append(
            m.get_index(),
            m.get_log_term(),
            m.get_commit(),
            m.get_entries(),
        ) {
            Some(mlast_index) => {
                to_send.set_index(mlast_index);
                self.send(to_send);
            }
            None => {
                debug!(
                    "{} [logterm: {:?}, index: {}] rejected msgApp [logterm: {}, index: {}] \
                     from {}",
                    self.tag,
                    self.raft_log.term(m.get_index()),
                    m.get_index(),
                    m.get_log_term(),
                    m.get_index(),
                    m.get_from()
                );
                to_send.set_index(m.get_index());
                to_send.set_reject(true);
                to_send.set_reject_hint(self.raft_log.last_index());
                self.send(to_send);
            }
        }
    }

    // TODO: revoke pub when there is a better way to test.
    /// For a message, commit and send out heartbeat.
    pub fn handle_heartbeat(&mut self, mut m: Message) {
        self.raft_log.commit_to(m.get_commit());
        let mut to_send = Message::default();
        to_send.set_to(m.get_from());
        to_send.set_msg_type(MessageType::MsgHeartbeatResponse);
        to_send.set_context(m.take_context());
        self.send(to_send);
    }

    fn handle_snapshot(&mut self, mut m: Message) {
        debug_assert!(m.get_term() != 0, "{:?} term can't be 0", m);
        let (sindex, sterm) = (
            m.get_snapshot().get_metadata().get_index(),
            m.get_snapshot().get_metadata().get_term(),
        );
        if self.restore(m.take_snapshot()) {
            info!(
                "{} [commit: {}, term: {}] restored snapshot [index: {}, term: {}]",
                self.tag, self.term, self.raft_log.committed, sindex, sterm
            );
            let mut to_send = Message::default();
            to_send.set_to(m.get_from());
            to_send.set_msg_type(MessageType::MsgAppendResponse);
            to_send.set_index(self.raft_log.last_index());
            self.send(to_send);
        } else {
            info!(
                "{} [commit: {}] ignored snapshot [index: {}, term: {}]",
                self.tag, self.raft_log.committed, sindex, sterm
            );
            let mut to_send = Message::default();
            to_send.set_to(m.get_from());
            to_send.set_msg_type(MessageType::MsgAppendResponse);
            to_send.set_index(self.raft_log.committed);
            self.send(to_send);
        }
    }

    fn restore_raft(&mut self, snap: &Snapshot) -> Option<bool> {
        let meta = snap.get_metadata();
        if self.raft_log.match_term(meta.get_index(), meta.get_term()) {
            info!(
                "{} [commit: {}, lastindex: {}, lastterm: {}] fast-forwarded commit to \
                 snapshot [index: {}, term: {}]",
                self.tag,
                self.raft_log.committed,
                self.raft_log.last_index(),
                self.raft_log.last_term(),
                meta.get_index(),
                meta.get_term()
            );
            self.raft_log.commit_to(meta.get_index());
            return Some(false);
        }

        // Both of learners and voters are empty means the peer is created by ConfChange.
        if self.prs().iter().len() != 0 && !self.is_learner {
            for &id in meta.get_conf_state().get_learners() {
                if id == self.id {
                    error!(
                        "{} can't become learner when restores snapshot [index: {}, term: {}]",
                        self.tag,
                        meta.get_index(),
                        meta.get_term(),
                    );
                    return Some(false);
                }
            }
        }

        info!(
            "{} [commit: {}, lastindex: {}, lastterm: {}] starts to restore snapshot \
             [index: {}, term: {}]",
            self.tag,
            self.raft_log.committed,
            self.raft_log.last_index(),
            self.raft_log.last_term(),
            meta.get_index(),
            meta.get_term()
        );

        let next_idx = self.raft_log.last_index() + 1;
        let mut prs = ProgressSet::restore_snapmeta(meta, next_idx, self.max_inflight);
        prs.get_mut(self.id).unwrap().matched = next_idx - 1;
        if self.is_learner && prs.configuration().voters().contains(&self.id) {
            self.is_learner = false;
        }
        self.prs = Some(prs);
        if meta.get_pending_membership_change_index() > 0 {
            let cs = meta.get_pending_membership_change().clone();
            let mut conf_change = ConfChange::default();
            conf_change.set_change_type(ConfChangeType::BeginMembershipChange);
            conf_change.set_configuration(cs);
            conf_change.set_start_index(meta.get_pending_membership_change_index());
            self.pending_membership_change = Some(conf_change);
        }
        None
    }

    /// Recovers the state machine from a snapshot. It restores the log and the
    /// configuration of state machine.
    pub fn restore(&mut self, snap: Snapshot) -> bool {
        if snap.get_metadata().get_index() < self.raft_log.committed {
            return false;
        }
        if let Some(b) = self.restore_raft(&snap) {
            return b;
        }

        self.raft_log.restore(snap);
        true
    }

    /// Check if there is any pending confchange.
    ///
    /// This method can be false positive.
    #[inline]
    pub fn has_pending_conf(&self) -> bool {
        self.pending_conf_index > self.raft_log.applied || self.pending_membership_change.is_some()
    }

    /// Specifies if the commit should be broadcast.
    pub fn should_bcast_commit(&self) -> bool {
        !self.skip_bcast_commit || self.has_pending_conf()
    }

    /// Indicates whether state machine can be promoted to leader,
    /// which is true when its own id is in progress list.
    pub fn promotable(&self) -> bool {
        self.prs().voter_ids().contains(&self.id)
    }

    /// Propose that the peer group change its active set to a new set.
    ///
    /// > **Note:** This is an experimental feature.
    ///
    /// ```rust
    /// use raft::{Raft, Config, storage::MemStorage, eraftpb::ConfState};
    /// use raft::raw_node::RawNode;
    /// let config = Config {
    ///     id: 1,
    ///     ..Default::default()
    /// };
    /// let store = MemStorage::new_with_conf_state((vec![1], vec![]));
    /// let mut node = RawNode::new(&config, store).unwrap();
    /// let mut raft = node.raft;
    /// raft.become_candidate();
    /// raft.become_leader(); // It must be a leader!
    ///
    /// let mut conf = ConfState::default();
    /// conf.set_nodes(vec![1,2,3]);
    /// conf.set_learners(vec![4]);
    /// if let Err(e) = raft.propose_membership_change(conf) {
    ///     panic!("{}", e);
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// * This Peer is not leader.
    /// * `voters` and `learners` are not mutually exclusive.
    /// * `voters` is empty.
    pub fn propose_membership_change(&mut self, config: impl Into<Configuration>) -> Result<()> {
        if self.state != StateRole::Leader {
            return Err(Error::InvalidState(self.state));
        }
        let config = config.into();
        config.valid()?;
        debug!(
            "Replicating SetNodes with voters ({:?}), learners ({:?}).",
            config.voters(),
            config.learners()
        );
        let destination_index = self.raft_log.last_index() + 1;
        // Prep a configuration change to append.
        let mut conf_change = ConfChange::default();
        conf_change.set_change_type(ConfChangeType::BeginMembershipChange);
        conf_change.set_configuration(config.into());
        conf_change.set_start_index(destination_index);
        let mut data = Vec::with_capacity(ProstMsg::encoded_len(&conf_change));
        conf_change.encode(&mut data).unwrap();
        let mut entry = Entry::default();
        entry.set_entry_type(EntryType::EntryConfChange);
        entry.set_data(data);
        let mut message = Message::default();
        message.set_msg_type(MessageType::MsgPropose);
        message.set_from(self.id);
        message.set_index(destination_index);
        message.set_entries(vec![entry]);
        // `append_entry` sets term, index for us.
        self.step(message)?;
        Ok(())
    }

    /// # Errors
    ///
    /// * `id` is already a voter.
    /// * `id` is already a learner.
    /// * There is a pending membership change. (See `is_in_membership_change()`)
    fn add_voter_or_learner(&mut self, id: u64, learner: bool) -> Result<()> {
        debug!(
            "Adding node (learner: {}) with ID {} to peers.",
            learner, id
        );

        let result = if learner {
            let progress = Progress::new(self.raft_log.last_index() + 1, self.max_inflight);
            self.mut_prs().insert_learner(id, progress)
        } else if self.prs().learner_ids().contains(&id) {
            self.mut_prs().promote_learner(id)
        } else {
            let progress = Progress::new(self.raft_log.last_index() + 1, self.max_inflight);
            self.mut_prs().insert_voter(id, progress)
        };

        if let Err(e) = result {
            error!("{}", e);
            return Err(e);
        }
        if self.id == id {
            self.is_learner = learner
        };
        // When a node is first added/promoted, we should mark it as recently active.
        // Otherwise, check_quorum may cause us to step down if it is invoked
        // before the added node has a chance to commuicate with us.
        self.mut_prs().get_mut(id).unwrap().recent_active = true;
        result
    }

    /// Adds a new node to the cluster.
    ///
    /// # Errors
    ///
    /// * `id` is already a voter.
    /// * `id` is already a learner.
    /// * There is a pending membership change. (See `is_in_membership_change()`)
    pub fn add_node(&mut self, id: u64) -> Result<()> {
        self.add_voter_or_learner(id, false)
    }

    /// Adds a learner node.
    ///
    /// # Errors
    ///
    /// * `id` is already a voter.
    /// * `id` is already a learner.
    /// * There is a pending membership change. (See `is_in_membership_change()`)
    pub fn add_learner(&mut self, id: u64) -> Result<()> {
        self.add_voter_or_learner(id, true)
    }

    /// Removes a node from the raft.
    ///
    /// # Errors
    ///
    /// * `id` is not a voter or learner.
    /// * There is a pending membership change. (See `is_in_membership_change()`)
    pub fn remove_node(&mut self, id: u64) -> Result<()> {
        self.mut_prs().remove(id)?;

        // do not try to commit or abort transferring if there are no nodes in the cluster.
        if self.prs().voter_ids().is_empty() && self.prs().learner_ids().is_empty() {
            return Ok(());
        }

        // The quorum size is now smaller, so see if any pending entries can
        // be committed.
        if self.maybe_commit() {
            self.bcast_append();
        }
        // If the removed node is the lead_transferee, then abort the leadership transferring.
        if self.state == StateRole::Leader && self.lead_transferee == Some(id) {
            self.abort_leader_transfer();
        }

        Ok(())
    }

    /// Updates the progress of the learner or voter.
    pub fn set_progress(&mut self, id: u64, matched: u64, next_idx: u64, is_learner: bool) {
        let mut p = Progress::new(next_idx, self.max_inflight);
        p.matched = matched;
        if is_learner {
            if let Err(e) = self.mut_prs().insert_learner(id, p) {
                panic!("{}", e);
            }
        } else if let Err(e) = self.mut_prs().insert_voter(id, p) {
            panic!("{}", e);
        }
    }

    /// Takes the progress set (destructively turns to `None`).
    pub fn take_prs(&mut self) -> ProgressSet {
        self.prs.take().unwrap()
    }

    /// Sets the progress set.
    pub fn set_prs(&mut self, prs: ProgressSet) {
        self.prs = Some(prs);
    }

    /// Returns a read-only reference to the progress set.
    pub fn prs(&self) -> &ProgressSet {
        self.prs.as_ref().unwrap()
    }

    /// Returns a mutable reference to the progress set.
    pub fn mut_prs(&mut self) -> &mut ProgressSet {
        self.prs.as_mut().unwrap()
    }

    // TODO: revoke pub when there is a better way to test.
    /// For a given hardstate, load the state into self.
    pub fn load_state(&mut self, hs: &HardState) {
        if hs.get_commit() < self.raft_log.committed || hs.get_commit() > self.raft_log.last_index()
        {
            panic!(
                "{} hs.commit {} is out of range [{}, {}]",
                self.tag,
                hs.get_commit(),
                self.raft_log.committed,
                self.raft_log.last_index()
            )
        }
        self.raft_log.committed = hs.get_commit();
        self.term = hs.get_term();
        self.vote = hs.get_vote();
    }

    /// `pass_election_timeout` returns true iff `election_elapsed` is greater
    /// than or equal to the randomized election timeout in
    /// [`election_timeout`, 2 * `election_timeout` - 1].
    pub fn pass_election_timeout(&self) -> bool {
        self.election_elapsed >= self.randomized_election_timeout
    }

    /// Regenerates and stores the election timeout.
    pub fn reset_randomized_election_timeout(&mut self) {
        let prev_timeout = self.randomized_election_timeout;
        let timeout =
            rand::thread_rng().gen_range(self.min_election_timeout, self.max_election_timeout);
        debug!(
            "{} reset election timeout {} -> {} at {}",
            self.tag, prev_timeout, timeout, self.election_elapsed
        );
        self.randomized_election_timeout = timeout;
    }

    // check_quorum_active returns true if the quorum is active from
    // the view of the local raft state machine. Otherwise, it returns
    // false.
    // check_quorum_active also resets all recent_active to false.
    // check_quorum_active can only called by leader.
    fn check_quorum_active(&mut self) -> bool {
        let self_id = self.id;
        self.mut_prs().quorum_recently_active(self_id)
    }

    /// Issues a message to timeout immediately.
    pub fn send_timeout_now(&mut self, to: u64) {
        let msg = new_message(to, MessageType::MsgTimeoutNow, None);
        self.send(msg);
    }

    /// Stops the tranfer of a leader.
    pub fn abort_leader_transfer(&mut self) {
        self.lead_transferee = None;
    }

    /// Determine if the Raft is in a transition state under Joint Consensus.
    pub fn is_in_membership_change(&self) -> bool {
        self.prs().is_in_membership_change()
    }
}
