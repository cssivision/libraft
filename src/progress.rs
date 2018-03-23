/// ## Progress
/// 
/// Progress represents a follower’s progress in the view of the leader. Leader maintains 
/// progresses of all followers, and sends `replication message` to the follower based on 
/// its progress. `replication message` is a `msgApp` with log entries.
/// 
/// A progress has two attribute: `match` and `next`. `match` is the index of the highest 
/// known matched entry. If leader knows nothing about follower’s replication status, `match`
///  is set to zero. `next` is the index of the first entry that will be replicated to the 
/// follower. Leader puts entries from `next` to its latest one in next `replication message`.

/// A progress is in one of the three state: `probe`, `replicate`, `snapshot`. 

/// ```
///                             +--------------------------------------------------------+          
///                             |                  send snapshot                         |          
///                             |                                                        |          
///                   +---------+----------+                                  +----------v---------+
///               +--->       probe        |                                  |      snapshot      |
///               |   |  max inflight = 1  <----------------------------------+  max inflight = 0  |
///               |   +---------+----------+                                  +--------------------+
///               |             |            1. snapshot success                                    
///               |             |               (next=snapshot.index + 1)                           
///               |             |            2. snapshot failure                                    
///               |             |               (no change)                                         
///               |             |            3. receives msgAppResp(rej=false&&index>lastsnap.index)
///               |             |               (match=m.index,next=match+1)                        
/// receives msgAppResp(rej=true)                                                                   
/// (next=match+1)|             |                                                                   
///               |             |                                                                   
///               |             |                                                                   
///               |             |   receives msgAppResp(rej=false&&index>match)                     
///               |             |   (match=m.index,next=match+1)                                    
///               |             |                                                                   
///               |             |                                                                   
///               |             |                                                                   
///               |   +---------v----------+                                                        
///               |   |     replicate      |                                                        
///               +---+  max inflight = n  |                                                        
///                   +--------------------+                                                        
/// ```

/// When the progress of a follower is in `probe` state, leader sends at most one `replication
/// message` per heartbeat interval. The leader sends `replication message` slowly and probing
/// the actual progress of the follower. A `msgHeartbeatResp` or a `msgAppResp` with reject might 
/// trigger the sending of the next `replication message`.

/// When the progress of a follower is in `replicate` state, leader sends `replication message`, 
/// then optimistically increases `next` to the latest entry sent. This is an optimized state for 
/// fast replicating log entries to the follower.

/// When the progress of a follower is in `snapshot` state, leader stops sending any `replication message`.

/// A newly elected leader sets the progresses of all the followers to `probe` state with `match` = 0 
/// and `next` = last index. The leader slowly (at most once per heartbeat) sends `replication message` 
/// to the follower and probes its progress.

/// A progress changes to `replicate` when the follower replies with a non-rejection `msgAppResp`, 
/// which implies that it has matched the index sent. At this point, leader starts to stream log 
/// entries to the follower fast. The progress will fall back to `probe` when the follower replies 
/// a rejection `msgAppResp` or the link layer reports the follower is unreachable. We aggressively 
/// reset `next` to `match`+1 since if we receive any `msgAppResp` soon, both `match` and `next` 
/// will increase directly to the `index` in `msgAppResp`. (We might end up with sending some 
/// duplicate entries when aggressively reset `next` too low.  see open question)

/// A progress changes from `probe` to `snapshot` when the follower falls very far behind and requires 
/// a snapshot. After sending `msgSnap`, the leader waits until the success, failure or abortion of the 
/// previous snapshot sent. The progress will go back to `probe` after the sending result is applied.

/// ### Flow Control

/// 1. limit the max size of message sent per message. Max should be configurable.
/// Lower the cost at probing state as we limit the size per message; lower the penalty when aggressively 
/// decreased to a too low `next`

/// 2. limit the # of in flight messages < N when in `replicate` state. N should be configurable. Most 
/// implementation will have a sending buffer on top of its actual network transport layer (not blocking r
/// aft node). We want to make sure raft does not overflow that buffer, which can cause message dropping 
/// and triggering a bunch of unnecessary resending repeatedly. 


