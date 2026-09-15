use super::Rng;

/// Two keys on one owner: enough for a repeated WATCH of one key and a
/// WATCH of the other after the first changed.
pub type Key = usize;
pub const KEYS: usize = 2;

/// How WATCH remembers what it saw.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Representation {
    /// M6-S03 as written: `{Version(v) | MISSING}` read at WATCH and
    /// compared at EXEC.
    EndpointEquality,
    /// ADR-0116 D4: the owner keeps a bounded registration whose dirty
    /// state every mutation sets synchronously; EXEC asks the owner.
    OwnerRegistration,
}

/// What a `WATCH` of a key the connection already watches does.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RepeatedWatch {
    /// The batch-22 model as written: a fresh registration — a new
    /// observation, a clean dirty bit, eviction and restart forgotten.
    Reregister,
    /// ADR-0116 A5: a connection-local no-op — the first registration,
    /// its dirty state and its fate stand until EXEC/DISCARD/UNWATCH/
    /// RESET/disconnect release every registration together.
    Additive,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    pub representation: Representation,
    pub repeated_watch: RepeatedWatch,
}

impl Rules {
    pub fn chosen() -> Rules {
        Rules {
            representation: Representation::OwnerRegistration,
            repeated_watch: RepeatedWatch::Additive,
        }
    }

