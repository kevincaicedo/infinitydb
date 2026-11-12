//! Starting `infinityd` from a test: the one way, through the box's node lock.
//!
//! io_uring charges a node's registered buffers, and on some kernels its
//! rings, to one locked-memory budget per user, checked against the charging
//! process's own `RLIMIT_MEMLOCK`. A test that boots under a small limit
//! (`memlock_boot`, 8 MiB) is refused whenever another process of the user
//! holds that budget: a node another test started, in this lane or another.
//!
//! The rule: a test starts `infinityd` only through this module.
//! [`NodeBinary`] names the binary and builds the only command that runs it.
//! Every start of that command takes the box's node lock shared and hands it
//! to the child as its standard input, so the node holds the lock from
//! before its first ring to its exit, whatever the test does; a [`Node`]
//! kills and reaps its child on drop. A test whose verdict reads the budget
//! takes the lock exclusive ([`NodeLock::exclusive`]) and starts its own
//! nodes under the lease ([`NodeCommand::spawn_under`]); a process never
//! holds the lease beside a live shared node of its own.
//!
//! The lock is two `flock(2)` files in a per-user directory,
//! `$XDG_RUNTIME_DIR/infinity-locked-memory/`, else
//! `<temp dir>/infinity-locked-memory-<uid>/`, made 0700 and refused when
//! another user owns it: `nodes`, which every live node holds shared and the
//! writer exclusive, and `turnstile`, which the writer holds exclusive while
//! it drains, so a thread's first start waits behind the writer while a
//! thread that already holds a node passes. Every wait is a polled try under
//! a deadline: a lock another process holds never blocks a start forever.
//!
//! The writer waits for the longest node-holding span of one test it
//! overlaps, so a lane that runs beside another runs at most 10 test threads.
//!
//! The module is included once per test binary: each `bins/infinityd` test
//! target names it once by `#[path]`, and compat's tests reach it through
//! `compat::harness`, never by a `#[path]` of their own. Its in-process
//! state (the box-wide lock's counts, the per-thread live counts, the lease
//! flag) exists once per copy: through a second copy a nested start would
//! wait at the turnstile while its thread holds a node, until the writer's
//! deadline, and the in-process refusals would not see across the copies.
//!
//! # The ways out the types refuse
//!
//! Each block below writes one way the binary's path, a command or a node's
//! child could leave the module, and must not compile; its fence names the
//! error the refusal raises, which rustdoc checks on nightly. The `no_run`
//! block after them is their twin: one function a block, with that block's
//! names and the allowed form of its expression, so a block that fails for
//! another reason (a renamed method) leaves the twin red. No block runs: each
//! is a function no code calls, so none starts a node or takes a lock.
//!
//! The binary's path enters no `Command`: it is no program, has no `Display`,
//! no `Deref` and no accessor.
//!
//! ```compile_fail,E0277
//! use compat::harness::node::NodeBinary;
//! use std::process::Command;
//! fn way_out(binary: NodeBinary) {
//!     let _ = Command::new(binary);
//! }
//! ```
//!
//! ```compile_fail,E0277
//! use compat::harness::node::NodeBinary;
//! fn way_out(binary: NodeBinary) -> String {
//!     format!("{}", binary)
//! }
//! ```
//!
//! ```compile_fail,E0614
//! use compat::harness::node::NodeBinary;
//! use std::process::Command;
//! fn way_out(binary: NodeBinary) {
//!     let _ = Command::new(&*binary);
//! }
//! ```
//!
//! ```compile_fail,E0599
//! use compat::harness::node::NodeBinary;
//! fn way_out(binary: NodeBinary) {
//!     let _ = binary.path();
//! }
//! ```
//!
//! A scope runs its own program only: the box-wide lock starts no stand-in,
//! a box-wide lease no stand-in, a private lease not the binary.
//!
//! ```compile_fail,E0599
//! use compat::harness::node::NodeLock;
//! fn way_out() {
//!     let _ = NodeLock::box_wide().stand_in();
//! }
//! ```
//!
//! ```compile_fail,E0308
//! use compat::harness::node::{BoxWide, ExclusiveLease, NodeLock, Private};
//! fn way_out(private: NodeLock<Private>, lease: &ExclusiveLease<BoxWide>) {
//!     let _ = private.stand_in().spawn_under(lease);
//! }
//! ```
//!
//! ```compile_fail,E0308
//! use compat::harness::node::{ExclusiveLease, NodeBinary, Private};
//! fn way_out(binary: NodeBinary, lease: &ExclusiveLease<Private>) {
//!     let _ = binary.command().spawn_under(lease);
//! }
//! ```
//!
//! A command becomes no `Command`, takes no standard input (its standard
//! input is the lock) and is built only by the module.
//!
//! ```compile_fail,E0277
//! use compat::harness::node::NodeBinary;
//! use std::process::Command;
//! fn way_out(binary: NodeBinary) {
//!     let _: Command = binary.command().into();
//! }
//! ```
//!
//! ```compile_fail,E0614
//! use compat::harness::node::NodeBinary;
//! fn way_out(binary: NodeBinary) {
//!     let command = binary.command();
//!     let _ = &*command;
//! }
//! ```
//!
//! ```compile_fail,E0599
//! use compat::harness::node::{BoxWide, NodeCommand};
//! use std::process::Stdio;
//! fn way_out(command: NodeCommand<BoxWide>) {
//!     let _ = command.stdin(Stdio::null());
//! }
//! ```
//!
//! ```compile_fail,E0451
//! use compat::harness::node::{BoxWide, NodeCommand, NodeLock};
//! use std::path::PathBuf;
//! fn way_out(lock: NodeLock<BoxWide>, program: PathBuf) -> NodeCommand<BoxWide> {
//!     NodeCommand {
//!         lock,
//!         program,
//!         args: Vec::new(),
//!         envs: Vec::new(),
//!         stdout: None,
//!         stderr: None,
//!         ulimit: None,
//!     }
//! }
//! ```
//!
//! ```compile_fail,E0599
//! use compat::harness::node::{NodeCommand, Private};
//! use std::path::PathBuf;
//! fn way_out(program: PathBuf) -> NodeCommand<Private> {
//!     NodeCommand::<Private>::new(program)
//! }
//! ```
//!
//! A node's handle gives no way to its child, which only its drop ends.
//!
//! ```compile_fail,E0614
//! use compat::harness::node::Node;
//! fn way_out(node: Node) {
//!     let _ = &*node;
//! }
//! ```
//!
//! ```compile_fail,E0614
//! use compat::harness::node::LeasedNode;
//! fn way_out(leased: LeasedNode<'_>) {
//!     let _ = &*leased;
//! }
//! ```
//!
//! The twin: each block's names and signature, its expression in the allowed
//! form; together they call every method of a command.
//!
//! ```no_run
//! mod as_program {
//!     use compat::harness::node::NodeBinary;
//!     use std::process::Command;
//!     pub fn allowed(binary: NodeBinary) {
//!         let _ = binary.command().arg("--help").output();
//!     }
//! }
//! mod as_display {
//!     use compat::harness::node::NodeBinary;
//!     pub fn allowed(binary: NodeBinary) -> String {
//!         format!("{binary:?}")
//!     }
//! }
//! mod as_deref {
//!     use compat::harness::node::NodeBinary;
//!     use std::process::Command;
//!     pub fn allowed(binary: NodeBinary) {
//!         let _ = binary.command().args(["--cells", "1"]).spawn();
//!     }
//! }
//! mod as_accessor {
//!     use compat::harness::node::NodeBinary;
//!     pub fn allowed(binary: NodeBinary) {
//!         let _ = binary.command();
//!     }
//! }
//! mod box_wide_stand_in {
//!     use compat::harness::node::{NodeLock, Waits};
//!     use std::path::Path;
//!     pub fn allowed(dir: &Path) {
//!         let _ = NodeLock::at(dir, Waits::BOX_WIDE).stand_in();
//!     }
//! }
//! mod stand_in_under_box_wide_lease {
//!     use compat::harness::node::{BoxWide, ExclusiveLease, NodeLock, Private};
//!     pub fn allowed(private: NodeLock<Private>, lease: &ExclusiveLease<Private>) {
//!         let _ = private.stand_in().spawn_under(lease);
//!     }
//! }
//! mod binary_under_private_lease {
//!     use compat::harness::node::{BoxWide, ExclusiveLease, NodeBinary, Private};
//!     pub fn allowed(binary: NodeBinary, lease: &ExclusiveLease<BoxWide>) {
//!         let _ = binary.command().spawn_under(lease);
//!     }
//! }
//! mod command_into {
//!     use compat::harness::node::NodeBinary;
//!     use std::process::Command;
//!     pub fn allowed(binary: NodeBinary) {
//!         let _ = binary.command().env("INF_TWIN", "1").output();
//!     }
//! }
//! mod command_deref {
//!     use compat::harness::node::{NodeBinary, Ulimit};
//!     pub fn allowed(binary: NodeBinary) {
//!         let command = binary.command();
//!         let _ = command.ulimit(Ulimit::LockedKib(64));
//!     }
//! }
//! mod command_stdin {
//!     use compat::harness::node::{BoxWide, NodeCommand};
//!     use std::process::Stdio;
//!     pub fn allowed(command: NodeCommand<BoxWide>) {
//!         let _ = command.stdout(Stdio::null()).stderr(Stdio::null());
//!     }
//! }
//! mod command_literal {
//!     use compat::harness::node::{BoxWide, NodeBinary, NodeCommand, NodeLock};
//!     use std::path::PathBuf;
//!     pub fn allowed(
//!         lock: NodeLock<BoxWide>,
//!         program: PathBuf,
//!         binary: NodeBinary,
//!     ) -> NodeCommand<BoxWide> {
//!         let _ = (lock.dir(), program);
//!         binary.command()
//!     }
//! }
//! mod command_new {
//!     use compat::harness::node::{NodeCommand, NodeLock, Private};
//!     use std::path::PathBuf;
//!     pub fn allowed(program: PathBuf, lock: NodeLock<Private>) -> NodeCommand<Private> {
//!         let _ = program;
//!         lock.stand_in()
//!     }
//! }
//! mod node_deref {
//!     use compat::harness::node::Node;
//!     pub fn allowed(node: Node) -> u32 {
//!         node.id()
//!     }
//! }
//! mod leased_deref {
//!     use compat::harness::node::LeasedNode;
//!     pub fn allowed(leased: LeasedNode<'_>) -> u32 {
//!         leased.id()
//!     }
//! }
//! ```
#![allow(dead_code)]
#![allow(clippy::disallowed_types, reason = "test-only: lock files and a uid probe")]

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io;
use std::marker::PhantomData;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// A start's wait for both locks, in seconds: past it the start returns
/// [`NodeError::TimedOut`], naming the lock. A hang bound: the legitimate
/// wait is one writer's hold, under a minute.
pub const SHARED_WAIT_S: u64 = 300;
/// The writer's wait for both locks, in seconds: past it the ask returns
/// [`NodeError::TimedOut`] with both locks released and its flag cleared.
pub const EXCLUSIVE_WAIT_S: u64 = 120;
/// How long the writer holds both locks before its lease returns, in
/// seconds: the kernel uncharges an ended node's locked memory within 0.3 s.
pub const SETTLE_S: u64 = 1;
/// Every lock wait polls its try at this interval.
const POLL: Duration = Duration::from_millis(10);
const LEAF: &str = "infinity-locked-memory";
const TURNSTILE: &str = "turnstile";
const NODES: &str = "nodes";