use std::cmp;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ProgressState {
    Probe,
    Replicate,
    Snapshot,
}

impl Default for ProgressState {
    fn default() -> ProgressState {
        ProgressState::Probe
    }
}

#[derive(Debug, Default)]
pub struct Progress {
    pub matched: u64,
    pub next: u64,
    pub state: ProgressState,
    pub paused: bool,
    pub pending_snapshot: u64,
    pub recent_active: bool,
    pub ins: Inflights,
    pub is_lenarner: bool,
}

impl Progress {
    pub fn reset_state(&mut self, state: ProgressState) {
        self.paused = false;
        self.pending_snapshot = 0;
        self.state = state;
        self.ins.reset();
    }

    pub fn become_probe(&mut self) {
        // If the original state is ProgressState::Snapshot, progress knows that
	    // the pending snapshot has been sent to this peer successfully, then
	    // probes from pendingSnapshot + 1.
        // otherwise original state should be ProgressState::Replicate, this means 
        // follower reject leader's sending replicate msg request.
        if self.state == ProgressState::Snapshot {
            let pending_snapshot = self.pending_snapshot;
            self.reset_state(ProgressState::Probe);
            self.next = cmp::max(self.matched + 1, pending_snapshot + 1);
        } else {
            self.reset_state(ProgressState::Probe);
            self.next = self.matched + 1;
        }
    }

    pub fn become_replicate(&mut self) {
        // Original state must be ProgressState::Probe, and send msg successfully, 
        // matchd should be matchd = m.index, next = matched + 1
        self.reset_state(ProgressState::Replicate);
        self.next = self.matched + 1;
    }

    pub fn become_snapshot(&mut self, index: u64) {
        // Original state must be ProgressState::Snapshot, after sending snapshot to follower
        // pending_snapshot = index.
        self.reset_state(ProgressState::Snapshot);
        self.pending_snapshot = index;
    }

    fn resume(&mut self) {
        self.paused = false;
    }

    fn pause(&mut self) {
        self.paused = true;
    }

    pub fn maybe_update(&mut self, n: u64) -> bool {
        let mut updated = false;
        if self.matched < n {
            self.matched = n;
            updated = true;
            self.resume();
        }

        if self.next < n+1 {
            self.next = n + 1;
        }

        return updated;
    }

    // when the progress of a follower is in `replicate` state, leader sends 
    // `replication message`, then optimistically increases `next` to the latest entry sent.
    pub fn optimistic_update(&mut self, n: u64) {
        self.next = n + 1;
    }

    // IsPaused returns whether sending log entries to this node has been
    // paused. A node may be paused because it has rejected recent
    // MsgApps, is currently waiting for a snapshot, or has reached the
    // MaxInflightMsgs limit.
    pub fn is_paused(&self) -> bool {
        match self.state {
            ProgressState::Probe => self.paused,
            ProgressState::Replicate => self.ins.full(),

            // When the progress of a follower is in `snapshot` state, 
            // leader stops sending any `replication message`.
            ProgressState::Snapshot => true,
        }
    }

    pub fn snapshot_failure(&mut self) {
        self.pending_snapshot = 0;
    }

    // needSnapshotAbort returns true if snapshot progress's Match
    // is equal or higher than the pendingSnapshot.
    pub fn need_snapshot_failure(&mut self) -> bool {
        self.state == ProgressState::Snapshot && self.matched >= self.pending_snapshot
    }
}

#[derive(Debug, Default)]
pub struct Inflights {
    // the starting index in the buffer
    pub start: usize,
    // number of inflights in the buffer
    pub count: usize,
    // the size of the buffer
    pub size: usize,
    // buffer contains the index of the last entry
	// inside one message.
    pub buffer: Vec<u64>,
}

impl Inflights {
    fn reset(&mut self) {
        self.start = 0;
        self.count = 0;
    }

    fn full(&self) -> bool {
        self.count == self.size
    }
}