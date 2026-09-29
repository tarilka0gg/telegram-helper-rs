//! MTProto session kept in memory and persisted as an encrypted JSON blob in
//! `telegram_sessions.session_string_enc` (no plaintext session file on disk).

use std::{
    convert::Infallible,
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex},
};

use grammers_session::{
    types::{ChannelState, DcOption, PeerId, PeerInfo, UpdateState, UpdatesState},
    BoxFuture, Session, SessionData,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Snapshot {
    home_dc: i32,
    dc_options: Vec<DcOption>,
    peers: Vec<PeerInfo>,
    updates_state: UpdatesState,
}

pub struct DbSession {
    data: Mutex<SessionData>,
    dirty: AtomicBool,
}

impl Default for DbSession {
    fn default() -> Self {
        Self { data: Mutex::new(SessionData::default()), dirty: AtomicBool::new(false) }
    }
}

impl DbSession {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn from_json(json: &str) -> anyhow::Result<Arc<Self>> {
        let s: Snapshot = serde_json::from_str(json)?;
        let data = SessionData {
            home_dc: s.home_dc,
            dc_options: s.dc_options.into_iter().map(|d| (d.id, d)).collect(),
            peer_infos: s.peers.into_iter().map(|p| (p.id(), p)).collect(),
            updates_state: s.updates_state,
        };
        Ok(Arc::new(Self { data: Mutex::new(data), dirty: AtomicBool::new(false) }))
    }

    pub fn to_json(&self) -> String {
        let d = self.data.lock().unwrap();
        let snap = Snapshot {
            home_dc: d.home_dc,
            dc_options: d.dc_options.values().cloned().collect(),
            peers: d.peer_infos.values().cloned().collect(),
            updates_state: d.updates_state.clone(),
        };
        serde_json::to_string(&snap).expect("session is serializable")
    }

    /// True (once) if anything changed since the last call — the persister's cue to write.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    fn touch(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Has an auth key for the home DC, i.e. a login happened.
    pub fn has_auth_key(&self) -> bool {
        let d = self.data.lock().unwrap();
        d.dc_options.get(&d.home_dc).is_some_and(|o| o.auth_key.is_some())
    }
}

impl Session for DbSession {
    type Error = Infallible;

    fn home_dc_id(&self) -> Result<i32, Infallible> {
        Ok(self.data.lock().unwrap().home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), Infallible>> {
        Box::pin(async move {
            self.data.lock().unwrap().home_dc = dc_id;
            self.touch();
            Ok(())
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, Infallible> {
        Ok(self.data.lock().unwrap().dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), Infallible>> {
        let o = dc_option.clone();
        Box::pin(async move {
            self.data.lock().unwrap().dc_options.insert(o.id, o);
            self.touch();
            Ok(())
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, Infallible>> {
        Box::pin(async move { Ok(self.data.lock().unwrap().peer_infos.get(&peer).cloned()) })
    }

    fn cache_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), Infallible>> {
        let peer = peer.clone();
        Box::pin(async move {
            let mut d = self.data.lock().unwrap();
            let changed = match d.peer_infos.get_mut(&peer.id()) {
                Some(existing) => existing.extend_info(&peer),
                None => {
                    d.peer_infos.insert(peer.id(), peer);
                    true
                }
            };
            drop(d);
            if changed {
                self.touch();
            }
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, Infallible>> {
        Box::pin(async move { Ok(self.data.lock().unwrap().updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), Infallible>> {
        Box::pin(async move {
            let mut d = self.data.lock().unwrap();
            match update {
                UpdateState::All(s) => d.updates_state = s,
                UpdateState::Primary { pts, date, seq } => {
                    d.updates_state.pts = pts;
                    d.updates_state.date = date;
                    d.updates_state.seq = seq;
                }
                UpdateState::Secondary { qts } => d.updates_state.qts = qts,
                UpdateState::Channel { id, pts } => {
                    d.updates_state.channels.retain(|c| c.id != id);
                    d.updates_state.channels.push(ChannelState { id, pts });
                }
            }
            drop(d);
            self.touch();
            Ok(())
        })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_roundtrip_and_dirty_flag() {
        let s = DbSession::new();
        assert!(!s.take_dirty());
        s.set_home_dc_id(4).await.unwrap();
        s.set_update_state(UpdateState::Primary { pts: 10, date: 20, seq: 30 }).await.unwrap();
        s.set_update_state(UpdateState::Channel { id: 7, pts: 3 }).await.unwrap();
        assert!(s.take_dirty() && !s.take_dirty());

        let copy = DbSession::from_json(&s.to_json()).unwrap();
        assert_eq!(copy.home_dc_id().unwrap(), 4);
        let st = copy.updates_state().await.unwrap();
        assert_eq!((st.pts, st.date, st.seq), (10, 20, 30));
        assert_eq!(st.channels.len(), 1);
        assert!(!copy.take_dirty());
        assert!(DbSession::from_json("{broken").is_err());
    }
}