/// The `infinityd` a test runs. No path escapes it, its `Debug` included,
/// so the binary enters a command only through [`NodeBinary::command`].
pub struct NodeBinary {
    path: PathBuf,
}

impl fmt::Debug for NodeBinary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NodeBinary(..)")
    }
}

impl NodeBinary {
    /// The binary cargo built beside `bins/infinityd`'s tests. From another
    /// package it panics: there cargo names no such binary.
    pub fn built() -> Self {
        let Some(path) = option_env!("CARGO_BIN_EXE_infinityd") else {
            panic!(
                "NodeBinary::built() names the infinityd cargo builds for bins/infinityd's own \
                 tests; another package names its binary through a variable it reads"
            );
        };
        Self { path: PathBuf::from(path) }
    }

    /// The path the variable `var` names when it is set (a test pointed at
    /// another build), else [`NodeBinary::built`].
    pub fn built_or(var: &str) -> Self {
        match std::env::var_os(var) {
            Some(path) => Self { path: PathBuf::from(path) },
            None => Self::built(),
        }
    }

    /// The binary `INFINITYD_BIN` names, for a package cargo builds no
    /// `infinityd` beside (compat's recipe and CI set it). `None` when the
    /// variable is unset, empty or not Unicode: the caller skips its lane,
    /// or refuses when the lane is required, and never starts a path that
    /// names no binary.
    pub fn from_infinityd_bin() -> Option<Self> {
        match std::env::var("INFINITYD_BIN") {
            Ok(path) if !path.is_empty() => Some(Self { path: PathBuf::from(path) }),
            Ok(_) | Err(std::env::VarError::NotPresent | std::env::VarError::NotUnicode(_)) => None,
        }
    }

