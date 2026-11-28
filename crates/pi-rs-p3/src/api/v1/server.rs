//! Chord's sync layer for the backup plugin's server facet: targets and the relay.

pub use pi_rs_chord::sync::{
    AsyncRemote, BatchInfo, Body, FileSystemSyncRemote, PutBatchResult, RemoteListing,
    SYNC_PROTOCOL, SyncFuture,
};
