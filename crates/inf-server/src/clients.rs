//! Per-cell client registry (M1-S03): backs `CLIENT ID/GETNAME/SETNAME/
//! LIST/INFO/KILL`. Cell-local behind `Rc<NodeInfo>` — connections belong to
//! exactly one cell (L1), so a cell's CLIENT LIST shows its own connections;
//! the control thread aggregates across cells when it grows that surface.
//!
//! Kill is a flag handshake: `CLIENT KILL` marks the target, the plane
//! sweeps marks in MAINTAIN and closes at the owner — command execution
//! never touches another connection's I/O state.

use core::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

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
    /// for a client no plane admitted (an embedded or candidate context).
    pub key: Option<u64>,
}

/// BTreeMap keyed by client id: CLIENT LIST output is id-ordered, and the
/// surface is cold (admin path), so density beats hashing here.
#[derive(Debug, Default)]
pub struct ClientRegistry {
    clients: BTreeMap<u64, ClientInfo>,
}

impl ClientRegistry {
    /// The one mint of a [`ClientAdmission`]: a new registry, shared, and
    /// the admission over it. No function mints an admission for a
    /// registry that exists, so an admission inserts only into the
    /// registry minted with it. A client row is inserted only at
    /// admission, so no command run after `Closed` puts its client back
    /// into `CLIENT LIST`.
    pub fn with_admission() -> (Rc<RefCell<ClientRegistry>>, ClientAdmission) {
        let registry = Rc::new(RefCell::new(ClientRegistry::default()));
        let admission = ClientAdmission { registry: Rc::clone(&registry) };
        (registry, admission)
    }

    /// The registry's one inserting function, reached through
    /// [`ClientAdmission::register`] alone.
    fn register(&mut self, id: u64, key: Option<u64>, addr: String, created_ms: u64) {
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
                key,
            },
        );
    }

    /// Canary: an inserting update, the one a command's context would need
    /// to put a closed client back.
    #[cfg(inf_canary_command_registers_client)]
    fn ensure(&mut self, id: u64) {
        self.clients.entry(id).or_default();
    }

    pub fn unregister(&mut self, id: u64) {
        self.clients.remove(&id);
    }

    pub fn get(&self, id: u64) -> Option<&ClientInfo> {
        self.clients.get(&id)
    }

    /// Mirrors the connection's db and subscription counts (F-L15-09);
    /// nothing for an id with no row: an update, never an insert.
    pub fn note_conn_state(&mut self, id: u64, db: u16, sub: u32, psub: u32) {
        #[cfg(inf_canary_command_registers_client)]
        self.ensure(id);
        if let Some(c) = self.clients.get_mut(&id) {
            c.db = db;
            c.sub = sub;
            c.psub = psub;
        }
    }

    /// `CLIENT SETNAME`'s write; nothing for an id with no row.
    pub fn set_name(&mut self, id: u64, name: &[u8]) {
        #[cfg(inf_canary_command_registers_client)]
        self.ensure(id);
        if let Some(c) = self.clients.get_mut(&id) {
            c.name = name.to_vec();
        }
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

/// The right to insert a client row: held by the plane that
/// accepts connections, never by a command's context, which reaches the
/// registry only through its node and can update a row, not insert one.
#[derive(Debug)]
pub struct ClientAdmission {
    registry: Rc<RefCell<ClientRegistry>>,
}

impl ClientAdmission {
    /// Registers an admitted client: `key` is the plane's packed
    /// connection key, for the kill sweep, or `None` for a context no
    /// plane admitted.
    pub fn register(&self, id: u64, key: Option<u64>, addr: String, created_ms: u64) {
        self.registry.borrow_mut().register(id, key, addr, created_ms);
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
        let (registry, admission) = ClientRegistry::with_admission();
        admission.register(7, Some(70), "1.2.3.4:5".into(), 1000);
        admission.register(9, Some(90), "1.2.3.4:6".into(), 2000);
        let mut reg = registry.borrow_mut();
        assert!(reg.request_kill(9));
        assert!(!reg.request_kill(404));
        assert_eq!(reg.take_kill_requests(), vec![9]);
        assert!(reg.take_kill_requests().is_empty(), "marks drain");
        reg.unregister(7);
        assert_eq!(reg.len(), 1);
    }

    /// A row is inserted only through the admission: an update of an id
    /// with no row (a client whose slot left `CLIENT LIST`) inserts none.
    #[test]
    fn an_update_of_an_unknown_id_inserts_no_row() {
        let (registry, admission) = ClientRegistry::with_admission();
        let mut reg = registry.borrow_mut();
        reg.set_name(5, b"ghost");
        reg.note_conn_state(5, 1, 1, 0);
        reg.set_resp(5, 3);
        assert!(reg.is_empty(), "an update inserted a row");
        drop(reg);
        admission.register(5, None, "0.0.0.0:0".into(), 0);
        registry.borrow_mut().set_name(5, b"named");
        assert_eq!(registry.borrow().get(5).map(|c| c.name.clone()), Some(b"named".to_vec()));
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