    /// The only command that runs the binary: its starts take the box's lock.
    pub fn command(&self) -> NodeCommand<BoxWide> {
        NodeCommand::under_lock(NodeLock::box_wide(), self.path.clone())
    }
}

/// A lock's scope: the box's one lock, or a test's own directory.
pub trait Scope: sealed::Sealed {}

mod sealed {
    pub trait Sealed {}
}

/// The box's node lock: only the node binary runs under it.
#[derive(Debug)]
pub enum BoxWide {}
/// A test's own lock directory: only the `sleep` stand-in runs under it.
#[derive(Debug)]
pub enum Private {}

impl sealed::Sealed for BoxWide {}
impl sealed::Sealed for Private {}
impl Scope for BoxWide {}
impl Scope for Private {}

/// The deadlines and the settle of one lock.
#[derive(Clone, Copy, Debug)]
pub struct Waits {
    /// A start's wait for both locks.
    pub shared: Duration,
    /// An exclusive ask's wait for both locks.
    pub exclusive: Duration,
    /// The writer's hold of both locks before its lease returns.
    pub settle: Duration,
}

impl Waits {
    /// The box-wide lock's: [`SHARED_WAIT_S`], [`EXCLUSIVE_WAIT_S`], [`SETTLE_S`].
    pub const BOX_WIDE: Waits = Waits {
        shared: Duration::from_secs(SHARED_WAIT_S),
        exclusive: Duration::from_secs(EXCLUSIVE_WAIT_S),
        settle: Duration::from_secs(SETTLE_S),
    };
}