    pub fn withdrawn() -> Rules {
        Rules {
            representation: Representation::EndpointEquality,
            repeated_watch: RepeatedWatch::Reregister,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Watch(Key),
    Set(Key),
    Del(Key),
    Expire(Key),
    /// `FLUSHDB`: every present key is deleted.
    Flush,
    /// The registration table evicts this key's entry (capacity).
    Evict(Key),
    /// The owner restarts: registrations and intents are lost.
    Restart,
    /// `UNWATCH`/`DISCARD`/`RESET`/disconnect: the lifecycle reset that
    /// releases every registration the connection holds.
    Unwatch,
    Exec,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Commit,
    Abort,
}

#[derive(Clone, Copy, Debug, Default)]
struct Record {
    present: bool,
    /// u24 record version: bumps per mutation; a recreated record
    /// starts at 0 again (interfaces-m0 record header).
    version: u32,
}

/// The connection's token for one key and the owner's entry behind it.
#[derive(Clone, Copy, Debug)]
struct Registration {
    observed: Option<u32>,
    dirty: bool,
    evicted: bool,
    /// The owner restarted: the token is unknown there.
    lost: bool,
}

/// The sticky-window oracle: the connection is watching from its first
/// `Watch` until `Exec`/`Unwatch`; anything inside that window that a
/// registration cannot vouch for — a mutation of a watched key, an
/// eviction, an owner restart — is a violation no later `Watch` can
/// undo. Deliberately not the per-registration bits the model keeps.
#[derive(Clone, Copy, Debug, Default)]
struct Window {
    watched: [bool; KEYS],
    violated: bool,
}

impl Window {
    fn open(&self) -> bool {
        self.watched.iter().any(|w| *w)
    }
}

/// Run `history` (must end with `Exec`) and return the verdict the
/// rules give and the sticky-window oracle's verdict.
pub fn run(rules: Rules, history: &[Event]) -> (Verdict, Verdict) {
    let mut recs = [Record::default(); KEYS];
    let mut regs: [Option<Registration>; KEYS] = [None; KEYS];
    let mut window = Window::default();
    for ev in history {
        match *ev {
            Event::Watch(k) => {
                if regs[k].is_none() || rules.repeated_watch == RepeatedWatch::Reregister {
                    regs[k] = Some(Registration {
                        observed: recs[k].present.then_some(recs[k].version),
                        dirty: false,
                        evicted: false,
                        lost: false,
                    });
                }
                window.watched[k] = true;
            }
            Event::Set(k) => {
                if recs[k].present {
                    recs[k].version = (recs[k].version + 1) & 0x00FF_FFFF;
                } else {
                    recs[k] = Record { present: true, version: 0 };
                }
                mutate(&mut regs, &mut window, k);
            }
            Event::Del(k) | Event::Expire(k) => {
                if recs[k].present {
                    recs[k] = Record::default();
                    mutate(&mut regs, &mut window, k);
                }
            }
            Event::Flush => {
                let present: Vec<Key> = (0..KEYS).filter(|k| recs[*k].present).collect();
                for k in present {
                    recs[k] = Record::default();
                    mutate(&mut regs, &mut window, k);
                }
            }
            Event::Evict(k) => {
                if let Some(reg) = &mut regs[k] {
                    reg.evicted = true;
                }
                window.violated |= window.watched[k];
            }
            Event::Restart => {
                for reg in regs.iter_mut().flatten() {
                    reg.lost = true;
                }
                window.violated |= window.open();
            }
            Event::Unwatch => {
                regs = [None; KEYS];
                window = Window::default();
            }
            Event::Exec => {
                let oracle = if window.violated { Verdict::Abort } else { Verdict::Commit };
                let clean = |k: usize, reg: &Registration| match rules.representation {
                    Representation::EndpointEquality => {
                        reg.observed == recs[k].present.then_some(recs[k].version)
                    }
                    Representation::OwnerRegistration => !(reg.dirty || reg.evicted || reg.lost),
                };
                let verdict =
                    if regs.iter().enumerate().all(|(k, r)| r.is_none_or(|r| clean(k, &r))) {
                        Verdict::Commit
                    } else {
                        Verdict::Abort
                    };
                return (verdict, oracle);
            }
        }
    }
    panic!("history must end with Exec");
}

/// The owner marks the key's registration dirty synchronously; the
/// window records the violation.
fn mutate(regs: &mut [Option<Registration>; KEYS], window: &mut Window, k: Key) {
    if let Some(reg) = &mut regs[k] {
        reg.dirty = true;
    }
    window.violated |= window.watched[k];
}

/// The F3 history: WATCH an absent key, another client creates and
/// deletes it, EXEC sees `MISSING` again.
pub const ABSENT_PRESENT_ABSENT: [Event; 4] =
    [Event::Watch(0), Event::Set(0), Event::Del(0), Event::Exec];
/// Delete/recreate: the recreated record's initial version equals the
/// observed one.
pub const DELETE_RECREATE: [Event; 5] =
    [Event::Set(0), Event::Watch(0), Event::Del(0), Event::Set(0), Event::Exec];
/// The review's F07 history: a repeated WATCH must not launder the
/// mutation between the two.
pub const REPEATED_WATCH: [Event; 4] =
    [Event::Watch(0), Event::Set(0), Event::Watch(0), Event::Exec];
/// Watching another key after the first watched key changed.
pub const ADDITIONAL_KEY_AFTER_CHANGE: [Event; 4] =
    [Event::Watch(0), Event::Set(0), Event::Watch(1), Event::Exec];
/// A repeated WATCH after an eviction or an owner restart is a no-op:
/// the first token is the one EXEC presents, and it is unknown.
pub const REPEATED_WATCH_AFTER_EVICTION: [Event; 5] =
    [Event::Set(0), Event::Watch(0), Event::Evict(0), Event::Watch(0), Event::Exec];
pub const REPEATED_WATCH_AFTER_RESTART: [Event; 5] =
    [Event::Set(0), Event::Watch(0), Event::Restart, Event::Watch(0), Event::Exec];
/// `UNWATCH` is the real reset: the WATCH after it starts clean.
pub const RESET_THEN_WATCH: [Event; 5] =
    [Event::Watch(0), Event::Set(0), Event::Unwatch, Event::Watch(0), Event::Exec];

/// A seeded random history of `len` events over both keys ending in
/// `Exec`.
pub fn random_history(seed: u64, len: usize) -> Vec<Event> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(len + 1);
    for _ in 0..len {
        let k = rng.below(KEYS);
        out.push(match rng.below(11) {
            0 | 1 => Event::Watch(k),
            2 | 3 => Event::Set(k),
            4 => Event::Del(k),
            5 => Event::Expire(k),
            6 => Event::Flush,
            7 => Event::Evict(k),
            8 => Event::Restart,
            9 => Event::Unwatch,
            _ => Event::Set(k),
        });
    }
    out.push(Event::Exec);
    out
}

/// One line of `seeds/watch-redis-oracle.txt`: the history and Redis's
/// own verdict (`scripts/txmodel-watch-redis-oracle.py`).
pub fn parse_fixture_line(line: &str) -> Option<(Vec<Event>, Verdict)> {
    let (history, verdict) = line.split_once('\t')?;
    let verdict = match verdict.trim() {
        "abort" => Verdict::Abort,
        "commit" => Verdict::Commit,
        _ => return None,
    };
    let key = |tok: &str| tok[1..].parse::<usize>().ok().filter(|k| *k < KEYS);
    let events = history
        .split_whitespace()
        .map(|tok| match tok.as_bytes()[0] {
            b'W' => key(tok).map(Event::Watch),
            b'S' => key(tok).map(Event::Set),
            b'D' => key(tok).map(Event::Del),
            b'F' => Some(Event::Flush),
            b'U' => Some(Event::Unwatch),
            b'X' => Some(Event::Exec),
            _ => None,
        })
        .collect::<Option<Vec<Event>>>()?;
    Some((events, verdict))
}

/// The fixture, compiled in so the test needs no working directory.
pub const REDIS_FIXTURE: &str = include_str!("../../seeds/watch-redis-oracle.txt");
