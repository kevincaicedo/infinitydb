//! Per-cell client registry (M1-S03): backs `CLIENT ID/GETNAME/SETNAME/
//! LIST/INFO/KILL`. Cell-local behind `Rc<NodeInfo>` — connections belong to
//! exactly one cell (L1), so a cell's CLIENT LIST shows its own connections;
//! the control thread aggregates across cells when it grows that surface.
//!
//! Kill is a flag handshake: `CLIENT KILL` marks the target, the plane
//! sweeps marks in MAINTAIN and closes at the owner — command execution
//! never touches another connection's I/O state.

use std::collections::BTreeMap;

/// One connection's introspection record.
#[derive(Clone, Debug, Default)]
pub struct ClientInfo {
    pub name: Vec<u8>,
    /// Peer address when the transport knows it (`0.0.0.0:0` placeholder
    /// otherwise — recorded deviation until the plane captures peernames).
    pub addr: String,
    /// Registration time, injected-clock milliseconds.
    pub created_ms: u64,
    /// Negotiated RESP version (2/3).
    pub resp: u8,
    /// Selected db, mirrored from the connection at SELECT / `INF.NS USE`
    /// (F-L15-09: tracked state is reported, never a placeholder zero).
    pub db: u16,
    /// Channel / pattern subscription counts, mirrored at every
    /// (un)subscribe.
    pub sub: u32,
    pub psub: u32,
    pub kill_requested: bool,
    /// The plane's connection key (packed), for the kill sweep; `None`
    /// for callers the plane never saw (`ensure`).
    pub key: Option<u64>,
}

/// BTreeMap keyed by client id: CLIENT LIST output is id-ordered, and the
/// surface is cold (admin path), so density beats hashing here.
#[derive(Debug, Default)]
pub struct ClientRegistry {
    clients: BTreeMap<u64, ClientInfo>,
}

impl ClientRegistry {
    pub fn register(&mut self, id: u64, key: u64, addr: String, created_ms: u64) {
        self.clients.insert(
            id,
            ClientInfo {
                name: Vec::new(),
                addr,
                created_ms,
                resp: 2,
                db: 0,
                sub: 0,
                psub: 0,
                kill_requested: false,
                key: Some(key),
            },
        );
    }

    pub fn unregister(&mut self, id: u64) {
        self.clients.remove(&id);
    }

    /// Lazily self-registers callers the plane never saw (the embedded /
    /// compat-candidate path).
    pub fn ensure(&mut self, id: u64, created_ms: u64) -> &mut ClientInfo {
        self.clients.entry(id).or_insert_with(|| ClientInfo {
            name: Vec::new(),
            addr: "0.0.0.0:0".to_string(),
            created_ms,
            resp: 2,
            db: 0,
            sub: 0,
            psub: 0,
            kill_requested: false,
            key: None,
        })
    }

    pub fn get(&self, id: u64) -> Option<&ClientInfo> {
        self.clients.get(&id)
    }

    /// Mirrors the connection's db and subscription counts (F-L15-09);
    /// registers a caller the plane never saw, like [`ClientRegistry::ensure`].
    pub fn note_conn_state(&mut self, id: u64, created_ms: u64, db: u16, sub: u32, psub: u32) {
        let c = self.ensure(id, created_ms);
        c.db = db;
        c.sub = sub;
        c.psub = psub;
    }

    pub fn set_resp(&mut self, id: u64, resp: u8) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.resp = resp;
        }
    }

    /// Marks `id` for the plane's kill sweep. False when unknown.
    pub fn request_kill(&mut self, id: u64) -> bool {
        match self.clients.get_mut(&id) {
            Some(c) => {
                c.kill_requested = true;
                true
            }
            None => false,
        }
    }

    /// The plane key registered for `id` (the kill sweep's map back).
    pub fn conn_key(&self, id: u64) -> Option<u64> {
        self.clients.get(&id).and_then(|c| c.key)
    }

    /// Drains kill marks (plane MAINTAIN sweep).
    pub fn take_kill_requests(&mut self) -> Vec<u64> {
        self.clients
            .iter_mut()
            .filter(|(_, c)| c.kill_requested)
            .map(|(id, c)| {
                c.kill_requested = false;
                *id
            })
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, &ClientInfo)> {
        self.clients.iter().map(|(id, c)| (*id, c))
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }
}

/// Redis `CLIENT SETNAME` charset rule: printable ASCII, no spaces.
pub fn valid_client_name(name: &[u8]) -> bool {
    name.iter().all(|&b| (b'!'..=b'~').contains(&b))
}

/// One `CLIENT LIST`/`CLIENT INFO` line (no trailing newline) — Redis field
/// vocabulary. `db`/`sub`/`psub`/`resp`/`name`/`age` are tracked; the
/// zeros (`idle`, `cmd`, `tot-*`, buffer gauges) are stats this build
/// does not track — the compat matrix's `CLIENT` deviation names them.
pub fn format_client_line(id: u64, info: &ClientInfo, age_secs: u64, last_cmd: &str) -> String {
    format!(
        "id={id} addr={addr} laddr=0.0.0.0:0 fd=-1 name={name} age={age_secs} idle=0 flags=N \
         db={db} sub={sub} psub={psub} ssub=0 multi=-1 watch=0 qbuf=0 qbuf-free=0 argv-mem=0 \
         multi-mem=0 tot-net-in=0 tot-net-out=0 rbs=1024 rbp=0 obl=0 oll=0 omem=0 tot-mem=0 \
         events=r cmd={last_cmd} user=default redir=-1 resp={resp} lib-name= lib-ver= tot-cmds=0",
        addr = info.addr,
        name = String::from_utf8_lossy(&info.name),
        db = info.db,
        sub = info.sub,
        psub = info.psub,
        resp = info.resp,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_kill_sweep_roundtrip() {
        let mut reg = ClientRegistry::default();
        reg.register(7, 70, "1.2.3.4:5".into(), 1000);
        reg.register(9, 90, "1.2.3.4:6".into(), 2000);
        assert!(reg.request_kill(9));
        assert!(!reg.request_kill(404));
        assert_eq!(reg.take_kill_requests(), vec![9]);
        assert!(reg.take_kill_requests().is_empty(), "marks drain");
        reg.unregister(7);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn name_charset_matches_redis_rule() {
        assert!(valid_client_name(b"worker-1"));
        assert!(valid_client_name(b""));
        assert!(!valid_client_name(b"has space"));
        assert!(!valid_client_name(b"new\nline"));
        assert!(!valid_client_name(&[0xFF]));
    }
}