/// A limit the shell sets before it runs the command: `ulimit -l` or `-v`.
#[derive(Clone, Copy, Debug)]
pub enum Ulimit {
    /// `RLIMIT_MEMLOCK`, in KiB.
    LockedKib(u32),
    /// `RLIMIT_AS`, in KiB.
    AddressSpaceKib(u64),
}

/// What this process holds on one lock: its live shared nodes (in the
/// process and on the calling thread) and whether it holds the lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Live {
    pub process: u32,
    pub thread: u32,
    pub leased: bool,
}

/// One lock's in-process state. A start adds to `live`, then reads
/// `leased`; an ask sets `leased`, then reads `live`; both `SeqCst`, so of
/// two racing calls at least one sees the other and refuses.
struct LockState {
    id: u64,
    live: AtomicU32,
    leased: AtomicBool,
}

impl LockState {
    fn new(id: u64) -> Self {
        Self { id, live: AtomicU32::new(0), leased: AtomicBool::new(false) }
    }
}

/// Private locks' ids; the box-wide lock is 0.
static NEXT_LOCK_ID: AtomicU64 = AtomicU64::new(1);

fn box_wide_state() -> Arc<LockState> {
    static STATE: OnceLock<Arc<LockState>> = OnceLock::new();
    Arc::clone(STATE.get_or_init(|| Arc::new(LockState::new(0))))
}

thread_local! {
    /// This thread's live nodes per lock id: a thread that holds one starts
    /// its next without the turnstile, so it never waits on a writer that
    /// waits on it.
    static THREAD_NODES: RefCell<Vec<(u64, u32)>> = const { RefCell::new(Vec::new()) };
}

fn thread_nodes(id: u64) -> u32 {
    THREAD_NODES.with(|nodes| {
        nodes.borrow().iter().find(|(lock, _)| *lock == id).map_or(0, |(_, count)| *count)
    })
}

fn thread_add(id: u64) {
    THREAD_NODES.with(|nodes| {
        let mut nodes = nodes.borrow_mut();
        match nodes.iter_mut().find(|(lock, _)| *lock == id) {
            Some((_, count)) => *count += 1,
            None => nodes.push((id, 1)),
        }
    });
}

fn thread_remove(id: u64) {
    THREAD_NODES.with(|nodes| {
        let mut nodes = nodes.borrow_mut();
        if let Some(index) = nodes.iter().position(|(lock, _)| *lock == id) {
            nodes[index].1 -= 1;
            if nodes[index].1 == 0 {
                nodes.swap_remove(index);
            }
        }
    });
}

/// One live shared node, counted in its process and on its thread until
/// drop. Not `Send`: the drop lowers its own thread's count.
struct Counted {
    state: Arc<LockState>,
    thread: PhantomData<*const ()>,
}

impl Counted {
    fn new(state: &Arc<LockState>) -> Self {
        state.live.fetch_add(1, Ordering::SeqCst);
        thread_add(state.id);
        Self { state: Arc::clone(state), thread: PhantomData }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        thread_remove(self.state.id);
        self.state.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The lease flag, cleared on drop: every `Err` arm of an ask restores it.
struct LeaseFlag(Arc<LockState>);

impl Drop for LeaseFlag {
    fn drop(&mut self) {
        self.0.leased.store(false, Ordering::SeqCst);
    }
}

/// A node lock: its directory, its waits and its in-process state.
pub struct NodeLock<S: Scope> {
    dir: PathBuf,
    waits: Waits,
    state: Arc<LockState>,
    scope: PhantomData<S>,
}

impl<S: Scope> fmt::Debug for NodeLock<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeLock").field("dir", &self.dir).field("waits", &self.waits).finish()
    }
}

impl NodeLock<BoxWide> {
    /// The box's lock, per user: under `XDG_RUNTIME_DIR` when it is set,
    /// else under the temp directory, named with the user's uid.
    pub fn box_wide() -> Self {
        let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(runtime) if !runtime.is_empty() => PathBuf::from(runtime).join(LEAF),
            _ => {
                let uid = own_uid().map_or_else(|_| "unknown".to_owned(), |uid| uid.to_string());
                std::env::temp_dir().join(format!("{LEAF}-{uid}"))
            }
        };
        Self { dir, waits: Waits::BOX_WIDE, state: box_wide_state(), scope: PhantomData }
    }
}

impl NodeLock<Private> {
    /// A test's own lock in `dir`, with its own deadlines and settle.
    pub fn at(dir: &Path, waits: Waits) -> Self {
        let id = NEXT_LOCK_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            dir: dir.to_path_buf(),
            waits,
            state: Arc::new(LockState::new(id)),
            scope: PhantomData,
        }
    }

    /// `sleep`, the only command a private lock starts: a stand-in node.
    pub fn stand_in(&self) -> NodeCommand<Private> {
        NodeCommand::under_lock(self.copy(), PathBuf::from("sleep"))
    }
}

impl<S: Scope> NodeLock<S> {
    /// The lock's directory, for a red's text.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The lease: `turnstile`, then `nodes`, exclusive, each a polled try
    /// under one deadline, then the settle with both held. Refused at once
    /// in a process that holds the lease or a live shared node of its own.
    pub fn exclusive(&self) -> Result<ExclusiveLease<S>, NodeError> {
        if self.state.leased.swap(true, Ordering::SeqCst) {
            return Err(NodeError::LeasedHere);
        }
        let flag = LeaseFlag(Arc::clone(&self.state));
        let live = self.state.live.load(Ordering::SeqCst);
        if live > 0 {
            return Err(NodeError::NodesHere(live));
        }
        ensure_dir(&self.dir)?;
        let began = now();
        let turnstile = open_lock(&self.dir, TURNSTILE)?;
        poll_lock(
            &turnstile,
            Mode::Exclusive,
            &self.dir.join(TURNSTILE),
            began,
            self.waits.exclusive,
        )?;
        let nodes = open_lock(&self.dir, NODES)?;
        poll_lock(&nodes, Mode::Exclusive, &self.dir.join(NODES), began, self.waits.exclusive)?;
        settle(self.waits.settle);
        Ok(ExclusiveLease { nodes, turnstile, flag, scope: PhantomData })
    }

    /// This process's live shared nodes and lease on this lock.
    pub fn live(&self) -> Live {
        Live {
            process: self.state.live.load(Ordering::SeqCst),
            thread: thread_nodes(self.state.id),
            leased: self.state.leased.load(Ordering::SeqCst),
        }
    }

    fn copy(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            waits: self.waits,
            state: Arc::clone(&self.state),
            scope: PhantomData,
        }
    }
}

/// The exclusive hold of a lock. Its fields drop in order: `nodes`, then
/// `turnstile`, then the flag; the nodes it started borrow it, so they are
/// gone first.
pub struct ExclusiveLease<S: Scope> {
    nodes: File,
    turnstile: File,
    flag: LeaseFlag,
    scope: PhantomData<S>,
}

/// A command a lock starts: the node binary's under the box's lock, the
/// stand-in's under a private one. By value; it has no stdin setter, since
/// its standard input carries the lock.
pub struct NodeCommand<S: Scope> {
    lock: NodeLock<S>,
    program: PathBuf,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
    ulimit: Option<Ulimit>,
}

/// No path: the binary's path never leaves the module through a command.
impl<S: Scope> fmt::Debug for NodeCommand<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NodeCommand(..)")
    }
}

impl<S: Scope> NodeCommand<S> {
    /// The module's own constructor. No `new` exists, private or public:
    /// the doc's block that calls one fails as a missing item, so only a
    /// constructor added to the type compiles it.
    fn under_lock(lock: NodeLock<S>, program: PathBuf) -> Self {
        Self {
            lock,
            program,
            args: Vec::new(),
            envs: Vec::new(),
            stdout: None,
            stderr: None,
            ulimit: None,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, A>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: AsRef<OsStr>,
    {
        self.args.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.envs.push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    pub fn stdout(mut self, stdout: impl Into<Stdio>) -> Self {
        self.stdout = Some(stdout.into());
        self
    }

    pub fn stderr(mut self, stderr: impl Into<Stdio>) -> Self {
        self.stderr = Some(stderr.into());
        self
    }

    /// Runs the command under `sh`, which sets the limit, then execs it:
    /// the node keeps the shell's pid and its standard input.
    pub fn ulimit(mut self, limit: Ulimit) -> Self {
        self.ulimit = Some(limit);
        self
    }

    /// Starts the command holding the lock shared: the child's standard
    /// input is the locked `nodes` description, so the lock lives as long
    /// as the child, and the returned [`Node`] kills and reaps it on drop.
    pub fn spawn(self) -> Result<Node, NodeError> {
        let program = self.program.clone();
        let (counted, mut command) = self.shared()?;
        let child = command.spawn().map_err(|error| spawn_failed(&program, STARTED, error))?;
        // This process's copy of the description closes here: the child's
        // fd 0 is its only holder.
        drop(command);
        Ok(Node { child, counted })
    }

    /// As [`NodeCommand::spawn`], for a child that exits by itself: waits
    /// for it in place and returns what it wrote, both outputs piped
    /// unless set.
    pub fn output(mut self) -> Result<Output, NodeError> {
        self.stdout.get_or_insert_with(Stdio::piped);
        self.stderr.get_or_insert_with(Stdio::piped);
        let program = self.program.clone();
        let (counted, mut command) = self.shared()?;
        let child = command.spawn().map_err(|error| spawn_failed(&program, STARTED, error))?;
        drop(command);
        let output =
            child.wait_with_output().map_err(|error| spawn_failed(&program, REAPED, error));
        drop(counted);
        output
    }

    /// Starts the command under the lease, taking no shared hold: its
    /// standard input is null. The returned [`LeasedNode`] borrows the lease.
    pub fn spawn_under(self, lease: &ExclusiveLease<S>) -> Result<LeasedNode<'_>, NodeError> {
        assert_eq!(lease.flag.0.id, self.lock.state.id, "a lease starts its own lock's commands");
        let program = self.program.clone();
        let (_, mut command) = self.into_command();
        command.stdin(Stdio::null());
        let child = command.spawn().map_err(|error| spawn_failed(&program, STARTED, error))?;
        Ok(LeasedNode { child, lease: PhantomData })
    }

    /// The shared hold: counted, then the flag read (a leased process
    /// refuses), then `turnstile` unless this thread holds a live node, then
    /// `nodes`, each a polled try under one deadline; `turnstile` released.
    /// Every `Err` arm drops what it took and the counts fall back.
    fn shared(self) -> Result<(Counted, Command), NodeError> {
        let state = Arc::clone(&self.lock.state);
        let nested = thread_nodes(state.id) > 0;
        let counted = Counted::new(&state);
        if state.leased.load(Ordering::SeqCst) {
            return Err(NodeError::LeasedHere);
        }
        let dir = self.lock.dir.clone();
        let wait = self.lock.waits.shared;
        ensure_dir(&dir)?;
        let began = now();
        let turnstile = if nested {
            None
        } else {
            let turnstile = open_lock(&dir, TURNSTILE)?;
            poll_lock(&turnstile, Mode::Shared, &dir.join(TURNSTILE), began, wait)?;
            Some(turnstile)
        };
        let nodes = open_lock(&dir, NODES)?;
        poll_lock(&nodes, Mode::Shared, &dir.join(NODES), began, wait)?;
        drop(turnstile);
        let (_, mut command) = self.into_command();
        command.stdin(Stdio::from(nodes));
        Ok((counted, command))
    }

    fn into_command(self) -> (NodeLock<S>, Command) {
        let mut command = match self.ulimit {
            None => Command::new(&self.program),
            Some(limit) => {
                let (flag, kib) = match limit {
                    Ulimit::LockedKib(kib) => ("-l", u64::from(kib)),
                    Ulimit::AddressSpaceKib(kib) => ("-v", kib),
                };
                let mut shell = Command::new("sh");
                shell
                    .arg("-c")
                    .arg(format!("ulimit {flag} \"$1\" && shift && exec \"$@\""))
                    .arg("sh")
                    .arg(kib.to_string())
                    .arg(&self.program);
                shell
            }
        };
        command.args(&self.args);
        for (key, value) in &self.envs {
            command.env(key, value);
        }
        if let Some(stdout) = self.stdout {
            command.stdout(stdout);
        }
        if let Some(stderr) = self.stderr {
            command.stderr(stderr);
        }
        (self.lock, command)
    }
}

macro_rules! child_methods {
    () => {
        pub fn id(&self) -> u32 {
            self.child.id()
        }

        pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            self.child.try_wait()
        }

        pub fn wait(&mut self) -> io::Result<ExitStatus> {
            self.child.wait()
        }

        pub fn kill(&mut self) -> io::Result<()> {
            self.child.kill()
        }

        pub fn take_stdout(&mut self) -> Option<ChildStdout> {
            self.child.stdout.take()
        }

        pub fn take_stderr(&mut self) -> Option<ChildStderr> {
            self.child.stderr.take()
        }
    };
}

/// A node started holding the lock shared. Its drop kills and reaps the
/// child, unwinding included, then lowers the counts; it owns the `Child`
/// with no way out to it. Not `Send`: its thread's count is its own.
pub struct Node {
    child: Child,
    counted: Counted,
}

impl Node {
    child_methods!();
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node").field("pid", &self.child.id()).finish()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A node started under a lease, which it borrows. Its drop kills and reaps
/// the child, unwinding included.
pub struct LeasedNode<'lease> {
    child: Child,
    lease: PhantomData<&'lease ()>,
}

impl LeasedNode<'_> {
    child_methods!();
}

impl fmt::Debug for LeasedNode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeasedNode").field("pid", &self.child.id()).finish()
    }
}

impl Drop for LeasedNode<'_> {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Why a start or an ask took nothing.
#[derive(Debug)]
pub enum NodeError {
    /// The lock's directory or one of its files cannot be used.
    Dir(DirRefusal),
    /// A lock wait passed its deadline.
    TimedOut(LockTimeout),
    /// A shared start in a process that holds the lease.
    LeasedHere,
    /// An ask in a process with this many live shared nodes of its own.
    NodesHere(u32),
    /// The child could not be started or waited for (`ENOENT`, `EACCES`):
    /// the error keeps its kind, and its text names the program.
    Spawn(io::Error),
}

/// A lock path and why it cannot be used.
#[derive(Debug)]
pub struct DirRefusal {
    pub path: PathBuf,
    pub reason: DirReason,
}

#[derive(Debug)]
pub enum DirReason {
    NotADirectory,
    /// Another user's directory: its uid, then this process's.
    OwnedBy {
        uid: u32,
        own: u32,
    },
    Io(io::Error),
}

/// The lock file a wait gave up on, and how long it waited.
#[derive(Debug)]
pub struct LockTimeout {
    pub lock: PathBuf,
    pub waited: Duration,
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeError::Dir(DirRefusal { path, reason }) => match reason {
                DirReason::NotADirectory => write!(f, "{} is not a directory", path.display()),
                DirReason::OwnedBy { uid, own } => {
                    write!(f, "{} is uid {uid}'s, not this user's ({own})", path.display())
                }
                DirReason::Io(error) => write!(f, "{}: {error}", path.display()),
            },
            NodeError::TimedOut(LockTimeout { lock, waited }) => write!(
                f,
                "waited {:.1} s for {}: another process holds it (/proc/locks names the \
                 process that took it; a node holds `nodes` as its fd 0)",
                waited.as_secs_f64(),
                lock.display()
            ),
            NodeError::LeasedHere => f.write_str("this process holds the node lock's lease"),
            NodeError::NodesHere(live) => {
                write!(f, "this process holds {live} live node(s) under the node lock")
            }
            NodeError::Spawn(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for NodeError {}

const STARTED: &str = "did not start";
const REAPED: &str = "could not be waited for";

/// A spawn's or a wait's failure, its kind kept. Only an error's text names a
/// binary's path (ADR-0203), so the program rides in the text, never in a
/// field a test could read back and start.
fn spawn_failed(program: &Path, what: &str, error: io::Error) -> NodeError {
    let text = format!("{} {what}: {error}", program.display());
    NodeError::Spawn(io::Error::new(error.kind(), text))
}

#[derive(Clone, Copy)]
enum Mode {
    Shared,
    Exclusive,
}

/// Makes the directory (0700) if it is missing, then refuses one that is
/// not a directory or is another user's.
#[allow(clippy::disallowed_methods, reason = "test-only: the lock directory's checks")]
fn ensure_dir(dir: &Path) -> Result<(), NodeError> {
    let refuse = |reason| NodeError::Dir(DirRefusal { path: dir.to_path_buf(), reason });
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(refuse(DirReason::Io(error))),
    }
    let metadata = std::fs::metadata(dir).map_err(|error| refuse(DirReason::Io(error)))?;
    if !metadata.is_dir() {
        return Err(refuse(DirReason::NotADirectory));
    }
    let own = own_uid()?;
    if metadata.uid() != own {
        return Err(refuse(DirReason::OwnedBy { uid: metadata.uid(), own }));
    }
    Ok(())
}

/// A fresh open of a lock file: a description of its own, which a child
/// can carry and whose last close frees the lock.
fn open_lock(dir: &Path, name: &str) -> Result<File, NodeError> {
    let path = dir.join(name);
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|error| NodeError::Dir(DirRefusal { path, reason: DirReason::Io(error) }))
}

/// A failed uid probe: the probe file's path, the error's kind and text.
type ProbeFailure = (PathBuf, io::ErrorKind, String);

/// This process's uid, from a file it makes and removes (std reads no uid
/// otherwise); probed once per process. A failure is refused against the
/// probe's own path, its error kind kept.
fn own_uid() -> Result<u32, NodeError> {
    static UID: OnceLock<Result<u32, ProbeFailure>> = OnceLock::new();
    match UID.get_or_init(probe_uid) {
        Ok(uid) => Ok(*uid),
        Err((path, kind, text)) => Err(NodeError::Dir(DirRefusal {
            path: path.clone(),
            reason: DirReason::Io(io::Error::new(*kind, text.clone())),
        })),
    }
}

#[allow(clippy::disallowed_methods, reason = "test-only: a uid probe file, made and removed")]
fn probe_uid() -> Result<u32, ProbeFailure> {
    let temp = std::env::temp_dir();
    let mut last = None;
    for attempt in 0..8 {
        let probe = temp.join(format!(".infinity-uid-probe-{}-{attempt}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&probe) {
            Ok(file) => {
                let uid = file.metadata().map(|metadata| metadata.uid());
                drop(file);
                let _ = std::fs::remove_file(&probe);
                return uid.map_err(|error| (probe, error.kind(), error.to_string()));
            }
            Err(error) => last = Some((probe, error.kind(), error.to_string())),
        }
    }
    Err(last.unwrap_or_else(|| (temp, io::ErrorKind::Other, "no uid probe was tried".to_owned())))
}

#[allow(clippy::disallowed_methods, reason = "test-only: a lock wait's wall-clock deadline")]
fn now() -> Instant {
    Instant::now()
}

/// One lock, polled every 10 ms until `wait` has passed since `began`: a
/// try never queues, so a writer gets no help from the kernel, and a holder
/// elsewhere never blocks this thread past its deadline.
#[allow(clippy::disallowed_methods, reason = "test-only: a polled lock wait under a deadline")]
fn poll_lock(
    file: &File,
    mode: Mode,
    path: &Path,
    began: Instant,
    wait: Duration,
) -> Result<(), NodeError> {
    loop {
        let tried = match mode {
            Mode::Shared => file.try_lock_shared(),
            Mode::Exclusive => file.try_lock(),
        };
        match tried {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                let path = path.to_path_buf();
                return Err(NodeError::Dir(DirRefusal { path, reason: DirReason::Io(error) }));
            }
        }
        let waited = began.elapsed();
        if waited >= wait {
            return Err(NodeError::TimedOut(LockTimeout { lock: path.to_path_buf(), waited }));
        }
        std::thread::sleep(POLL);
    }
}

#[allow(clippy::disallowed_methods, reason = "test-only: the writer's settle, both locks held")]
fn settle(time: Duration) {
    if !time.is_zero() {
        std::thread::sleep(time);
    }
}
