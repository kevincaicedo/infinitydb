//! The simulated network + [`SimDriver`]: per-cell in-memory connections
//! behind the real `BackendDriver` contract. Deterministic by construction:
//! `BTreeMap` iteration order, seeded chunk sizes, no wall clock, no real
//! syscalls.

use core::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::rc::Rc;

use inf_alloc::{BufferId, BufferPool, LeaseKind};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_runtime::{
    BackendDriver, Capabilities, Completion, CompletionResult, CompletionToken, FileOpenMode,
    FileSyncMode, IoOp, RawFd, SubmitStats, Wait,
};

const SIM_DISK_SECTOR_BYTES: usize = 512;

/// Fault plants (armed per scenario, fire on seeded draws).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Plant {
    #[default]
    None,
    /// Drop one recv readiness edge: the connection's pending bytes stop
    /// being delivered until *new* bytes arrive (the classic lost wakeup).
    /// Sequential clients never send again before the reply ⇒ stall.
    LostWakeup,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum SimFileOpKind {
    Open,
    CreateDir,
    Preallocate,
    Truncate,
    Read,
    Write,
    Sync,
    Close,
    Rename,
    Unlink,
}

#[derive(Debug, Default)]
struct SimConn {
    to_server: VecDeque<u8>,
    to_client: Vec<u8>,
    client_closed: bool,
    server_closed: bool,
    recv_armed: bool,
    recv_token: Option<CompletionToken>,
    /// Lost-wakeup plant fired here: delivery suppressed until new bytes.
    suppressed: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum SimNodeKind {
    File,
    Directory,
}

#[derive(Clone, Debug)]
struct PendingFileWrite {
    offset_bytes: u64,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
struct SimNode {
    kind: SimNodeKind,
    len_bytes: u64,
    bytes: Vec<u8>,
    stable_len_bytes: u64,
    stable_bytes: Vec<u8>,
    pending_writes: Vec<PendingFileWrite>,
    synced: bool,
}

impl SimNode {
    fn directory(synced: bool) -> SimNode {
        SimNode {
            kind: SimNodeKind::Directory,
            len_bytes: 0,
            bytes: Vec::new(),
            stable_len_bytes: 0,
            stable_bytes: Vec::new(),
            pending_writes: Vec::new(),
            synced,
        }
    }

    fn file(synced: bool) -> SimNode {
        SimNode {
            kind: SimNodeKind::File,
            len_bytes: 0,
            bytes: Vec::new(),
            stable_len_bytes: 0,
            stable_bytes: Vec::new(),
            pending_writes: Vec::new(),
            synced,
        }
    }

    fn sync_file(&mut self) {
        assert_eq!(self.kind, SimNodeKind::File);
        self.stable_len_bytes = self.len_bytes;
        self.stable_bytes = self.bytes.clone();
        self.pending_writes.clear();
        self.synced = true;
    }

    fn apply_power_cut(&mut self, rng: &mut SplitMix64) {
        assert_eq!(self.kind, SimNodeKind::File);
        let mut image = self.stable_bytes.clone();
        let mut order: Vec<usize> = (0..self.pending_writes.len()).collect();
        shuffle_order(&mut order, rng);

        for idx in order {
            let write = &self.pending_writes[idx];
            let keep = surviving_write_len(write.bytes.len(), rng);
            if keep == 0 {
                continue;
            }
            let offset = write.offset_bytes as usize;
            let end = offset + keep;
            if end > image.len() {
                image.resize(end, 0);
            }
            image[offset..end].copy_from_slice(&write.bytes[..keep]);
        }

        self.bytes = image;
        self.len_bytes = self.bytes.len() as u64;
        self.stable_len_bytes = self.len_bytes;
        self.stable_bytes = self.bytes.clone();
        self.pending_writes.clear();
        self.synced = true;
    }
}

fn shuffle_order(order: &mut [usize], rng: &mut SplitMix64) {
    let mut remaining = order.len();
    while remaining > 1 {
        let swap = rng.next_below(remaining as u64) as usize;
        order.swap(remaining - 1, swap);
        remaining -= 1;
    }
}

fn surviving_write_len(len: usize, rng: &mut SplitMix64) -> usize {
    assert!(len > 0);
    match rng.next_u64() % 3 {
        0 => 0,
        1 => len,
        _ => torn_sector_prefix_len(len, rng),
    }
}

fn torn_sector_prefix_len(len: usize, rng: &mut SplitMix64) -> usize {
    assert!(len > 0);
    let max_torn_sectors = (len - 1) / SIM_DISK_SECTOR_BYTES;
    if max_torn_sectors == 0 {
        return 0;
    }

    let sectors = 1 + rng.next_below(max_torn_sectors as u64) as usize;
    sectors * SIM_DISK_SECTOR_BYTES
}

/// One cell's network endpoint: the listener plus every connection accepted
/// by this cell. The harness holds the same handle to play the client side.
#[derive(Debug)]
pub struct CellNet {
    cell: u16,
    accept_armed: bool,
    accept_token: Option<CompletionToken>,
    backlog: VecDeque<RawFd>,
    conns: BTreeMap<RawFd, SimConn>,
    next_fd: RawFd,
    rng: SplitMix64,
    plant: Plant,
    plant_fired: bool,
    nodes: BTreeMap<u64, SimNode>,
    paths: BTreeMap<String, u64>,
    stable_paths: BTreeMap<String, u64>,
    fds: BTreeMap<RawFd, u64>,
    file_faults: BTreeMap<SimFileOpKind, VecDeque<i32>>,
    file_sync_mode_faults: VecDeque<(FileSyncMode, i32)>,
    next_inode: u64,
    next_file_fd: RawFd,
}

/// Synthetic listener fd for a cell (never a real fd).
pub fn listener_fd(cell: u16) -> RawFd {
    1_000_000 + i32::from(cell)
}

fn first_file_fd(cell: u16) -> RawFd {
    10_000_000 + i32::from(cell) * 100_000
}

impl CellNet {
    pub fn new(cell: u16, seed: u64, plant: Plant) -> Rc<RefCell<CellNet>> {
        let mut nodes = BTreeMap::new();
        let mut paths = BTreeMap::new();
        nodes.insert(1, SimNode::directory(true));
        paths.insert(".".to_string(), 1);
        let stable_paths = paths.clone();
        Rc::new(RefCell::new(CellNet {
            cell,
            accept_armed: false,
            accept_token: None,
            backlog: VecDeque::new(),
            conns: BTreeMap::new(),
            next_fd: 0,
            rng: SplitMix64::new(seed ^ 0xD15C_0000 ^ u64::from(cell)),
            plant,
            plant_fired: false,
            nodes,
            paths,
            stable_paths,
            fds: BTreeMap::new(),
            file_faults: BTreeMap::new(),
            file_sync_mode_faults: VecDeque::new(),
            next_inode: 1,
            next_file_fd: first_file_fd(cell),
        }))
    }

    /// Client side: open a connection to this cell; returns the fd handle.
    pub fn connect(&mut self) -> RawFd {
        self.next_fd += 1;
        let fd = i32::from(self.cell) * 100_000 + self.next_fd;
        self.conns.insert(fd, SimConn::default());
        self.backlog.push_back(fd);
        fd
    }

    /// Client side: send bytes. New arrivals clear a suppressed-delivery
    /// plant (edge-triggered semantics: the lost wakeup heals only on new
    /// data — which a reply-waiting client never produces).
    pub fn client_send(&mut self, fd: RawFd, bytes: &[u8]) {
        if let Some(conn) = self.conns.get_mut(&fd) {
            conn.to_server.extend(bytes);
            conn.suppressed = false;
        }
    }

    /// Client side: drain reply bytes.
    pub fn client_recv(&mut self, fd: RawFd) -> Vec<u8> {
        match self.conns.get_mut(&fd) {
            Some(conn) => core::mem::take(&mut conn.to_client),
            None => Vec::new(),
        }
    }

    /// Client side: half-close (FIN). The server reaps EOF and closes.
    pub fn client_close(&mut self, fd: RawFd) {
        if let Some(conn) = self.conns.get_mut(&fd) {
            conn.client_closed = true;
            conn.suppressed = false;
        }
    }

    /// True once the server closed its side too (teardown complete).
    pub fn closed(&self, fd: RawFd) -> bool {
        self.conns.get(&fd).is_none_or(|c| c.server_closed)
    }

    /// Total undelivered client→server bytes (progress accounting).
    pub fn pending_bytes(&self) -> usize {
        self.conns.values().map(|c| c.to_server.len()).sum()
    }

    pub fn fail_next_file_op(&mut self, kind: SimFileOpKind, errno: i32) {
        assert!(errno > 0, "sim file-op faults use positive errno values");
        self.file_faults.entry(kind).or_default().push_back(errno);
    }

    pub fn fail_next_file_sync(&mut self, mode: FileSyncMode, errno: i32) {
        assert!(errno > 0, "sim file-sync faults use positive errno values");
        self.file_sync_mode_faults.push_back((mode, errno));
    }

    /// Apply a deterministic power cut: unstable metadata reverts to the last
    /// directory fsync, and unstable file writes may be lost, torn, or applied
    /// in a seeded order.
    pub fn power_cut(&mut self, seed: u64) {
        let mut rng = SplitMix64::new(seed ^ 0xD15C_C070 ^ u64::from(self.cell));
        self.paths = self.stable_paths.clone();
        self.fds.clear();

        let live: BTreeSet<u64> = self.paths.values().copied().collect();
        self.nodes.retain(|inode, _| live.contains(inode));
        for node in self.nodes.values_mut() {
            match node.kind {
                SimNodeKind::File => node.apply_power_cut(&mut rng),
                SimNodeKind::Directory => node.synced = true,
            }
        }
    }

    #[cfg(test)]
    fn deterministic_disk_image(&self) -> Vec<u8> {
        fn push_bool(out: &mut Vec<u8>, value: bool) {
            out.push(u8::from(value));
        }

        fn push_u8(out: &mut Vec<u8>, value: u8) {
            out.push(value);
        }

        fn push_u64(out: &mut Vec<u8>, value: u64) {
            out.extend_from_slice(&value.to_le_bytes());
        }

        fn push_len(out: &mut Vec<u8>, len: usize) {
            push_u64(out, u64::try_from(len).expect("sim disk image field fits u64"));
        }

        fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
            push_len(out, bytes.len());
            out.extend_from_slice(bytes);
        }

        fn push_str(out: &mut Vec<u8>, value: &str) {
            push_bytes(out, value.as_bytes());
        }

        fn push_paths(out: &mut Vec<u8>, paths: &BTreeMap<String, u64>) {
            push_len(out, paths.len());
            for (path, inode) in paths {
                push_str(out, path);
                push_u64(out, *inode);
            }
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"inf-sim-disk-image-v1");
        push_paths(&mut out, &self.paths);
        push_paths(&mut out, &self.stable_paths);
        push_len(&mut out, self.nodes.len());
        for (inode, node) in &self.nodes {
            push_u64(&mut out, *inode);
            push_u8(
                &mut out,
                match node.kind {
                    SimNodeKind::File => 1,
                    SimNodeKind::Directory => 2,
                },
            );
            push_u64(&mut out, node.len_bytes);
            push_bytes(&mut out, &node.bytes);
            push_u64(&mut out, node.stable_len_bytes);
            push_bytes(&mut out, &node.stable_bytes);
            push_len(&mut out, node.pending_writes.len());
            for write in &node.pending_writes {
                push_u64(&mut out, write.offset_bytes);
                push_bytes(&mut out, &write.bytes);
            }
            push_bool(&mut out, node.synced);
        }
        out
    }

    fn pop_file_fault(&mut self, kind: SimFileOpKind) -> Option<i32> {
        let faults = self.file_faults.get_mut(&kind)?;
        let errno = faults.pop_front();
        if faults.is_empty() {
            self.file_faults.remove(&kind);
        }
        errno
    }

    fn pop_file_sync_fault(&mut self, mode: FileSyncMode) -> Option<i32> {
        let (wanted, errno) = self.file_sync_mode_faults.front().copied()?;
        if wanted != mode {
            return None;
        }
        self.file_sync_mode_faults.pop_front();
        Some(errno)
    }

    fn resolve_path(&self, dir: RawFd, name: &str) -> Result<String, i32> {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(libc::EINVAL);
        }
        if name == "." {
            return if dir == libc::AT_FDCWD { Ok(".".to_string()) } else { self.dir_path(dir) };
        }
        if name.starts_with('/') {
            return Ok(name.to_string());
        }
        let base = if dir == libc::AT_FDCWD { ".".to_string() } else { self.dir_path(dir)? };
        if base == "." { Ok(format!("./{name}")) } else { Ok(format!("{base}/{name}")) }
    }

    fn dir_path(&self, fd: RawFd) -> Result<String, i32> {
        let inode = *self.fds.get(&fd).ok_or(libc::EBADF)?;
        let node = self.nodes.get(&inode).ok_or(libc::EBADF)?;
        if node.kind != SimNodeKind::Directory {
            return Err(libc::ENOTDIR);
        }
        self.paths
            .iter()
            .find_map(|(path, candidate)| (*candidate == inode).then(|| path.clone()))
            .ok_or(libc::ENOENT)
    }

    fn alloc_file_fd(&mut self, inode: u64) -> RawFd {
        self.next_file_fd += 1;
        let fd = self.next_file_fd;
        self.fds.insert(fd, inode);
        fd
    }

    fn file_open(&mut self, dir: RawFd, name: String, mode: FileOpenMode) -> CompletionResult {
        let path = match self.resolve_path(dir, &name) {
            Ok(path) => path,
            Err(errno) => return CompletionResult::Error { errno, buf: None },
        };
        match mode {
            FileOpenMode::Directory => self.file_open_directory(&path),
            FileOpenMode::ReadOnly | FileOpenMode::ReadWrite => self.file_open_existing_file(&path),
            FileOpenMode::ReadWriteCreate => self.file_open_create_file(path),
            FileOpenMode::ReadWriteCreateTruncate => self.file_open_create_truncate_file(path),
        }
    }

    fn parent_path(path: &str) -> &str {
        match path.rfind('/') {
            Some(0) => "/",
            Some(idx) => &path[..idx],
            None => ".",
        }
    }

    fn parent_inode(&self, path: &str) -> Result<u64, i32> {
        let parent = Self::parent_path(path);
        let inode = self.paths.get(parent).copied().ok_or(libc::ENOENT)?;
        let node = self.nodes.get(&inode).ok_or(libc::ENOENT)?;
        if node.kind != SimNodeKind::Directory {
            return Err(libc::ENOTDIR);
        }
        Ok(inode)
    }

    fn file_create_dir(&mut self, dir: RawFd, name: String, _mode: u32) -> CompletionResult {
        let path = match self.resolve_path(dir, &name) {
            Ok(path) => path,
            Err(errno) => return CompletionResult::Error { errno, buf: None },
        };
        if self.paths.contains_key(&path) {
            return CompletionResult::Error { errno: libc::EEXIST, buf: None };
        }
        let parent_inode = match self.parent_inode(&path) {
            Ok(inode) => inode,
            Err(errno) => return CompletionResult::Error { errno, buf: None },
        };
        self.next_inode += 1;
        let inode = self.next_inode;
        self.nodes.insert(inode, SimNode::directory(false));
        self.paths.insert(path, inode);
        self.nodes.get_mut(&parent_inode).expect("parent checked above").synced = false;
        CompletionResult::FileDone
    }

    fn file_open_directory(&mut self, path: &str) -> CompletionResult {
        let Some(inode) = self.paths.get(path).copied() else {
            return CompletionResult::Error { errno: libc::ENOENT, buf: None };
        };
        let Some(node) = self.nodes.get(&inode) else {
            return CompletionResult::Error { errno: libc::ENOENT, buf: None };
        };
        if node.kind != SimNodeKind::Directory {
            return CompletionResult::Error { errno: libc::ENOTDIR, buf: None };
        }
        CompletionResult::FileOpened { fd: self.alloc_file_fd(inode) }
    }

    fn file_open_existing_file(&mut self, path: &str) -> CompletionResult {
        let Some(inode) = self.paths.get(path).copied() else {
            return CompletionResult::Error { errno: libc::ENOENT, buf: None };
        };
        let Some(node) = self.nodes.get(&inode) else {
            return CompletionResult::Error { errno: libc::ENOENT, buf: None };
        };
        if node.kind != SimNodeKind::File {
            return CompletionResult::Error { errno: libc::EISDIR, buf: None };
        }
        CompletionResult::FileOpened { fd: self.alloc_file_fd(inode) }
    }

    fn file_open_create_file(&mut self, path: String) -> CompletionResult {
        let inode = match self.paths.get(&path).copied() {
            Some(inode) => inode,
            None => {
                let parent_inode = match self.parent_inode(&path) {
                    Ok(inode) => inode,
                    Err(errno) => return CompletionResult::Error { errno, buf: None },
                };
                self.next_inode += 1;
                let inode = self.next_inode;
                self.nodes.insert(inode, SimNode::file(false));
                self.paths.insert(path, inode);
                self.nodes.get_mut(&parent_inode).expect("parent checked above").synced = false;
                inode
            }
        };
        let Some(node) = self.nodes.get(&inode) else {
            return CompletionResult::Error { errno: libc::ENOENT, buf: None };
        };
        if node.kind != SimNodeKind::File {
            return CompletionResult::Error { errno: libc::EISDIR, buf: None };
        }
        CompletionResult::FileOpened { fd: self.alloc_file_fd(inode) }
    }

    fn file_open_create_truncate_file(&mut self, path: String) -> CompletionResult {
        let inode = match self.paths.get(&path).copied() {
            Some(inode) => inode,
            None => {
                let parent_inode = match self.parent_inode(&path) {
                    Ok(inode) => inode,
                    Err(errno) => return CompletionResult::Error { errno, buf: None },
                };
                self.next_inode += 1;
                let inode = self.next_inode;
                self.nodes.insert(inode, SimNode::file(false));
                self.paths.insert(path, inode);
                self.nodes.get_mut(&parent_inode).expect("parent checked above").synced = false;
                inode
            }
        };
        let Some(node) = self.nodes.get_mut(&inode) else {
            return CompletionResult::Error { errno: libc::ENOENT, buf: None };
        };
        if node.kind != SimNodeKind::File {
            return CompletionResult::Error { errno: libc::EISDIR, buf: None };
        }
        node.len_bytes = 0;
        node.bytes.clear();
        node.pending_writes.clear();
        node.synced = false;
        CompletionResult::FileOpened { fd: self.alloc_file_fd(inode) }
    }

    fn file_preallocate(&mut self, fd: RawFd, len_bytes: u64) -> CompletionResult {
        let Some(inode) = self.fds.get(&fd).copied() else {
            return CompletionResult::Error { errno: libc::EBADF, buf: None };
        };
        let Some(node) = self.nodes.get_mut(&inode) else {
            return CompletionResult::Error { errno: libc::EBADF, buf: None };
        };
        if node.kind != SimNodeKind::File {
            return CompletionResult::Error { errno: libc::EISDIR, buf: None };
        }
        let Ok(new_len) = usize::try_from(len_bytes) else {
            return CompletionResult::Error { errno: libc::EINVAL, buf: None };
        };
        node.len_bytes = node.len_bytes.max(len_bytes);
        if new_len > node.bytes.len() {
            node.bytes.resize(new_len, 0);
        }
        node.synced = false;
        CompletionResult::FileDone
    }

    fn file_truncate(&mut self, fd: RawFd, len_bytes: u64) -> CompletionResult {
        let Some(inode) = self.fds.get(&fd).copied() else {
            return CompletionResult::Error { errno: libc::EBADF, buf: None };
        };
        let Some(node) = self.nodes.get_mut(&inode) else {
            return CompletionResult::Error { errno: libc::EBADF, buf: None };
        };
        if node.kind != SimNodeKind::File {
            return CompletionResult::Error { errno: libc::EISDIR, buf: None };
        }
        let Ok(new_len) = usize::try_from(len_bytes) else {
            return CompletionResult::Error { errno: libc::EINVAL, buf: None };
        };
        node.len_bytes = len_bytes;
        node.bytes.resize(new_len, 0);
        node.pending_writes.retain(|write| write.offset_bytes < len_bytes);
        for write in &mut node.pending_writes {
            let remaining = len_bytes.saturating_sub(write.offset_bytes);
            if remaining < write.bytes.len() as u64 {
                write.bytes.truncate(remaining as usize);
            }
        }
        node.synced = false;
        CompletionResult::FileDone
    }

    fn file_read_at(
        &self,
        fd: RawFd,
        offset_bytes: u64,
        buf: BufferId,
        len: u32,
        pool: &mut BufferPool,
    ) -> CompletionResult {
        if len as usize > pool.buf_size() {
            return CompletionResult::Error { errno: libc::EINVAL, buf: Some(buf) };
        }
        let Some(inode) = self.fds.get(&fd).copied() else {
            return CompletionResult::Error { errno: libc::EBADF, buf: Some(buf) };
        };
        let Some(node) = self.nodes.get(&inode) else {
            return CompletionResult::Error { errno: libc::EBADF, buf: Some(buf) };
        };
        if node.kind != SimNodeKind::File {
            return CompletionResult::Error { errno: libc::EISDIR, buf: Some(buf) };
        }
        let Ok(offset) = usize::try_from(offset_bytes) else {
            return CompletionResult::Error { errno: libc::EINVAL, buf: Some(buf) };
        };
        if offset >= node.bytes.len() {
            return CompletionResult::FileRead { buf, len: 0 };
        }
        let len = (len as usize).min(node.bytes.len() - offset);
        pool.bytes_mut(buf)[..len].copy_from_slice(&node.bytes[offset..offset + len]);
        CompletionResult::FileRead { buf, len: len as u32 }
    }

    fn file_write_at(
        &mut self,
        fd: RawFd,
        offset_bytes: u64,
        buf: BufferId,
        len: u32,
        pool: &BufferPool,
    ) -> CompletionResult {
        if len == 0 || len as usize > pool.buf_size() {
            return CompletionResult::Error { errno: libc::EINVAL, buf: Some(buf) };
        }
        let Some(inode) = self.fds.get(&fd).copied() else {
            return CompletionResult::Error { errno: libc::EBADF, buf: Some(buf) };
        };
        let Some(node) = self.nodes.get_mut(&inode) else {
            return CompletionResult::Error { errno: libc::EBADF, buf: Some(buf) };
        };
        if node.kind != SimNodeKind::File {
            return CompletionResult::Error { errno: libc::EISDIR, buf: Some(buf) };
        }
        let Ok(offset) = usize::try_from(offset_bytes) else {
            return CompletionResult::Error { errno: libc::EINVAL, buf: Some(buf) };
        };
        let Some(end) = offset.checked_add(len as usize) else {
            return CompletionResult::Error { errno: libc::EINVAL, buf: Some(buf) };
        };
        if end > node.bytes.len() {
            node.bytes.resize(end, 0);
        }
        node.bytes[offset..end].copy_from_slice(&pool.bytes(buf)[..len as usize]);
        node.len_bytes = node.len_bytes.max(end as u64);
        node.pending_writes.push(PendingFileWrite {
            offset_bytes,
            bytes: pool.bytes(buf)[..len as usize].to_vec(),
        });
        node.synced = false;
        CompletionResult::FileWritten { buf }
    }

    fn file_sync(&mut self, fd: RawFd) -> CompletionResult {
        let Some(inode) = self.fds.get(&fd).copied() else {
            return CompletionResult::Error { errno: libc::EBADF, buf: None };
        };
        let Some(node) = self.nodes.get(&inode) else {
            return CompletionResult::Error { errno: libc::EBADF, buf: None };
        };
        match node.kind {
            SimNodeKind::File => {
                let node = self.nodes.get_mut(&inode).expect("inode checked above");
                node.sync_file();
            }
            SimNodeKind::Directory => {
                self.stable_paths = self.paths.clone();
                let node = self.nodes.get_mut(&inode).expect("inode checked above");
                node.synced = true;
            }
        }
        CompletionResult::FileDone
    }

    fn file_close(&mut self, fd: RawFd) -> CompletionResult {
        if self.fds.remove(&fd).is_some() {
            CompletionResult::FileClosed
        } else {
            CompletionResult::Error { errno: libc::EBADF, buf: None }
        }
    }

    fn file_rename(
        &mut self,
        old_dir: RawFd,
        old_name: String,
        new_dir: RawFd,
        new_name: String,
    ) -> CompletionResult {
        let old_path = match self.resolve_path(old_dir, &old_name) {
            Ok(path) => path,
            Err(errno) => return CompletionResult::Error { errno, buf: None },
        };
        let new_path = match self.resolve_path(new_dir, &new_name) {
            Ok(path) => path,
            Err(errno) => return CompletionResult::Error { errno, buf: None },
        };
        let old_parent = match self.parent_inode(&old_path) {
            Ok(inode) => inode,
            Err(errno) => return CompletionResult::Error { errno, buf: None },
        };
        let new_parent = match self.parent_inode(&new_path) {
            Ok(inode) => inode,
            Err(errno) => return CompletionResult::Error { errno, buf: None },
        };
        let Some(inode) = self.paths.remove(&old_path) else {
            return CompletionResult::Error { errno: libc::ENOENT, buf: None };
        };
        self.paths.insert(new_path, inode);
        self.nodes.get_mut(&old_parent).expect("parent checked above").synced = false;
        self.nodes.get_mut(&new_parent).expect("parent checked above").synced = false;
        CompletionResult::FileDone
    }

    fn file_unlink(&mut self, dir: RawFd, name: String) -> CompletionResult {
        let path = match self.resolve_path(dir, &name) {
            Ok(path) => path,
            Err(errno) => return CompletionResult::Error { errno, buf: None },
        };
        let parent = match self.parent_inode(&path) {
            Ok(inode) => inode,
            Err(errno) => return CompletionResult::Error { errno, buf: None },
        };
        match self.paths.remove(&path) {
            Some(_) => {
                self.nodes.get_mut(&parent).expect("parent checked above").synced = false;
                CompletionResult::FileDone
            }
            None => CompletionResult::Error { errno: libc::ENOENT, buf: None },
        }
    }
}

/// `BackendDriver` over a [`CellNet`]. One per cell.
#[derive(Debug)]
pub struct SimDriver {
    net: Rc<RefCell<CellNet>>,
    ops: Vec<IoOp>,
    stats: SubmitStats,
}

impl SimDriver {
    pub fn new(net: Rc<RefCell<CellNet>>) -> SimDriver {
        SimDriver { net, ops: Vec::new(), stats: SubmitStats::default() }
    }
}

impl BackendDriver for SimDriver {
    fn push(&mut self, op: IoOp) {
        self.ops.push(op);
    }

    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        _wait: Wait,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        let before = out.len();
        let mut net = self.net.borrow_mut();
        let submitted = self.ops.len() as u64;

        for op in self.ops.drain(..) {
            match op {
                IoOp::AcceptArm { token, .. } => {
                    net.accept_armed = true;
                    net.accept_token = Some(token);
                }
                IoOp::RecvArm { fd, token } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.recv_armed = true;
                        conn.recv_token = Some(token);
                    }
                }
                IoOp::RecvDisarm { fd } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.recv_armed = false;
                    }
                }
                IoOp::Send { fd, buf, len, token } => {
                    let result = match net.conns.get_mut(&fd) {
                        Some(conn) if !conn.server_closed => {
                            conn.to_client.extend_from_slice(&pool.bytes(buf)[..len as usize]);
                            CompletionResult::Sent { buf }
                        }
                        _ => CompletionResult::Error { errno: libc::EPIPE, buf: Some(buf) },
                    };
                    out.push(Completion { token, result });
                }
                IoOp::Close { fd, token } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.server_closed = true;
                        conn.recv_armed = false;
                    }
                    out.push(Completion { token, result: CompletionResult::Closed });
                }
                IoOp::FileOpen { dir, name, mode, token } => {
                    let result = match net.pop_file_fault(SimFileOpKind::Open) {
                        Some(errno) => CompletionResult::Error { errno, buf: None },
                        None => net.file_open(dir, name, mode),
                    };
                    out.push(Completion { token, result });
                }
                IoOp::FileCreateDir { dir, name, mode, token } => {
                    let result = match net.pop_file_fault(SimFileOpKind::CreateDir) {
                        Some(errno) => CompletionResult::Error { errno, buf: None },
                        None => net.file_create_dir(dir, name, mode),
                    };
                    out.push(Completion { token, result });
                }
                IoOp::FilePreallocate { fd, len_bytes, token } => {
                    let result = match net.pop_file_fault(SimFileOpKind::Preallocate) {
                        Some(errno) => CompletionResult::Error { errno, buf: None },
                        None => net.file_preallocate(fd, len_bytes),
                    };
                    out.push(Completion { token, result });
                }
                IoOp::FileTruncate { fd, len_bytes, token } => {
                    let result = match net.pop_file_fault(SimFileOpKind::Truncate) {
                        Some(errno) => CompletionResult::Error { errno, buf: None },
                        None => net.file_truncate(fd, len_bytes),
                    };
                    out.push(Completion { token, result });
                }
                IoOp::FileReadAt { fd, offset_bytes, buf, len, token } => {
                    let result = match net.pop_file_fault(SimFileOpKind::Read) {
                        Some(errno) => CompletionResult::Error { errno, buf: Some(buf) },
                        None => net.file_read_at(fd, offset_bytes, buf, len, pool),
                    };
                    out.push(Completion { token, result });
                }
                IoOp::FileWriteAt { fd, offset_bytes, buf, len, token } => {
                    let result = match net.pop_file_fault(SimFileOpKind::Write) {
                        Some(errno) => CompletionResult::Error { errno, buf: Some(buf) },
                        None => net.file_write_at(fd, offset_bytes, buf, len, pool),
                    };
                    out.push(Completion { token, result });
                }
                IoOp::FileSync { fd, mode, token } => {
                    let result = match net
                        .pop_file_sync_fault(mode)
                        .or_else(|| net.pop_file_fault(SimFileOpKind::Sync))
                    {
                        Some(errno) => CompletionResult::Error { errno, buf: None },
                        None => net.file_sync(fd),
                    };
                    out.push(Completion { token, result });
                }
                IoOp::FileClose { fd, token } => {
                    let result = match net.pop_file_fault(SimFileOpKind::Close) {
                        Some(errno) => CompletionResult::Error { errno, buf: None },
                        None => net.file_close(fd),
                    };
                    out.push(Completion { token, result });
                }
                IoOp::FileRename { old_dir, old_name, new_dir, new_name, token } => {
                    let result = match net.pop_file_fault(SimFileOpKind::Rename) {
                        Some(errno) => CompletionResult::Error { errno, buf: None },
                        None => net.file_rename(old_dir, old_name, new_dir, new_name),
                    };
                    out.push(Completion { token, result });
                }
                IoOp::FileUnlink { dir, name, token } => {
                    let result = match net.pop_file_fault(SimFileOpKind::Unlink) {
                        Some(errno) => CompletionResult::Error { errno, buf: None },
                        None => net.file_unlink(dir, name),
                    };
                    out.push(Completion { token, result });
                }
            }
        }

        // Accept everything queued (multishot semantics).
        if net.accept_armed {
            let token = net.accept_token.expect("armed implies token");
            while let Some(fd) = net.backlog.pop_front() {
                out.push(Completion { token, result: CompletionResult::Accepted { fd } });
            }
        }

        // Deliver one seeded chunk per armed connection per reap (BTreeMap
        // order = deterministic). Chunk boundaries are random so spanning
        // frames exercise the parser's accumulator on every run.
        let fds: Vec<RawFd> = net.conns.keys().copied().collect();
        for fd in fds {
            let CellNet { conns, rng, plant, plant_fired, .. } = &mut *net;
            let Some(conn) = conns.get_mut(&fd) else { continue };
            if !conn.recv_armed || conn.server_closed || conn.suppressed {
                continue;
            }
            let token = conn.recv_token.expect("armed implies token");
            if conn.to_server.is_empty() {
                if conn.client_closed {
                    // EOF: zero-length recv with a leased buffer (contract).
                    if let Some(buf) = pool.try_lease(LeaseKind::Recv) {
                        conn.recv_armed = false;
                        out.push(Completion {
                            token,
                            result: CompletionResult::Recv { buf, len: 0 },
                        });
                    }
                }
                continue;
            }
            // The lost-wakeup plant: one seeded readiness edge vanishes.
            if *plant == Plant::LostWakeup && !*plant_fired && rng.next_u64() % 256 == 0 {
                conn.suppressed = true;
                *plant_fired = true;
                continue;
            }
            let Some(buf) = pool.try_lease(LeaseKind::Recv) else { continue };
            let max = conn.to_server.len().min(pool.buf_size());
            let chunk = 1 + (rng.next_u64() as usize) % max;
            let bytes = pool.bytes_mut(buf);
            for (i, b) in conn.to_server.drain(..chunk).enumerate() {
                bytes[i] = b;
            }
            out.push(Completion {
                token,
                result: CompletionResult::Recv { buf, len: chunk as u32 },
            });
        }

        let produced = out.len() - before;
        self.stats = SubmitStats { syscalls: 1, sqes: submitted, cqes: produced as u64 };
        Ok(produced)
    }

    fn register_pool(&mut self, _pool: &mut BufferPool) -> io::Result<()> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "sim",
            multishot_accept: true,
            multishot_recv: true,
            provided_buffers: false,
            fixed_buffers: false,
            single_issuer: true,
            defer_taskrun: false,
            performance_tier: false, // gate tooling must reject sim numbers
        }
    }

    fn submit_stats(&self) -> SubmitStats {
        self.stats
    }
}

pub mod crash_matrix {
    use std::collections::{BTreeMap, BTreeSet};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::Rc;

    use crate::durability::DurabilitySweepConfig;
    use inf_alloc::{BufferPool, LeaseKind};
    use inf_fabric::{Mesh, MeshConfig};
    use inf_foundation::rng::{Entropy, SplitMix64};
    use inf_foundation::time::{Nanos, VirtualClock};
    use inf_foundation::{CellId, hash64};
    use inf_runtime::{
        BackendDriver, CellLoop, CompletionResult, CompletionToken, FileOpenMode, FileSyncMode,
        IoOp, LoopConfig, RawFd, TokenClass, Wait,
    };
    use inf_server::checkpoint::{
        CheckpointImagePublishConfig, CheckpointImagePublishError, CheckpointKeyspacePublishConfig,
        CheckpointKeyspacePublishError, CheckpointKeyspaceSnapshotConfig,
        LiveCheckpointPublishConfig, LiveCheckpointPublisher,
        publish_checkpoint_keyspace_snapshot_image,
    };
    use inf_server::durability::DurabilityCell;
    use inf_server::log_bootstrap::{
        LogCheckpointId, LogCheckpointRef, LogDataRootConfig, LogFrameMeta, LogNamespaceId,
        LogRecoveryManifest, LogSegmentCatalog, LogSegmentId, first_boot_segment_catalog,
        load_recovery_manifest_in_data_root, open_checkpoint_directory_in_data_root,
        open_first_boot_log_writer_in_data_root, open_log_directory_in_data_root,
        open_recovered_log_writer_replaying_in_data_root,
        open_recovered_log_writer_replaying_manifest_in_data_root, scan_log_segment_names,
    };
    use inf_server::log_maintenance::{LogSegmentMaintenance, LogSegmentMaintenanceConfig};
    use inf_server::log_writer::{LogWriteCompletion, LogWriteIo};
    use inf_server::manifest::{
        RecoveryManifestPublishConfig, RecoveryManifestPublishError, publish_recovery_manifest,
    };
    use inf_server::ns_catalog::{
        NamespaceCatalogDataRootLoadConfig, NamespaceCatalogDataRootPublishConfig,
        NamespaceCatalogLivePublishConfig, NamespaceCatalogLivePublisher,
        load_namespace_catalog_in_data_root, publish_namespace_catalog_in_data_root,
    };
    use inf_server::{NodeInfo, ServerPlane};
    use inf_store::{
        Keyspace, MutationEffect, NsCatalog, NsFsyncPolicy, NsId, NsMode, NsSpec, SetOptions,
        StoreConfig,
    };
    use toml::Value;

    use super::{CellNet, Plant, SimDriver, SimFileOpKind, SimNodeKind, listener_fd};

    const M2_CRASH_MATRIX_TOML: &str = include_str!("../../../tests/crash-matrix/m2.toml");
    const M2_RECOVERY_DATA_ROOT: &str = "m2-data";
    const M2_RECOVERY_BOOTSTRAP_TOKEN_SLOT: u32 = 0x00_D200;
    const M2_RECOVERY_WRITER_TOKEN_SLOT: u32 = 0x00_D201;
    const M2_RECOVERY_NS_CATALOG_TOKEN_SLOT: u32 = 0x00_D202;
    const M2_RECOVERY_MANIFEST_TOKEN_SLOT: u32 = 0x00_D203;
    const M2_CHECKPOINT_IMAGE_TOKEN_SLOT: u32 = 0x00_D204;
    const M2_LOG_MAINTENANCE_TOKEN_SLOT: u32 = 0x00_D205;
    const PUBLIC_SIM_MAX_ITERS: usize = 512;
    const PUBLIC_RECOVERY_SWEEP_HASH_SEED: u64 = 0xD2D2_0045;
    const PUBLIC_EVERYSEC_SWEEP_HASH_SEED: u64 = 0xE5EC_0084;
    const PUBLIC_EVERYSEC_WORKLOAD_SWEEP_HASH_SEED: u64 = 0xE5EC_0085;
    const EXPECTED_RUNNER_ROWS: [&str; 20] = [
        "public_always_single_write_power_cut",
        "public_always_batched_pipeline_power_cut",
        "public_everysec_single_write_contract",
        "public_always_single_write_fsync_err_fail_stop",
        "public_fsync_err_after_prior_frame_recovers_previous_watermark",
        "public_log_append_write_fault_fail_stop",
        "public_power_cut_after_seal_recovers_rotated_segment",
        "public_power_cut_after_non_exact_seal_recovers_truncated_segment",
        "public_torn_final_frame_recovers_stable_prefix",
        "public_active_tail_later_magic_truncates_prefix",
        "public_manifest_checkpoint_tail_power_cut",
        "public_manifest_rename_fail_full_log_recovery",
        "public_manifest_dir_fsync_fail_full_log_recovery",
        "public_live_checkpoint_wait_dir_fsync_fail_no_reply",
        "public_checkpoint_write_enospc_preserves_old_manifest",
        "public_manifest_replacement_dir_fsync_fail_preserves_old_manifest",
        "public_manifest_replacement_rename_fail_preserves_old_manifest",
        "public_always_recovered_state_sweep",
        "public_everysec_loss_window_sweep",
        "public_everysec_workload_sweep",
    ];

    #[derive(Clone, PartialEq, Eq, Debug)]
    pub struct M2CrashMatrixReport {
        pub rows: u64,
        pub manifest: Vec<u8>,
        pub manifest_hash: u64,
        pub violations: Vec<String>,
    }

    impl M2CrashMatrixReport {
        pub fn ok(&self) -> bool {
            self.violations.is_empty()
        }
    }

    pub fn run_m2_crash_matrix_rows() -> M2CrashMatrixReport {
        run_m2_crash_matrix_rows_from_toml(M2_CRASH_MATRIX_TOML)
    }

    pub fn run_m2_crash_matrix_rows_from_toml(matrix_toml: &str) -> M2CrashMatrixReport {
        let mut report = M2CrashMatrixReport {
            rows: 0,
            manifest: Vec::new(),
            manifest_hash: 0,
            violations: Vec::new(),
        };
        let matrix = match toml::from_str::<Value>(matrix_toml) {
            Ok(matrix) => matrix,
            Err(error) => {
                report.violations.push(format!("matrix TOML parse failed: {error}"));
                return finish_report(report);
            }
        };
        if let Err(error) = require_eq(&matrix, "status", "partial-runner") {
            report.violations.push(error);
        }
        let Some(rows) = matrix.get("runner_rows").and_then(Value::as_array) else {
            report.violations.push("matrix is missing runner_rows".to_string());
            return finish_report(report);
        };

        let mut ran = BTreeSet::new();
        for row in rows {
            report.rows += 1;
            let id = optional_string(row, "id").unwrap_or("<missing id>");
            let outcome = catch_unwind(AssertUnwindSafe(|| execute_runner_row(row)));
            match outcome {
                Ok(Ok(evidence)) => {
                    if !ran.insert(evidence.id.clone()) {
                        report.violations.push(format!("duplicate runner row {}", evidence.id));
                    }
                    report.manifest.extend_from_slice(&evidence.manifest);
                }
                Ok(Err(error)) => report.violations.push(error),
                Err(payload) => {
                    report
                        .violations
                        .push(format!("runner row {id} panicked: {}", panic_payload(&*payload)));
                }
            }
        }

        for expected in EXPECTED_RUNNER_ROWS {
            if !ran.contains(expected) {
                report.violations.push(format!("missing expected runner row {expected}"));
            }
        }

        finish_report(report)
    }

    fn finish_report(mut report: M2CrashMatrixReport) -> M2CrashMatrixReport {
        report.manifest_hash = hash64(&report.manifest, 0xD2D2_0049);
        report
    }

    struct RowEvidence {
        id: String,
        manifest: Vec<u8>,
    }

    fn execute_runner_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "status", "ci-green")?;
        require_eq(row, "runner", "inf-sim --scenario m2-crash-matrix --verify-determinism")?;
        match required_string(row, "id")? {
            "public_always_single_write_power_cut" => execute_public_always_row(row),
            "public_always_batched_pipeline_power_cut" => {
                execute_public_always_batched_pipeline_row(row)
            }
            "public_everysec_single_write_contract" => execute_public_everysec_row(row),
            "public_always_recovered_state_sweep" => execute_public_always_sweep_row(row),
            "public_everysec_loss_window_sweep" => execute_public_everysec_sweep_row(row),
            "public_everysec_workload_sweep" => execute_public_everysec_workload_sweep_row(row),
            "public_always_single_write_fsync_err_fail_stop" => execute_public_fsync_err_row(row),
            "public_fsync_err_after_prior_frame_recovers_previous_watermark" => {
                execute_public_fsync_err_restart_watermark_row(row)
            }
            "public_log_append_write_fault_fail_stop" => {
                execute_public_log_append_write_fault_row(row)
            }
            "public_power_cut_after_seal_recovers_rotated_segment" => {
                execute_public_power_cut_after_seal_row(row)
            }
            "public_power_cut_after_non_exact_seal_recovers_truncated_segment" => {
                execute_public_power_cut_after_non_exact_seal_row(row)
            }
            "public_torn_final_frame_recovers_stable_prefix" => {
                execute_public_torn_final_frame_row(row)
            }
            "public_active_tail_later_magic_truncates_prefix" => {
                execute_public_active_tail_later_magic_row(row)
            }
            "public_manifest_checkpoint_tail_power_cut" => execute_public_manifest_row(row),
            "public_manifest_rename_fail_full_log_recovery" => {
                execute_public_manifest_rename_fail_row(row)
            }
            "public_manifest_dir_fsync_fail_full_log_recovery" => {
                execute_public_manifest_dir_fsync_fail_row(row)
            }
            "public_live_checkpoint_wait_dir_fsync_fail_no_reply" => {
                execute_public_live_checkpoint_wait_dir_fsync_fail_row(row)
            }
            "public_checkpoint_write_enospc_preserves_old_manifest" => {
                execute_public_checkpoint_write_enospc_row(row)
            }
            "public_manifest_replacement_dir_fsync_fail_preserves_old_manifest" => {
                execute_public_manifest_replacement_dir_fsync_fail_row(row)
            }
            "public_manifest_replacement_rename_fail_preserves_old_manifest" => {
                execute_public_manifest_replacement_rename_fail_row(row)
            }
            other => Err(format!("unhandled M2 crash-matrix runner row {other}")),
        }
    }

    fn execute_public_always_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "torn_frame")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "single_write")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_always_recovery_report(seed);
        if !report.value_survived {
            return Err("always runner row lost an acknowledged write".to_string());
        }
        if report.watermark_at_cut == 0 {
            return Err("always runner row cut before durability watermark advanced".to_string());
        }
        if report.replay_frames != 1 || report.replay_records != 1 {
            return Err(format!(
                "always runner row replayed {} frames / {} records, expected 1 / 1",
                report.replay_frames, report.replay_records
            ));
        }

        let id = "public_always_single_write_power_cut".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "always");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, report.watermark_at_cut);
        append_bool(&mut manifest, report.value_survived);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_always_batched_pipeline_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "torn_frame")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "batched_pipeline")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_always_batched_pipeline_recovery_report(seed);
        if report.survived_keys != 2 {
            return Err(format!(
                "always batched-pipeline row recovered {} keys, expected 2",
                report.survived_keys
            ));
        }
        if report.watermark_at_cut == 0 {
            return Err(
                "always batched-pipeline row cut before durability watermark advanced".to_string()
            );
        }
        if report.replay_frames != 1 || report.replay_records != 2 {
            return Err(format!(
                "always batched-pipeline row replayed {} frames / {} records, expected 1 / 2",
                report.replay_frames, report.replay_records
            ));
        }

        let id = "public_always_batched_pipeline_power_cut".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "always");
        append_str(&mut manifest, "batched_pipeline");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, report.watermark_at_cut);
        append_u64(&mut manifest, report.survived_keys);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_everysec_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "torn_frame")?;
        require_eq(row, "fsync_policy", "everysec")?;
        require_eq(row, "workload", "single_write")?;
        let seed = matrix_seed(row, "seed")?;
        let synced = public_everysec_recovery_report(seed, true);
        if !synced.value_survived {
            return Err("everysec synced runner row lost a timer-fsynced write".to_string());
        }
        if synced.watermark_at_cut == 0 {
            return Err("everysec synced runner row cut before timer fsync watermark".to_string());
        }
        if synced.replay_frames != 1 || synced.replay_records != 1 {
            return Err(format!(
                "everysec synced runner row replayed {} frames / {} records, expected 1 / 1",
                synced.replay_frames, synced.replay_records
            ));
        }

        let base = matrix_seed(row, "loss_seed_base")?;
        let count = matrix_seed(row, "loss_seed_count")?;
        let mut lost = None;
        for offset in 0..count {
            let report = public_everysec_recovery_report(base ^ offset, false);
            if report.watermark_at_cut != 0 {
                return Err(format!(
                    "everysec pre-fsync loss row advanced watermark for seed {:#x}",
                    report.seed
                ));
            }
            if !report.value_survived {
                lost = Some(report);
                break;
            }
        }
        let Some(lost) = lost else {
            return Err(format!(
                "everysec loss-window row found no lost seed in {count} attempts from {base:#x}"
            ));
        };
        if lost.replay_frames != 0 || lost.replay_records != 0 {
            return Err(format!(
                "everysec loss seed replayed {} frames / {} records, expected 0 / 0",
                lost.replay_frames, lost.replay_records
            ));
        }

        let id = "public_everysec_single_write_contract".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "everysec");
        append_u64(&mut manifest, synced.seed);
        append_u64(&mut manifest, synced.replay_frames);
        append_u64(&mut manifest, synced.replay_records);
        append_u64(&mut manifest, synced.watermark_at_cut);
        append_bool(&mut manifest, synced.value_survived);
        append_u64(&mut manifest, lost.seed);
        append_u64(&mut manifest, lost.replay_frames);
        append_u64(&mut manifest, lost.replay_records);
        append_u64(&mut manifest, lost.watermark_at_cut);
        append_bool(&mut manifest, lost.value_survived);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_always_sweep_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "torn_frame")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "public_recovered_state_sweep")?;
        let config = sweep_config_from_row(row)?;
        let report = run_public_durable_recovery_sweep(&config);
        let expected_hash = matrix_seed(row, "expected_hash")?;
        if report.manifest_hash != expected_hash {
            return Err(format!(
                "always public sweep hash {:#018x}, expected {expected_hash:#018x}",
                report.manifest_hash
            ));
        }
        if report.manifest.len() != report.seeds as usize * 48 {
            return Err(format!(
                "always public sweep manifest {} bytes, expected {}",
                report.manifest.len(),
                report.seeds * 48
            ));
        }

        let id = "public_always_recovered_state_sweep".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "always");
        append_str(&mut manifest, "public_recovered_state_sweep");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.seeds);
        append_u64(&mut manifest, report.writes_per_seed);
        append_u64(&mut manifest, report.key_space);
        append_u64(&mut manifest, report.manifest_hash);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_everysec_sweep_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "torn_frame")?;
        require_eq(row, "fsync_policy", "everysec")?;
        require_eq(row, "workload", "public_recovered_state_sweep")?;
        let config = sweep_config_from_row(row)?;
        let report = run_public_everysec_recovery_sweep(&config);
        let expected_hash = matrix_seed(row, "expected_hash")?;
        if report.manifest_hash != expected_hash {
            return Err(format!(
                "everysec public sweep hash {:#018x}, expected {expected_hash:#018x}",
                report.manifest_hash
            ));
        }
        if !report.ok() {
            return Err(format!(
                "everysec public sweep did not realize loss and survival: lost {}, survived {}, \
                 post {}",
                report.pre_timer_loss_cases,
                report.pre_timer_survival_cases,
                report.post_timer_survival_cases
            ));
        }

        let id = "public_everysec_loss_window_sweep".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "everysec");
        append_str(&mut manifest, "public_recovered_state_sweep");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.seeds);
        append_u64(&mut manifest, report.pre_timer_loss_cases);
        append_u64(&mut manifest, report.pre_timer_survival_cases);
        append_u64(&mut manifest, report.post_timer_survival_cases);
        append_u64(&mut manifest, report.manifest_hash);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_everysec_workload_sweep_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "torn_frame")?;
        require_eq(row, "fsync_policy", "everysec")?;
        require_eq(row, "workload", "public_everysec_workload_sweep")?;
        let config = sweep_config_from_row(row)?;
        let report = run_public_everysec_workload_sweep(&config);
        let expected_hash = matrix_seed(row, "expected_hash")?;
        if report.manifest_hash != expected_hash {
            return Err(format!(
                "everysec workload sweep hash {:#018x}, expected {expected_hash:#018x}",
                report.manifest_hash
            ));
        }
        if !report.ok() {
            return Err(format!(
                "everysec workload sweep did not realize truncation/full survival: truncated {}, \
                 full-window {}, full-flush {}",
                report.loss_window_truncated_cases,
                report.loss_window_full_survival_cases,
                report.full_flush_survival_cases
            ));
        }

        let id = "public_everysec_workload_sweep".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "everysec");
        append_str(&mut manifest, "public_everysec_workload_sweep");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.seeds);
        append_u64(&mut manifest, report.writes_per_seed);
        append_u64(&mut manifest, report.key_space);
        append_u64(&mut manifest, report.loss_window_truncated_cases);
        append_u64(&mut manifest, report.loss_window_full_survival_cases);
        append_u64(&mut manifest, report.full_flush_survival_cases);
        append_u64(&mut manifest, report.manifest_hash);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_fsync_err_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "fsync_err")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "single_write")?;
        require_eq(
            row,
            "process_runner",
            "inf-sim --scenario m2-fsync-err-process-fail-stop --seed 0xF5E10023",
        )?;
        require_eq(
            row,
            "process_test",
            "inf-sim::m2_crash_matrix::m2_fsync_err_process_fail_stop_exits_nonzero",
        )?;
        let seed = matrix_seed(row, "seed")?;
        let base = matrix_seed(row, "loss_seed_base")?;
        let count = matrix_seed(row, "loss_seed_count")?;
        let mut selected = None;
        for offset in 0..count {
            let report = public_fsync_err_report(seed, base ^ offset);
            validate_fsync_err_fail_stop(&report)?;
            if !report.value_survived && report.replay_frames == 0 && report.replay_records == 0 {
                selected = Some(report);
                break;
            }
        }
        let Some(report) = selected else {
            return Err(format!(
                "fsync_err row found no pre-batch recovery seed in {count} attempts from {base:#x}"
            ));
        };

        let id = "public_always_single_write_fsync_err_fail_stop".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "fsync_err");
        append_str(&mut manifest, "always");
        append_str(&mut manifest, required_string(row, "process_runner")?);
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.power_cut_seed);
        append_u64(&mut manifest, report.reply_bytes_before_fail_stop);
        append_u64(&mut manifest, report.watermark_before_fail_stop);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_bool(&mut manifest, report.value_survived);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_fsync_err_restart_watermark_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "fsync_err")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "restart_watermark")?;
        let seed = matrix_seed(row, "seed")?;
        let base = matrix_seed(row, "loss_seed_base")?;
        let count = matrix_seed(row, "loss_seed_count")?;
        let mut selected = None;
        for offset in 0..count {
            let report = public_fsync_err_restart_watermark_report(seed, base ^ offset);
            validate_fsync_err_restart_watermark_fail_stop(&report)?;
            if report.stable_value_survived && report.failed_value_absent {
                selected = Some(report);
                break;
            }
        }
        let Some(report) = selected else {
            return Err(format!(
                "fsync_err restart-watermark row found no prefix-only recovery seed in {count} \
                 attempts from {base:#x}"
            ));
        };

        let id = "public_fsync_err_after_prior_frame_recovers_previous_watermark".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "fsync_err");
        append_str(&mut manifest, "always");
        append_str(&mut manifest, "restart_watermark");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.power_cut_seed);
        append_u64(&mut manifest, report.previous_watermark);
        append_u64(&mut manifest, report.watermark_before_fail_stop);
        append_u64(&mut manifest, report.reply_bytes_before_fail_stop);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_bool(&mut manifest, report.stable_value_survived);
        append_bool(&mut manifest, report.failed_value_absent);
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_log_append_write_fault_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "log_append_short_write")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "single_write")?;
        require_eq(
            row,
            "oracle_scope",
            "terminal_file_write_error_under_current_backend_contract",
        )?;
        require_eq(
            row,
            "process_runner",
            "inf-sim --scenario m2-log-append-write-fault-process-fail-stop --seed 0xD279",
        )?;
        require_eq(
            row,
            "process_test",
            "inf-sim::m2_crash_matrix::m2_log_append_write_fault_process_fail_stop_exits_nonzero",
        )?;
        let seed = matrix_seed(row, "seed")?;
        let power_cut_seed = matrix_seed(row, "power_cut_seed")?;
        let report = public_log_append_write_fault_report(seed, power_cut_seed);
        validate_log_append_write_fault_fail_stop(&report)?;

        let id = "public_log_append_write_fault_fail_stop".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "log_append_short_write");
        append_str(&mut manifest, "always");
        append_str(&mut manifest, required_string(row, "oracle_scope")?);
        append_str(&mut manifest, required_string(row, "process_runner")?);
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.power_cut_seed);
        append_u64(&mut manifest, report.reply_bytes_before_fail_stop);
        append_u64(&mut manifest, report.watermark_before_fail_stop);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_bool(&mut manifest, report.value_survived);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_power_cut_after_seal_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "power_cut_after_seal")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "segment_recovery")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_power_cut_after_seal_recovery_report(seed);
        if report.replay_frames != 3 || report.replay_records != 3 {
            return Err(format!(
                "power_cut_after_seal row replayed {} frames / {} records, expected 3 / 3",
                report.replay_frames, report.replay_records
            ));
        }
        if report.active_segment != 1 {
            return Err(format!(
                "power_cut_after_seal row recovered active segment {}, expected 1",
                report.active_segment
            ));
        }
        if report.active_offset_bytes == 0 {
            return Err("power_cut_after_seal row recovered segment 1 at zero offset".to_string());
        }
        if report.sealed_segment_len_bytes != 256 {
            return Err(format!(
                "power_cut_after_seal row sealed segment length {}, expected 256",
                report.sealed_segment_len_bytes
            ));
        }
        if report.survived_keys != 3 {
            return Err(format!(
                "power_cut_after_seal row recovered {} keys, expected 3",
                report.survived_keys
            ));
        }

        let id = "public_power_cut_after_seal_recovers_rotated_segment".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "power_cut_after_seal");
        append_str(&mut manifest, "segment_recovery");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, u64::from(report.active_segment));
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        append_u64(&mut manifest, report.sealed_segment_len_bytes);
        append_u64(&mut manifest, report.survived_keys);
        append_u64(&mut manifest, report.watermark_at_cut);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_power_cut_after_non_exact_seal_row(
        row: &Value,
    ) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "power_cut_after_seal")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "segment_recovery")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_power_cut_after_non_exact_seal_recovery_report(seed);
        if report.replay_frames != 3 || report.replay_records != 3 {
            return Err(format!(
                "power_cut_after_non_exact_seal row replayed {} frames / {} records, expected 3 / 3",
                report.replay_frames, report.replay_records
            ));
        }
        if report.active_segment != 1 {
            return Err(format!(
                "power_cut_after_non_exact_seal row recovered active segment {}, expected 1",
                report.active_segment
            ));
        }
        if report.active_offset_bytes == 0 {
            return Err(
                "power_cut_after_non_exact_seal row recovered segment 1 at zero offset".to_string()
            );
        }
        if report.sealed_segment_len_bytes != 208 {
            return Err(format!(
                "power_cut_after_non_exact_seal row sealed segment length {}, expected 208",
                report.sealed_segment_len_bytes
            ));
        }
        if report.survived_keys != 3 {
            return Err(format!(
                "power_cut_after_non_exact_seal row recovered {} keys, expected 3",
                report.survived_keys
            ));
        }

        let id = "public_power_cut_after_non_exact_seal_recovers_truncated_segment".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "power_cut_after_seal");
        append_str(&mut manifest, "segment_recovery");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, u64::from(report.active_segment));
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        append_u64(&mut manifest, report.sealed_segment_len_bytes);
        append_u64(&mut manifest, report.survived_keys);
        append_u64(&mut manifest, report.watermark_at_cut);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_torn_final_frame_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "torn_frame")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "segment_recovery")?;
        require_eq(row, "path", "torn_final_frame")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_torn_final_frame_recovery_report(seed);
        validate_torn_final_frame_report(&report)?;

        let id = "public_torn_final_frame_recovers_stable_prefix".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "torn_frame");
        append_str(&mut manifest, "segment_recovery");
        append_str(&mut manifest, "torn_final_frame");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, u64::from(report.stable_frame_end_bytes));
        append_u64(&mut manifest, u64::from(report.corrupt_frame_offset_bytes));
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        append_bool(&mut manifest, report.torn_tail_truncated);
        append_bool(&mut manifest, report.stable_value_survived);
        append_bool(&mut manifest, report.corrupt_value_absent);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_active_tail_later_magic_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "torn_frame")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "segment_recovery")?;
        require_eq(row, "path", "active_tail_later_magic")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_active_tail_later_magic_recovery_report(seed);
        validate_active_tail_later_magic_report(&report)?;

        let id = "public_active_tail_later_magic_truncates_prefix".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "torn_frame");
        append_str(&mut manifest, "segment_recovery");
        append_str(&mut manifest, "active_tail_later_magic");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, u64::from(report.stable_frame_end_bytes));
        append_u64(&mut manifest, u64::from(report.corrupt_offset_bytes));
        append_u64(&mut manifest, u64::from(report.later_frame_offset_bytes));
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        append_bool(&mut manifest, report.active_tail_truncated);
        append_bool(&mut manifest, report.stable_value_survived);
        append_bool(&mut manifest, report.corrupt_value_absent);
        append_bool(&mut manifest, report.later_value_absent);
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_manifest_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "power_cut_after_manifest")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "checkpoint_tail")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_manifest_checkpoint_tail_recovery_report(seed);
        if report.checkpoint_records != 1 {
            return Err(format!(
                "manifest row applied {} checkpoint records, expected 1",
                report.checkpoint_records
            ));
        }
        if report.replay_frames != 2 || report.replay_records != 1 {
            return Err(format!(
                "manifest row replayed {} frames / {} records, expected 2 / 1",
                report.replay_frames, report.replay_records
            ));
        }
        if report.active_offset_bytes == 0 {
            return Err("manifest row recovered writer at zero active offset".to_string());
        }

        let id = "public_manifest_checkpoint_tail_power_cut".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "power_cut_after_manifest");
        append_str(&mut manifest, "checkpoint_tail");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.checkpoint_records);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_manifest_rename_fail_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "manifest_rename_fail")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "checkpoint_tail")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_manifest_rename_fail_recovery_report(seed);
        if report.manifest_present_after_recovery {
            return Err(
                "manifest rename-fail row loaded a MANIFEST after failed rename".to_string()
            );
        }
        if report.checkpoint_records != 1 {
            return Err(format!(
                "manifest rename-fail row published {} checkpoint records, expected 1",
                report.checkpoint_records
            ));
        }
        if report.replay_frames != 3 || report.replay_records != 2 {
            return Err(format!(
                "manifest rename-fail row replayed {} frames / {} records, expected 3 / 2",
                report.replay_frames, report.replay_records
            ));
        }
        if report.active_offset_bytes == 0 {
            return Err(
                "manifest rename-fail row recovered writer at zero active offset".to_string()
            );
        }

        let id = "public_manifest_rename_fail_full_log_recovery".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "manifest_rename_fail");
        append_str(&mut manifest, "checkpoint_tail");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.checkpoint_records);
        append_bool(&mut manifest, report.manifest_present_after_recovery);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_manifest_dir_fsync_fail_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "dir_fsync_fail")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "checkpoint_tail")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_manifest_dir_fsync_fail_recovery_report(seed);
        if report.manifest_present_after_recovery {
            return Err(
                "manifest dir-fsync row loaded a MANIFEST after failed dir fsync".to_string()
            );
        }
        if report.checkpoint_records != 1 {
            return Err(format!(
                "manifest dir-fsync row published {} checkpoint records, expected 1",
                report.checkpoint_records
            ));
        }
        if report.replay_frames != 3 || report.replay_records != 2 {
            return Err(format!(
                "manifest dir-fsync row replayed {} frames / {} records, expected 3 / 2",
                report.replay_frames, report.replay_records
            ));
        }
        if report.active_offset_bytes == 0 {
            return Err("manifest dir-fsync row recovered writer at zero active offset".to_string());
        }

        let id = "public_manifest_dir_fsync_fail_full_log_recovery".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "dir_fsync_fail");
        append_str(&mut manifest, "checkpoint_tail");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.checkpoint_records);
        append_bool(&mut manifest, report.manifest_present_after_recovery);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_live_checkpoint_wait_dir_fsync_fail_row(
        row: &Value,
    ) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "dir_fsync_fail")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "live_checkpoint_command")?;
        require_eq(
            row,
            "process_runner",
            "inf-sim --scenario m2-live-checkpoint-dir-fsync-process-fail-stop --seed 0xD290",
        )?;
        require_eq(
            row,
            "process_test",
            "inf-sim::m2_crash_matrix::m2_live_checkpoint_dir_fsync_process_fail_stop_exits_nonzero",
        )?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_live_checkpoint_wait_dir_fsync_fail_report(seed);
        validate_live_checkpoint_dir_fsync_fail_report(&report)?;

        let id = "public_live_checkpoint_wait_dir_fsync_fail_no_reply".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "dir_fsync_fail");
        append_str(&mut manifest, "live_checkpoint_command");
        append_str(&mut manifest, required_string(row, "process_runner")?);
        append_u64(&mut manifest, report.seed);
        append_bool(&mut manifest, report.fail_stopped);
        append_u64(&mut manifest, report.reply_bytes_before_fail_stop);
        append_u64(&mut manifest, report.watermark_before_fail_stop);
        append_bool(&mut manifest, report.manifest_present_after_recovery);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_bool(&mut manifest, report.before_value_survived);
        append_bool(&mut manifest, report.tail_value_survived);
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_manifest_replacement_dir_fsync_fail_row(
        row: &Value,
    ) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "dir_fsync_fail")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "manifest_replacement")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_manifest_replacement_dir_fsync_fail_recovery_report(seed);
        validate_manifest_replacement_report(&report, "manifest replacement dir-fsync")?;

        let id = "public_manifest_replacement_dir_fsync_fail_preserves_old_manifest".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "dir_fsync_fail");
        append_str(&mut manifest, "manifest_replacement");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.old_checkpoint_records);
        append_u64(&mut manifest, report.new_checkpoint_records);
        append_bool(&mut manifest, report.loaded_old_manifest);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_checkpoint_write_enospc_row(row: &Value) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "checkpoint_write_enospc")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "manifest_replacement")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_checkpoint_write_enospc_recovery_report(seed);
        validate_checkpoint_write_enospc_report(&report)?;

        let id = "public_checkpoint_write_enospc_preserves_old_manifest".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "checkpoint_write_enospc");
        append_str(&mut manifest, "manifest_replacement");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.old_checkpoint_records);
        append_bool(&mut manifest, report.checkpoint_publish_failed_enospc);
        append_bool(&mut manifest, report.loaded_old_manifest);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        Ok(RowEvidence { id, manifest })
    }

    fn execute_public_manifest_replacement_rename_fail_row(
        row: &Value,
    ) -> Result<RowEvidence, String> {
        require_eq(row, "fault_point", "manifest_rename_fail")?;
        require_eq(row, "fsync_policy", "always")?;
        require_eq(row, "workload", "manifest_replacement")?;
        let seed = matrix_seed(row, "seed")?;
        let report = public_manifest_replacement_rename_fail_recovery_report(seed);
        validate_manifest_replacement_report(&report, "manifest replacement rename")?;

        let id = "public_manifest_replacement_rename_fail_preserves_old_manifest".to_string();
        let mut manifest = Vec::new();
        append_str(&mut manifest, &id);
        append_str(&mut manifest, "manifest_rename_fail");
        append_str(&mut manifest, "manifest_replacement");
        append_u64(&mut manifest, report.seed);
        append_u64(&mut manifest, report.old_checkpoint_records);
        append_u64(&mut manifest, report.new_checkpoint_records);
        append_bool(&mut manifest, report.loaded_old_manifest);
        append_u64(&mut manifest, report.replay_frames);
        append_u64(&mut manifest, report.replay_records);
        append_u64(&mut manifest, u64::from(report.active_offset_bytes));
        Ok(RowEvidence { id, manifest })
    }

    fn validate_checkpoint_write_enospc_report(
        report: &PublicCheckpointWriteEnospcReport,
    ) -> Result<(), String> {
        if !report.checkpoint_publish_failed_enospc {
            return Err("checkpoint ENOSPC row did not fail at checkpoint image write".to_string());
        }
        if !report.loaded_old_manifest {
            return Err("checkpoint ENOSPC row did not reload the old durable MANIFEST".to_string());
        }
        if report.old_checkpoint_records != 1 {
            return Err(format!(
                "checkpoint ENOSPC row published {} old checkpoint records, expected 1",
                report.old_checkpoint_records
            ));
        }
        if report.replay_frames != 3 || report.replay_records != 1 {
            return Err(format!(
                "checkpoint ENOSPC row replayed {} frames / {} records, expected 3 / 1",
                report.replay_frames, report.replay_records
            ));
        }
        if report.active_offset_bytes == 0 {
            return Err("checkpoint ENOSPC row recovered writer at zero active offset".to_string());
        }
        Ok(())
    }

    fn validate_manifest_replacement_report(
        report: &PublicManifestReplacementFailRecoveryReport,
        row_name: &str,
    ) -> Result<(), String> {
        if !report.loaded_old_manifest {
            return Err(format!("{row_name} row did not reload the old durable MANIFEST"));
        }
        if report.old_checkpoint_records != 1 || report.new_checkpoint_records != 1 {
            return Err(format!(
                "{row_name} row published {} old / {} new checkpoint records, expected 1 / 1",
                report.old_checkpoint_records, report.new_checkpoint_records
            ));
        }
        if report.replay_frames != 4 || report.replay_records != 2 {
            return Err(format!(
                "{row_name} row replayed {} frames / {} records, expected 4 / 2",
                report.replay_frames, report.replay_records
            ));
        }
        if report.active_offset_bytes == 0 {
            return Err(format!("{row_name} row recovered writer at zero active offset"));
        }
        Ok(())
    }

    fn validate_live_checkpoint_dir_fsync_fail_report(
        report: &PublicLiveCheckpointDirFsyncFailReport,
    ) -> Result<(), String> {
        if !report.fail_stopped {
            return Err("live checkpoint dir-fsync row did not fail-stop within the bounded drive"
                .to_string());
        }
        if !report.panic_message.contains("checkpoint directory")
            || !report.panic_message.contains("errno 5")
        {
            return Err(format!(
                "live checkpoint dir-fsync row fail-stopped for an unexpected reason: {}",
                report.panic_message
            ));
        }
        if report.reply_bytes_before_fail_stop != 0 {
            return Err(format!(
                "live checkpoint dir-fsync row emitted {} reply bytes before fail-stop",
                report.reply_bytes_before_fail_stop
            ));
        }
        if report.watermark_before_fail_stop == 0 {
            return Err("live checkpoint dir-fsync row fail-stopped before any durable watermark"
                .to_string());
        }
        if report.manifest_present_after_recovery {
            return Err("live checkpoint dir-fsync row loaded a MANIFEST after failed dir fsync"
                .to_string());
        }
        if report.replay_frames != 3 || report.replay_records != 2 {
            return Err(format!(
                "live checkpoint dir-fsync row replayed {} frames / {} records, expected 3 / 2",
                report.replay_frames, report.replay_records
            ));
        }
        if !report.before_value_survived || !report.tail_value_survived {
            return Err(format!(
                "live checkpoint dir-fsync row recovery visibility mismatch: before={} tail={}",
                report.before_value_survived, report.tail_value_survived
            ));
        }
        if report.active_offset_bytes == 0 {
            return Err(
                "live checkpoint dir-fsync row recovered writer at zero active offset".to_string()
            );
        }
        Ok(())
    }

    fn validate_torn_final_frame_report(
        report: &PublicTornFinalFrameRecoveryReport,
    ) -> Result<(), String> {
        if !report.torn_tail_truncated {
            return Err("torn-final row did not truncate to the stable prefix".to_string());
        }
        if !report.stable_value_survived {
            return Err("torn-final row lost the stable prefix value".to_string());
        }
        if !report.corrupt_value_absent {
            return Err("torn-final row replayed the corrupt tail value".to_string());
        }
        if report.replay_frames != 1 || report.replay_records != 1 {
            return Err(format!(
                "torn-final row replayed {} frames / {} records, expected 1 / 1",
                report.replay_frames, report.replay_records
            ));
        }
        if report.active_offset_bytes != report.stable_frame_end_bytes {
            return Err(format!(
                "torn-final row recovered offset {}, expected stable frame end {}",
                report.active_offset_bytes, report.stable_frame_end_bytes
            ));
        }
        if report.corrupt_frame_offset_bytes < report.stable_frame_end_bytes {
            return Err(format!(
                "torn-final row corrupt offset {} precedes stable frame end {}",
                report.corrupt_frame_offset_bytes, report.stable_frame_end_bytes
            ));
        }
        Ok(())
    }

    fn validate_active_tail_later_magic_report(
        report: &PublicActiveTailLaterMagicRecoveryReport,
    ) -> Result<(), String> {
        if !report.active_tail_truncated {
            return Err("active-tail later-magic row did not truncate the tail".to_string());
        }
        if !report.stable_value_survived {
            return Err("active-tail later-magic row lost the stable prefix value".to_string());
        }
        if !report.corrupt_value_absent {
            return Err("active-tail later-magic row replayed the corrupt tail value".to_string());
        }
        if !report.later_value_absent {
            return Err("active-tail later-magic row replayed the later tail value".to_string());
        }
        if report.replay_frames != 1 || report.replay_records != 1 {
            return Err(format!(
                "active-tail later-magic row replayed {} frames / {} records, expected 1 / 1",
                report.replay_frames, report.replay_records
            ));
        }
        if report.active_offset_bytes != report.stable_frame_end_bytes {
            return Err(format!(
                "active-tail later-magic row recovered offset {}, expected stable frame end {}",
                report.active_offset_bytes, report.stable_frame_end_bytes
            ));
        }
        if report.later_frame_offset_bytes <= report.corrupt_offset_bytes {
            return Err(format!(
                "active-tail later-magic row later frame offset {} is not after corrupt offset {}",
                report.later_frame_offset_bytes, report.corrupt_offset_bytes
            ));
        }
        Ok(())
    }

    fn validate_log_append_write_fault_fail_stop(
        report: &PublicLogAppendWriteFaultReport,
    ) -> Result<(), String> {
        if !report.fail_stopped {
            return Err(
                "log append write-fault row did not fail-stop within the bounded drive".to_string()
            );
        }
        if !report.panic_message.contains("file write") || !report.panic_message.contains("errno 5")
        {
            return Err(format!(
                "log append write-fault row fail-stopped for an unexpected reason: {}",
                report.panic_message
            ));
        }
        if report.reply_bytes_before_fail_stop != 0 {
            return Err(format!(
                "log append write-fault row emitted {} reply bytes before fail-stop",
                report.reply_bytes_before_fail_stop
            ));
        }
        if report.watermark_before_fail_stop != 0 {
            return Err(format!(
                "log append write-fault row advanced watermark {} before fail-stop",
                report.watermark_before_fail_stop
            ));
        }
        if report.replay_frames != 0 || report.replay_records != 0 {
            return Err(format!(
                "log append write-fault row replayed {} frames / {} records, expected 0 / 0",
                report.replay_frames, report.replay_records
            ));
        }
        if report.value_survived {
            return Err("log append write-fault row recovered the failed write".to_string());
        }
        Ok(())
    }

    fn validate_fsync_err_fail_stop(report: &PublicFsyncErrReport) -> Result<(), String> {
        if !report.fail_stopped {
            return Err("fsync_err row did not fail-stop within the bounded drive".to_string());
        }
        if !report.panic_message.contains("fdatasync") || !report.panic_message.contains("errno 5")
        {
            return Err(format!(
                "fsync_err row fail-stopped for an unexpected reason: {}",
                report.panic_message
            ));
        }
        if report.reply_bytes_before_fail_stop != 0 {
            return Err(format!(
                "fsync_err row emitted {} reply bytes before fail-stop",
                report.reply_bytes_before_fail_stop
            ));
        }
        if report.watermark_before_fail_stop != 0 {
            return Err(format!(
                "fsync_err row advanced watermark {} before fail-stop",
                report.watermark_before_fail_stop
            ));
        }
        Ok(())
    }

    fn validate_fsync_err_restart_watermark_fail_stop(
        report: &PublicFsyncErrRestartWatermarkReport,
    ) -> Result<(), String> {
        if !report.fail_stopped {
            return Err(
                "fsync_err restart-watermark row did not fail-stop within the bounded drive"
                    .to_string(),
            );
        }
        if !report.panic_message.contains("fdatasync") || !report.panic_message.contains("errno 5")
        {
            return Err(format!(
                "fsync_err restart-watermark row fail-stopped for an unexpected reason: {}",
                report.panic_message
            ));
        }
        if report.previous_watermark == 0 {
            return Err("fsync_err restart-watermark row had zero previous watermark".to_string());
        }
        if report.watermark_before_fail_stop != report.previous_watermark {
            return Err(format!(
                "fsync_err restart-watermark row changed watermark {} -> {} before fail-stop",
                report.previous_watermark, report.watermark_before_fail_stop
            ));
        }
        if report.reply_bytes_before_fail_stop != 0 {
            return Err(format!(
                "fsync_err restart-watermark row emitted {} reply bytes before fail-stop",
                report.reply_bytes_before_fail_stop
            ));
        }
        if report.replay_frames != 1 || report.replay_records != 1 {
            return Err(format!(
                "fsync_err restart-watermark row replayed {} frames / {} records, expected 1 / 1",
                report.replay_frames, report.replay_records
            ));
        }
        if !report.stable_value_survived {
            return Err("fsync_err restart-watermark row lost the prior durable value".to_string());
        }
        if !report.failed_value_absent {
            return Err("fsync_err restart-watermark row recovered the failed value".to_string());
        }
        if u64::from(report.active_offset_bytes) != report.previous_watermark {
            return Err(format!(
                "fsync_err restart-watermark row recovered writer offset {}, expected {}",
                report.active_offset_bytes, report.previous_watermark
            ));
        }
        Ok(())
    }

    struct PublicSimCell {
        net: Rc<std::cell::RefCell<CellNet>>,
        clock: Rc<VirtualClock>,
        cell_loop: CellLoop<SimDriver, Rc<VirtualClock>>,
        plane: ServerPlane,
    }

    struct PublicSimInstalls {
        writer: LogWriteIo,
        maintenance: Option<LogSegmentMaintenance>,
        publisher: Option<NamespaceCatalogLivePublisher>,
        checkpoint: Option<LiveCheckpointPublisher>,
    }

    impl PublicSimCell {
        fn first_boot(net: Rc<std::cell::RefCell<CellNet>>, seed: u64) -> PublicSimCell {
            PublicSimCell::first_boot_with_config(net, seed, &m2_log_config())
        }

        fn first_boot_with_config(
            net: Rc<std::cell::RefCell<CellNet>>,
            seed: u64,
            log_config: &LogDataRootConfig,
        ) -> PublicSimCell {
            let mut driver = SimDriver::new(Rc::clone(&net));
            let mut pool = BufferPool::new(128, 4096);
            let mut completions = Vec::new();
            let writer = open_first_boot_log_writer_in_data_root(
                &mut driver,
                &mut pool,
                log_config,
                &mut completions,
            )
            .expect("first boot public sim log writer");
            let log_dir = open_log_directory_in_data_root(
                &mut driver,
                &mut pool,
                log_config,
                &mut completions,
            )
            .expect("first boot public sim log maintenance directory");
            let maintenance = LogSegmentMaintenance::new(LogSegmentMaintenanceConfig::new(
                log_dir,
                M2_LOG_MAINTENANCE_TOKEN_SLOT,
            ));
            let publisher = NamespaceCatalogLivePublisher::new(m2_live_publish_config())
                .expect("public sim namespace catalog publisher");
            let checkpoint_dir = open_checkpoint_directory_in_data_root(
                &mut driver,
                &mut pool,
                log_config,
                &mut completions,
            )
            .expect("public sim checkpoint directory");
            let checkpoint = LiveCheckpointPublisher::new(LiveCheckpointPublishConfig::new(
                checkpoint_dir,
                M2_CHECKPOINT_IMAGE_TOKEN_SLOT,
            ));
            PublicSimCell::from_parts(
                net,
                seed,
                driver,
                pool,
                Keyspace::new(StoreConfig::default()),
                PublicSimInstalls {
                    writer,
                    maintenance: Some(maintenance),
                    publisher: Some(publisher),
                    checkpoint: Some(checkpoint),
                },
            )
        }

        fn recovered(
            net: Rc<std::cell::RefCell<CellNet>>,
            seed: u64,
            keyspace: Keyspace,
            writer: LogWriteIo,
        ) -> PublicSimCell {
            PublicSimCell::recovered_with_config(net, seed, keyspace, writer, &m2_log_config())
        }

        fn recovered_with_config(
            net: Rc<std::cell::RefCell<CellNet>>,
            seed: u64,
            keyspace: Keyspace,
            writer: LogWriteIo,
            log_config: &LogDataRootConfig,
        ) -> PublicSimCell {
            let mut driver = SimDriver::new(Rc::clone(&net));
            let mut pool = BufferPool::new(128, 4096);
            let mut completions = Vec::new();
            let log_dir = open_log_directory_in_data_root(
                &mut driver,
                &mut pool,
                log_config,
                &mut completions,
            )
            .expect("recovered public sim log maintenance directory");
            let maintenance = LogSegmentMaintenance::new(LogSegmentMaintenanceConfig::new(
                log_dir,
                M2_LOG_MAINTENANCE_TOKEN_SLOT,
            ));
            PublicSimCell::from_parts(
                net,
                seed,
                driver,
                pool,
                keyspace,
                PublicSimInstalls {
                    writer,
                    maintenance: Some(maintenance),
                    publisher: None,
                    checkpoint: None,
                },
            )
        }

        fn from_parts(
            net: Rc<std::cell::RefCell<CellNet>>,
            seed: u64,
            driver: SimDriver,
            pool: BufferPool,
            keyspace: Keyspace,
            installs: PublicSimInstalls,
        ) -> PublicSimCell {
            let clock = Rc::new(VirtualClock::new(Nanos(1)));
            let fabric = Mesh::new(1, MeshConfig { ring_capacity: 64, data_credits: 32 })
                .into_iter()
                .next()
                .expect("one fabric endpoint");
            let node = Rc::new(NodeInfo::default());
            node.rng_state.set(seed ^ 0xD2D2_D2D2);
            let mut plane = ServerPlane::new(
                CellId(0),
                1,
                listener_fd(0),
                keyspace,
                fabric,
                node,
                inf_server::NoopObserver,
                false,
            );
            plane.install_log_writer(installs.writer);
            if let Some(maintenance) = installs.maintenance {
                plane.install_log_segment_maintenance(maintenance);
            }
            if let Some(publisher) = installs.publisher {
                plane.install_namespace_catalog_publisher(publisher);
            }
            if let Some(checkpoint) = installs.checkpoint {
                plane.install_checkpoint_publisher(checkpoint);
            }
            let config = LoopConfig { spin_iters: 4, ..Default::default() };
            let cell_loop = CellLoop::new(driver, Rc::clone(&clock), pool, config);
            PublicSimCell { net, clock, cell_loop, plane }
        }

        fn connect(&self) -> RawFd {
            self.net.borrow_mut().connect()
        }

        fn request(&mut self, fd: RawFd, argv: &[&[u8]], expected: &[u8]) {
            self.net.borrow_mut().client_send(fd, &resp_command(argv));
            let got = self.run_until_reply(fd, expected.len());
            assert_eq!(got, expected);
        }

        fn request_at_least(&mut self, fd: RawFd, argv: &[&[u8]], min_len: usize) -> Vec<u8> {
            self.net.borrow_mut().client_send(fd, &resp_command(argv));
            self.run_until_reply(fd, min_len)
        }

        fn request_reply(&mut self, fd: RawFd, argv: &[&[u8]]) -> Vec<u8> {
            self.net.borrow_mut().client_send(fd, &resp_command(argv));
            let mut reply = Vec::new();
            for _ in 0..PUBLIC_SIM_MAX_ITERS {
                self.cell_loop.run_iteration(&mut self.plane).expect("public sim reply iteration");
                reply.extend(self.net.borrow_mut().client_recv(fd));
                if resp_reply_complete(&reply) {
                    return reply;
                }
                self.clock.advance(Nanos(1_000));
            }
            panic!("public sim request did not produce a complete RESP reply");
        }

        fn run_until_reply(&mut self, fd: RawFd, expected_len: usize) -> Vec<u8> {
            let mut reply = Vec::new();
            for _ in 0..PUBLIC_SIM_MAX_ITERS {
                self.cell_loop.run_iteration(&mut self.plane).expect("public sim iteration");
                reply.extend(self.net.borrow_mut().client_recv(fd));
                if reply.len() >= expected_len {
                    return reply;
                }
                self.clock.advance(Nanos(1_000));
            }
            panic!("public sim request did not complete within bounded iterations");
        }

        fn run_iterations(&mut self, count: usize) {
            for _ in 0..count {
                self.cell_loop.run_iteration(&mut self.plane).expect("public sim iteration");
                self.clock.advance(Nanos(1_000));
            }
        }

        fn run_until_fail_stop_or_bound(&mut self, fd: RawFd) -> PublicFailStopDriveReport {
            let mut reply_bytes = 0u64;
            for _ in 0..PUBLIC_SIM_MAX_ITERS {
                let iteration =
                    catch_expected_fail_stop(|| self.cell_loop.run_iteration(&mut self.plane));
                match iteration {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => panic!("public sim fsync_err iteration failed: {error:?}"),
                    Err(panic_message) => {
                        reply_bytes += self.net.borrow_mut().client_recv(fd).len() as u64;
                        return PublicFailStopDriveReport {
                            fail_stopped: true,
                            panic_message,
                            reply_bytes,
                            watermark: self.plane.durability_watermark(),
                        };
                    }
                }
                reply_bytes += self.net.borrow_mut().client_recv(fd).len() as u64;
                self.clock.advance(Nanos(1_000));
            }

            PublicFailStopDriveReport {
                fail_stopped: false,
                panic_message: String::new(),
                reply_bytes,
                watermark: self.plane.durability_watermark(),
            }
        }

        fn run_until_fail_stop_after_stable_path(
            &mut self,
            fd: RawFd,
            stable_path: &str,
        ) -> PublicFailStopDriveReport {
            let mut reply_bytes = 0u64;
            let mut armed = false;
            for _ in 0..PUBLIC_SIM_MAX_ITERS {
                let iteration =
                    catch_expected_fail_stop(|| self.cell_loop.run_iteration(&mut self.plane));
                match iteration {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        panic!("public sim live checkpoint iteration failed: {error:?}");
                    }
                    Err(panic_message) => {
                        reply_bytes += self.net.borrow_mut().client_recv(fd).len() as u64;
                        return PublicFailStopDriveReport {
                            fail_stopped: true,
                            panic_message,
                            reply_bytes,
                            watermark: self.plane.durability_watermark(),
                        };
                    }
                }
                reply_bytes += self.net.borrow_mut().client_recv(fd).len() as u64;
                if !armed && self.net.borrow().stable_paths.contains_key(stable_path) {
                    self.net.borrow_mut().fail_next_file_sync(FileSyncMode::Full, libc::EIO);
                    armed = true;
                }
                self.clock.advance(Nanos(1_000));
            }

            PublicFailStopDriveReport {
                fail_stopped: false,
                panic_message: String::new(),
                reply_bytes,
                watermark: self.plane.durability_watermark(),
            }
        }

        fn run_until_process_fail_stop(&mut self, fd: RawFd, label: &str) {
            for _ in 0..PUBLIC_SIM_MAX_ITERS {
                self.cell_loop
                    .run_iteration(&mut self.plane)
                    .expect("public sim fail-stop process iteration");
                let reply_bytes = self.net.borrow_mut().client_recv(fd).len();
                assert_eq!(
                    reply_bytes, 0,
                    "{label} process row emitted {reply_bytes} reply bytes before fail-stop"
                );
                self.clock.advance(Nanos(1_000));
            }
            panic!("public sim {label} did not fail-stop within bounded iterations");
        }

        fn run_until_process_fail_stop_after_stable_path(
            &mut self,
            fd: RawFd,
            label: &str,
            stable_path: &str,
        ) {
            let mut armed = false;
            for _ in 0..PUBLIC_SIM_MAX_ITERS {
                self.cell_loop
                    .run_iteration(&mut self.plane)
                    .expect("public sim fail-stop process iteration");
                let reply_bytes = self.net.borrow_mut().client_recv(fd).len();
                assert_eq!(
                    reply_bytes, 0,
                    "{label} process row emitted {reply_bytes} reply bytes before fail-stop"
                );
                if !armed && self.net.borrow().stable_paths.contains_key(stable_path) {
                    self.net.borrow_mut().fail_next_file_sync(FileSyncMode::Full, libc::EIO);
                    armed = true;
                }
                self.clock.advance(Nanos(1_000));
            }
            panic!("public sim {label} did not fail-stop within bounded iterations");
        }

        fn drive_everysec_timer_fsync(&mut self) {
            self.clock.advance(Nanos::from_secs(1) + Nanos::from_millis(1));
            for _ in 0..PUBLIC_SIM_MAX_ITERS {
                self.cell_loop.run_iteration(&mut self.plane).expect("public sim iteration");
                if self.plane.durability_watermark() > 0 {
                    return;
                }
                self.clock.advance(Nanos(1_000));
            }
            panic!("public sim everysec timer did not advance the durability watermark");
        }

        fn close(&mut self, fd: RawFd) {
            self.net.borrow_mut().client_close(fd);
            for _ in 0..64 {
                self.cell_loop.run_iteration(&mut self.plane).expect("public sim close iteration");
                if self.net.borrow().closed(fd) {
                    return;
                }
                self.clock.advance(Nanos(1_000));
            }
            panic!("public sim connection did not close within bounded iterations");
        }
    }

    fn catch_expected_fail_stop<F, R>(f: F) -> Result<R, String>
    where
        F: FnOnce() -> R,
    {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = catch_unwind(AssertUnwindSafe(f));
        std::panic::set_hook(previous);
        outcome.map_err(|payload| panic_payload(&*payload))
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    struct PublicEverysecRecoveryReport {
        seed: u64,
        replay_frames: u64,
        replay_records: u64,
        value_survived: bool,
        watermark_at_cut: u64,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    struct PublicAlwaysRecoveryReport {
        seed: u64,
        replay_frames: u64,
        replay_records: u64,
        value_survived: bool,
        watermark_at_cut: u64,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicAlwaysBatchedPipelineReport {
        pub(super) seed: u64,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) survived_keys: u64,
        pub(super) watermark_at_cut: u64,
    }

    struct PublicFailStopDriveReport {
        fail_stopped: bool,
        panic_message: String,
        reply_bytes: u64,
        watermark: u64,
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicFsyncErrReport {
        pub(super) seed: u64,
        pub(super) power_cut_seed: u64,
        pub(super) fail_stopped: bool,
        pub(super) panic_message: String,
        pub(super) reply_bytes_before_fail_stop: u64,
        pub(super) watermark_before_fail_stop: u64,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) value_survived: bool,
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicFsyncErrRestartWatermarkReport {
        pub(super) seed: u64,
        pub(super) power_cut_seed: u64,
        pub(super) fail_stopped: bool,
        pub(super) panic_message: String,
        pub(super) previous_watermark: u64,
        pub(super) watermark_before_fail_stop: u64,
        pub(super) reply_bytes_before_fail_stop: u64,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) stable_value_survived: bool,
        pub(super) failed_value_absent: bool,
        pub(super) active_offset_bytes: u32,
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicLogAppendWriteFaultReport {
        pub(super) seed: u64,
        pub(super) power_cut_seed: u64,
        pub(super) fail_stopped: bool,
        pub(super) panic_message: String,
        pub(super) reply_bytes_before_fail_stop: u64,
        pub(super) watermark_before_fail_stop: u64,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) value_survived: bool,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicSealRecoveryReport {
        pub(super) seed: u64,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) active_segment: u32,
        pub(super) active_offset_bytes: u32,
        pub(super) sealed_segment_len_bytes: u64,
        pub(super) survived_keys: u64,
        pub(super) watermark_at_cut: u64,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicTornFinalFrameRecoveryReport {
        pub(super) seed: u64,
        pub(super) stable_frame_end_bytes: u32,
        pub(super) corrupt_frame_offset_bytes: u32,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) active_offset_bytes: u32,
        pub(super) torn_tail_truncated: bool,
        pub(super) stable_value_survived: bool,
        pub(super) corrupt_value_absent: bool,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicActiveTailLaterMagicRecoveryReport {
        pub(super) seed: u64,
        pub(super) stable_frame_end_bytes: u32,
        pub(super) corrupt_offset_bytes: u32,
        pub(super) later_frame_offset_bytes: u32,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) active_offset_bytes: u32,
        pub(super) active_tail_truncated: bool,
        pub(super) stable_value_survived: bool,
        pub(super) corrupt_value_absent: bool,
        pub(super) later_value_absent: bool,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicManifestRecoveryReport {
        pub(super) seed: u64,
        pub(super) checkpoint_records: u64,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) active_offset_bytes: u32,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicManifestRenameFailRecoveryReport {
        pub(super) seed: u64,
        pub(super) checkpoint_records: u64,
        pub(super) manifest_present_after_recovery: bool,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) active_offset_bytes: u32,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicManifestDirFsyncFailRecoveryReport {
        pub(super) seed: u64,
        pub(super) checkpoint_records: u64,
        pub(super) manifest_present_after_recovery: bool,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) active_offset_bytes: u32,
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicLiveCheckpointDirFsyncFailReport {
        pub(super) seed: u64,
        pub(super) fail_stopped: bool,
        pub(super) panic_message: String,
        pub(super) reply_bytes_before_fail_stop: u64,
        pub(super) watermark_before_fail_stop: u64,
        pub(super) manifest_present_after_recovery: bool,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) before_value_survived: bool,
        pub(super) tail_value_survived: bool,
        pub(super) active_offset_bytes: u32,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicManifestReplacementFailRecoveryReport {
        pub(super) seed: u64,
        pub(super) old_checkpoint_records: u64,
        pub(super) new_checkpoint_records: u64,
        pub(super) loaded_old_manifest: bool,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) active_offset_bytes: u32,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicCheckpointWriteEnospcReport {
        pub(super) seed: u64,
        pub(super) old_checkpoint_records: u64,
        pub(super) checkpoint_publish_failed_enospc: bool,
        pub(super) loaded_old_manifest: bool,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) active_offset_bytes: u32,
    }

    #[cfg(test)]
    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub(super) struct PublicPreallocateEnospcReport {
        pub(super) seed: u64,
        pub(super) durable_refused: bool,
        pub(super) memory_served_after_degrade: bool,
        pub(super) replay_frames: u64,
        pub(super) replay_records: u64,
        pub(super) durable_value_survived: bool,
        pub(super) refused_value_absent: bool,
        pub(super) memory_value_absent_after_restart: bool,
        pub(super) watermark_at_degrade: u64,
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    pub struct PublicDurableRecoverySweepReport {
        pub seed: u64,
        pub seed_offset: u64,
        pub seed_stride: u64,
        pub seeds: u64,
        pub writes_per_seed: u64,
        pub key_space: u64,
        pub manifest: Vec<u8>,
        pub manifest_hash: u64,
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    pub struct PublicEverysecRecoverySweepReport {
        pub seed: u64,
        pub seed_offset: u64,
        pub seed_stride: u64,
        pub seeds: u64,
        pub pre_timer_loss_cases: u64,
        pub pre_timer_survival_cases: u64,
        pub post_timer_survival_cases: u64,
        pub manifest: Vec<u8>,
        pub manifest_hash: u64,
    }

    impl PublicEverysecRecoverySweepReport {
        pub fn ok(&self) -> bool {
            self.pre_timer_loss_cases > 0
                && self.pre_timer_loss_cases + self.pre_timer_survival_cases == self.seeds
                && self.post_timer_survival_cases == self.seeds
        }
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    pub struct PublicEverysecWorkloadSweepReport {
        pub seed: u64,
        pub seed_offset: u64,
        pub seed_stride: u64,
        pub seeds: u64,
        pub writes_per_seed: u64,
        pub key_space: u64,
        pub loss_window_truncated_cases: u64,
        pub loss_window_full_survival_cases: u64,
        pub full_flush_survival_cases: u64,
        pub manifest: Vec<u8>,
        pub manifest_hash: u64,
    }

    impl PublicEverysecWorkloadSweepReport {
        pub fn ok(&self) -> bool {
            self.loss_window_truncated_cases > 0 && self.full_flush_survival_cases == self.seeds
        }
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    struct PublicDurableSeedReport {
        seed: u64,
        mutation_records: u64,
        replay_frames: u64,
        replay_records: u64,
        expected_digest: u64,
        recovered_digest: u64,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    struct PublicEverysecSeedReport {
        seed: u64,
        pre_timer_replay_frames: u64,
        pre_timer_replay_records: u64,
        pre_timer_watermark: u64,
        pre_timer_value_survived: bool,
        post_timer_replay_frames: u64,
        post_timer_replay_records: u64,
        post_timer_watermark: u64,
        post_timer_value_survived: bool,
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    struct PublicEverysecWorkloadOp {
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    struct PublicEverysecWorkloadSeedReport {
        seed: u64,
        flush_after_commands: u64,
        cut_idle_iters: u64,
        recovered_prefix_commands: u64,
        loss_replay_frames: u64,
        loss_replay_records: u64,
        loss_watermark: u64,
        loss_digest: u64,
        expected_digest: u64,
        full_replay_frames: u64,
        full_replay_records: u64,
        full_watermark: u64,
        full_digest: u64,
    }

    struct PublicEverysecWorkloadCaseReport {
        replay_frames: u64,
        replay_records: u64,
        watermark_at_cut: u64,
        recovered_state: BTreeMap<Vec<u8>, Vec<u8>>,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    enum ManifestReplacementFailure {
        Rename,
        DirFsync,
    }

    fn public_always_recovery_report(seed: u64) -> PublicAlwaysRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let watermark_at_cut;
        {
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
            cell.request(fd, &[b"SET", b"order:1", b"paid"], b"+OK\r\n");
            watermark_at_cut = cell.plane.durability_watermark();
            assert!(watermark_at_cut > 0);
            cell.close(fd);
        }

        net.borrow_mut().power_cut(seed ^ 0xA11C_E048);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public always namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"ledger"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Always)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered.ns_replace_with_recovered_catalog(loaded).expect("install recovered catalog");
        let catalog = first_boot_segment_catalog();
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("public always active-tail replay after sim power cut");
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        let got = cell.request_at_least(fd, &[b"GET", b"order:1"], b"$-1\r\n".len());
        let value_survived = if got == b"$-1\r\n" {
            false
        } else {
            assert_eq!(got, bulk_reply(b"paid"));
            true
        };
        cell.close(fd);

        PublicAlwaysRecoveryReport {
            seed,
            replay_frames: replay.frames,
            replay_records: replay.records,
            value_survived,
            watermark_at_cut,
        }
    }

    pub(super) fn public_always_batched_pipeline_recovery_report(
        seed: u64,
    ) -> PublicAlwaysBatchedPipelineReport {
        let net = CellNet::new(0, seed, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let mut writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("first boot log writer for batched-pipeline runner row");
        let catalog = durable_namespace_catalog();
        publish_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &catalog,
            &NamespaceCatalogDataRootPublishConfig::new(
                M2_RECOVERY_DATA_ROOT.to_string(),
                M2_RECOVERY_NS_CATALOG_TOKEN_SLOT,
            )
            .with_max_reaps(8),
            &mut completions,
        )
        .expect("publish namespace catalog for batched-pipeline runner row");

        let mut durability = DurabilityCell::with_capacity(1024).unwrap();
        durability
            .stage_mutation_effect(
                LogNamespaceId::new(16),
                MutationEffect::StringPostImage {
                    key: b"batch:1",
                    value: b"one",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage first batched-pipeline mutation");
        durability
            .stage_mutation_effect(
                LogNamespaceId::new(16),
                MutationEffect::StringPostImage {
                    key: b"batch:2",
                    value: b"two",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage second batched-pipeline mutation");
        let durable = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);
        assert_eq!(durable.record_count(), 2);
        assert!(completions.is_empty());

        net.borrow_mut().power_cut(seed ^ 0xA11C_BA7C);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public batched-pipeline namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"ledger"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Always)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install public batched-pipeline recovered namespace catalog");
        let catalog = first_boot_segment_catalog();
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("public batched-pipeline active-tail replay after sim power cut");
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        let mut survived_keys = 0;
        let got = cell.request_at_least(fd, &[b"GET", b"batch:1"], b"$-1\r\n".len());
        if got == bulk_reply(b"one") {
            survived_keys += 1;
        } else {
            assert_eq!(got, b"$-1\r\n");
        }
        let got = cell.request_at_least(fd, &[b"GET", b"batch:2"], b"$-1\r\n".len());
        if got == bulk_reply(b"two") {
            survived_keys += 1;
        } else {
            assert_eq!(got, b"$-1\r\n");
        }
        cell.close(fd);

        PublicAlwaysBatchedPipelineReport {
            seed,
            replay_frames: replay.frames,
            replay_records: replay.records,
            survived_keys,
            watermark_at_cut: u64::from(durable.frame_end().offset()),
        }
    }

    pub fn run_public_durable_recovery_sweep(
        config: &DurabilitySweepConfig,
    ) -> PublicDurableRecoverySweepReport {
        assert!(config.seeds > 0);
        assert!(config.seed_stride > 0);
        assert!(config.writes_per_seed > 0);
        assert!(config.key_space > 0);

        let mut manifest = Vec::new();
        for offset in 0..config.seeds {
            let seed = config
                .seed
                .wrapping_add(config.seed_offset)
                .wrapping_add(offset.wrapping_mul(config.seed_stride));
            let report = public_durable_seed_report(seed, config.writes_per_seed, config.key_space);
            append_u64(&mut manifest, report.seed);
            append_u64(&mut manifest, report.mutation_records);
            append_u64(&mut manifest, report.replay_frames);
            append_u64(&mut manifest, report.replay_records);
            append_u64(&mut manifest, report.expected_digest);
            append_u64(&mut manifest, report.recovered_digest);
        }

        PublicDurableRecoverySweepReport {
            seed: config.seed,
            seed_offset: config.seed_offset,
            seed_stride: config.seed_stride,
            seeds: config.seeds,
            writes_per_seed: config.writes_per_seed,
            key_space: config.key_space,
            manifest_hash: hash64(&manifest, PUBLIC_RECOVERY_SWEEP_HASH_SEED),
            manifest,
        }
    }

    pub fn run_public_everysec_recovery_sweep(
        config: &DurabilitySweepConfig,
    ) -> PublicEverysecRecoverySweepReport {
        assert!(config.seeds > 0);
        assert!(config.seed_stride > 0);

        let mut manifest = Vec::new();
        let mut pre_timer_loss_cases = 0;
        let mut pre_timer_survival_cases = 0;
        let mut post_timer_survival_cases = 0;
        for offset in 0..config.seeds {
            let seed = config
                .seed
                .wrapping_add(config.seed_offset)
                .wrapping_add(offset.wrapping_mul(config.seed_stride));
            let report = public_everysec_seed_report(seed);
            pre_timer_loss_cases += u64::from(!report.pre_timer_value_survived);
            pre_timer_survival_cases += u64::from(report.pre_timer_value_survived);
            post_timer_survival_cases += u64::from(report.post_timer_value_survived);

            append_u64(&mut manifest, report.seed);
            append_u64(&mut manifest, report.pre_timer_replay_frames);
            append_u64(&mut manifest, report.pre_timer_replay_records);
            append_u64(&mut manifest, report.pre_timer_watermark);
            append_bool(&mut manifest, report.pre_timer_value_survived);
            append_u64(&mut manifest, report.post_timer_replay_frames);
            append_u64(&mut manifest, report.post_timer_replay_records);
            append_u64(&mut manifest, report.post_timer_watermark);
            append_bool(&mut manifest, report.post_timer_value_survived);
        }

        PublicEverysecRecoverySweepReport {
            seed: config.seed,
            seed_offset: config.seed_offset,
            seed_stride: config.seed_stride,
            seeds: config.seeds,
            pre_timer_loss_cases,
            pre_timer_survival_cases,
            post_timer_survival_cases,
            manifest_hash: hash64(&manifest, PUBLIC_EVERYSEC_SWEEP_HASH_SEED),
            manifest,
        }
    }

    pub fn run_public_everysec_workload_sweep(
        config: &DurabilitySweepConfig,
    ) -> PublicEverysecWorkloadSweepReport {
        assert!(config.seeds > 0);
        assert!(config.seed_stride > 0);
        assert!(config.writes_per_seed > 1);
        assert!(config.key_space > 0);

        let mut manifest = Vec::new();
        let mut loss_window_truncated_cases = 0;
        let mut loss_window_full_survival_cases = 0;
        let mut full_flush_survival_cases = 0;
        for offset in 0..config.seeds {
            let seed = config
                .seed
                .wrapping_add(config.seed_offset)
                .wrapping_add(offset.wrapping_mul(config.seed_stride));
            let report = public_everysec_workload_seed_report(
                seed,
                config.writes_per_seed,
                config.key_space,
            );
            loss_window_truncated_cases +=
                u64::from(report.recovered_prefix_commands < config.writes_per_seed);
            loss_window_full_survival_cases +=
                u64::from(report.recovered_prefix_commands == config.writes_per_seed);
            full_flush_survival_cases += u64::from(report.full_digest == report.expected_digest);

            append_u64(&mut manifest, report.seed);
            append_u64(&mut manifest, report.flush_after_commands);
            append_u64(&mut manifest, report.cut_idle_iters);
            append_u64(&mut manifest, report.recovered_prefix_commands);
            append_u64(&mut manifest, report.loss_replay_frames);
            append_u64(&mut manifest, report.loss_replay_records);
            append_u64(&mut manifest, report.loss_watermark);
            append_u64(&mut manifest, report.loss_digest);
            append_u64(&mut manifest, report.expected_digest);
            append_u64(&mut manifest, report.full_replay_frames);
            append_u64(&mut manifest, report.full_replay_records);
            append_u64(&mut manifest, report.full_watermark);
            append_u64(&mut manifest, report.full_digest);
        }

        PublicEverysecWorkloadSweepReport {
            seed: config.seed,
            seed_offset: config.seed_offset,
            seed_stride: config.seed_stride,
            seeds: config.seeds,
            writes_per_seed: config.writes_per_seed,
            key_space: config.key_space,
            loss_window_truncated_cases,
            loss_window_full_survival_cases,
            full_flush_survival_cases,
            manifest_hash: hash64(&manifest, PUBLIC_EVERYSEC_WORKLOAD_SWEEP_HASH_SEED),
            manifest,
        }
    }

    fn public_everysec_workload_seed_report(
        seed: u64,
        writes_per_seed: u64,
        key_space: u64,
    ) -> PublicEverysecWorkloadSeedReport {
        assert!(writes_per_seed > 1);
        assert!(key_space > 0);

        let mut rng = SplitMix64::new(seed ^ PUBLIC_EVERYSEC_WORKLOAD_SWEEP_HASH_SEED);
        let ops = public_everysec_workload_ops(&mut rng, seed, writes_per_seed, key_space);
        let prefix_states = public_everysec_prefix_states(&ops);
        let flush_after_commands = 1 + rng.next_below(writes_per_seed - 1);
        let cut_idle_iters = rng.next_below(4);

        let loss_case = public_everysec_workload_case(
            seed,
            &ops,
            key_space,
            flush_after_commands,
            cut_idle_iters,
            seed ^ rng.next_u64(),
        );
        let loss_digest = state_digest(&loss_case.recovered_state);
        let recovered_prefix_commands =
            public_everysec_recovered_prefix(&prefix_states, &loss_case.recovered_state)
                .expect("loss-window recovered state must match a command prefix");
        assert!(
            recovered_prefix_commands >= flush_after_commands,
            "seed {seed:#x} recovered prefix {recovered_prefix_commands} before flushed prefix \
             {flush_after_commands}; replayed {} frames / {} records, watermark {}",
            loss_case.replay_frames,
            loss_case.replay_records,
            loss_case.watermark_at_cut
        );
        assert!(recovered_prefix_commands <= writes_per_seed);
        assert!(
            loss_case.replay_records >= flush_after_commands,
            "seed {seed:#x} replayed {} records before flushed prefix {flush_after_commands}",
            loss_case.replay_records
        );
        assert!(
            loss_case.replay_records <= recovered_prefix_commands,
            "seed {seed:#x} replayed {} records but latest equivalent prefix is {}",
            loss_case.replay_records,
            recovered_prefix_commands
        );

        let full_case = public_everysec_workload_case(
            seed ^ 0xF011_F105_0000_0000,
            &ops,
            key_space,
            writes_per_seed,
            0,
            seed ^ rng.next_u64(),
        );
        let expected = prefix_states.last().expect("prefix state list is never empty");
        assert_eq!(&full_case.recovered_state, expected);
        assert_eq!(full_case.replay_records, writes_per_seed);
        assert!(full_case.watermark_at_cut > 0);

        PublicEverysecWorkloadSeedReport {
            seed,
            flush_after_commands,
            cut_idle_iters,
            recovered_prefix_commands,
            loss_replay_frames: loss_case.replay_frames,
            loss_replay_records: loss_case.replay_records,
            loss_watermark: loss_case.watermark_at_cut,
            loss_digest,
            expected_digest: state_digest(expected),
            full_replay_frames: full_case.replay_frames,
            full_replay_records: full_case.replay_records,
            full_watermark: full_case.watermark_at_cut,
            full_digest: state_digest(&full_case.recovered_state),
        }
    }

    fn public_everysec_workload_ops(
        rng: &mut SplitMix64,
        seed: u64,
        writes_per_seed: u64,
        key_space: u64,
    ) -> Vec<PublicEverysecWorkloadOp> {
        let mut state: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut ops = Vec::new();
        for step in 0..writes_per_seed {
            if !state.is_empty() && rng.next_u64().is_multiple_of(5) {
                let key_index = rng.next_below(state.len() as u64) as usize;
                let key = state.keys().nth(key_index).expect("existing key").clone();
                state.remove(&key);
                ops.push(PublicEverysecWorkloadOp { key, value: None });
            } else {
                let key = format!("k:{}", rng.next_below(key_space)).into_bytes();
                let value = format!("v:{seed:016x}:{step:04}").into_bytes();
                state.insert(key.clone(), value.clone());
                ops.push(PublicEverysecWorkloadOp { key, value: Some(value) });
            }
        }
        ops
    }

    fn public_everysec_prefix_states(
        ops: &[PublicEverysecWorkloadOp],
    ) -> Vec<BTreeMap<Vec<u8>, Vec<u8>>> {
        let mut state = BTreeMap::new();
        let mut states = Vec::with_capacity(ops.len() + 1);
        states.push(state.clone());
        for op in ops {
            match &op.value {
                Some(value) => {
                    state.insert(op.key.clone(), value.clone());
                }
                None => {
                    assert!(state.remove(&op.key).is_some());
                }
            }
            states.push(state.clone());
        }
        states
    }

    fn public_everysec_workload_case(
        seed: u64,
        ops: &[PublicEverysecWorkloadOp],
        key_space: u64,
        flush_after_commands: u64,
        cut_idle_iters: u64,
        power_cut_seed: u64,
    ) -> PublicEverysecWorkloadCaseReport {
        assert!(!ops.is_empty());
        assert!(flush_after_commands > 0);
        assert!(flush_after_commands <= ops.len() as u64);

        let net = CellNet::new(0, seed, Plant::None);
        let watermark_at_cut;
        {
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"sessions", b"MODE", b"durable", b"FSYNC", b"everysec"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"sessions"], b"+OK\r\n");
            for (index, op) in ops.iter().enumerate() {
                public_everysec_apply_op(&mut cell, fd, op);
                cell.run_iterations(4);
                if index + 1 == flush_after_commands as usize {
                    cell.drive_everysec_timer_fsync();
                }
            }
            cell.run_iterations(cut_idle_iters as usize);
            watermark_at_cut = cell.plane.durability_watermark();
            assert!(watermark_at_cut > 0);
            cell.close(fd);
        }

        net.borrow_mut().power_cut(power_cut_seed);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public everysec workload namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"sessions"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Everysec)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install public everysec workload recovered namespace catalog");
        let catalog = first_boot_segment_catalog();
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("public everysec workload active-tail replay after sim power cut");
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"sessions"], b"+OK\r\n");
        let recovered_state = public_keyspace_state(&mut cell, fd, key_space);
        cell.close(fd);

        PublicEverysecWorkloadCaseReport {
            replay_frames: replay.frames,
            replay_records: replay.records,
            watermark_at_cut,
            recovered_state,
        }
    }

    fn public_everysec_apply_op(
        cell: &mut PublicSimCell,
        fd: RawFd,
        op: &PublicEverysecWorkloadOp,
    ) {
        match &op.value {
            Some(value) => {
                cell.request(fd, &[b"SET", op.key.as_slice(), value.as_slice()], b"+OK\r\n")
            }
            None => cell.request(fd, &[b"DEL", op.key.as_slice()], b":1\r\n"),
        }
    }

    fn public_everysec_recovered_prefix(
        states: &[BTreeMap<Vec<u8>, Vec<u8>>],
        recovered: &BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Option<u64> {
        states.iter().rposition(|state| state == recovered).map(|index| index as u64)
    }

    fn public_everysec_seed_report(seed: u64) -> PublicEverysecSeedReport {
        let pre_timer = public_everysec_recovery_report(seed, false);
        assert_eq!(pre_timer.watermark_at_cut, 0);
        if pre_timer.value_survived {
            assert_eq!(pre_timer.replay_frames, 1);
            assert_eq!(pre_timer.replay_records, 1);
        } else {
            assert_eq!(pre_timer.replay_frames, 0);
            assert_eq!(pre_timer.replay_records, 0);
        }

        let post_timer = public_everysec_recovery_report(seed, true);
        assert!(post_timer.watermark_at_cut > 0);
        assert!(post_timer.value_survived);
        assert_eq!(post_timer.replay_frames, 1);
        assert_eq!(post_timer.replay_records, 1);

        PublicEverysecSeedReport {
            seed,
            pre_timer_replay_frames: pre_timer.replay_frames,
            pre_timer_replay_records: pre_timer.replay_records,
            pre_timer_watermark: pre_timer.watermark_at_cut,
            pre_timer_value_survived: pre_timer.value_survived,
            post_timer_replay_frames: post_timer.replay_frames,
            post_timer_replay_records: post_timer.replay_records,
            post_timer_watermark: post_timer.watermark_at_cut,
            post_timer_value_survived: post_timer.value_survived,
        }
    }

    fn public_durable_seed_report(
        seed: u64,
        writes_per_seed: u64,
        key_space: u64,
    ) -> PublicDurableSeedReport {
        let net = CellNet::new(0, seed, Plant::None);
        let mut expected = BTreeMap::new();
        let mut mutation_records = 0u64;
        {
            let mut rng = SplitMix64::new(seed ^ PUBLIC_RECOVERY_SWEEP_HASH_SEED);
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
            for step in 0..writes_per_seed {
                let key = format!("k:{}", rng.next_below(key_space)).into_bytes();
                if rng.next_u64().is_multiple_of(5) {
                    let existed = expected.remove(&key).is_some();
                    let want = if existed { b":1\r\n".as_slice() } else { b":0\r\n".as_slice() };
                    cell.request(fd, &[b"DEL", key.as_slice()], want);
                    mutation_records += u64::from(existed);
                } else {
                    let value = format!("v:{seed:016x}:{step:04}").into_bytes();
                    cell.request(fd, &[b"SET", key.as_slice(), value.as_slice()], b"+OK\r\n");
                    expected.insert(key, value);
                    mutation_records += 1;
                }
            }
            assert!(cell.plane.durability_watermark() > 0);
            cell.close(fd);
        }

        net.borrow_mut().power_cut(seed ^ 0xA11C_E045);

        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public sweep namespace catalog after sim power cut");
        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install public sweep recovered namespace catalog");
        let catalog = first_boot_segment_catalog();
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("public sweep active-tail replay after sim power cut");
        assert_eq!(replay.records, mutation_records);
        assert_eq!(pool.reconcile(), Ok(()));

        let mut actual = BTreeMap::new();
        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        for key_idx in 0..key_space {
            let key = format!("k:{key_idx}").into_bytes();
            match expected.get(&key) {
                Some(value) => {
                    let want = bulk_reply(value);
                    cell.request(fd, &[b"GET", key.as_slice()], &want);
                    actual.insert(key, value.clone());
                }
                None => cell.request(fd, &[b"GET", key.as_slice()], b"$-1\r\n"),
            }
        }
        cell.close(fd);

        let expected_digest = state_digest(&expected);
        let recovered_digest = state_digest(&actual);
        assert_eq!(recovered_digest, expected_digest);
        PublicDurableSeedReport {
            seed,
            mutation_records,
            replay_frames: replay.frames,
            replay_records: replay.records,
            expected_digest,
            recovered_digest,
        }
    }

    fn public_everysec_recovery_report(
        seed: u64,
        flush_timer: bool,
    ) -> PublicEverysecRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let mut watermark_at_cut = 0;
        {
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"sessions", b"MODE", b"durable", b"FSYNC", b"everysec"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"sessions"], b"+OK\r\n");
            cell.request(fd, &[b"SET", b"session:1", b"alive"], b"+OK\r\n");
            assert_eq!(cell.plane.durability_watermark(), 0);

            cell.run_iterations(1);
            assert_eq!(cell.plane.durability_watermark(), 0);
            if flush_timer {
                cell.drive_everysec_timer_fsync();
                watermark_at_cut = cell.plane.durability_watermark();
                assert!(watermark_at_cut > 0);
            }
        }

        net.borrow_mut().power_cut(seed ^ 0xA11C_E046);

        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public everysec namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"sessions"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Everysec)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install public everysec recovered namespace catalog");
        let catalog = first_boot_segment_catalog();
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("public everysec active-tail replay after sim power cut");
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"sessions"], b"+OK\r\n");
        let got = cell.request_at_least(fd, &[b"GET", b"session:1"], b"$-1\r\n".len());
        let value_survived = if got == b"$-1\r\n" {
            false
        } else {
            assert_eq!(got, bulk_reply(b"alive"));
            true
        };
        cell.close(fd);

        PublicEverysecRecoveryReport {
            seed,
            replay_frames: replay.frames,
            replay_records: replay.records,
            value_survived,
            watermark_at_cut,
        }
    }

    pub(super) fn public_fsync_err_report(seed: u64, power_cut_seed: u64) -> PublicFsyncErrReport {
        let net = CellNet::new(0, seed, Plant::None);
        let drive;
        {
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
            net.borrow_mut().fail_next_file_op(SimFileOpKind::Sync, libc::EIO);
            net.borrow_mut().client_send(fd, &resp_command(&[b"SET", b"order:1", b"paid"]));
            drive = cell.run_until_fail_stop_or_bound(fd);
        }

        net.borrow_mut().power_cut(power_cut_seed);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public fsync_err namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"ledger"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Always)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install public fsync_err recovered namespace catalog");
        let catalog = first_boot_segment_catalog();
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("public fsync_err active-tail replay after sim power cut");
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        let got = cell.request_at_least(fd, &[b"GET", b"order:1"], b"$-1\r\n".len());
        let value_survived = if got == b"$-1\r\n" {
            false
        } else {
            assert_eq!(got, bulk_reply(b"paid"));
            true
        };
        cell.close(fd);

        PublicFsyncErrReport {
            seed,
            power_cut_seed,
            fail_stopped: drive.fail_stopped,
            panic_message: drive.panic_message,
            reply_bytes_before_fail_stop: drive.reply_bytes,
            watermark_before_fail_stop: drive.watermark,
            replay_frames: replay.frames,
            replay_records: replay.records,
            value_survived,
        }
    }

    pub(super) fn public_fsync_err_restart_watermark_report(
        seed: u64,
        power_cut_seed: u64,
    ) -> PublicFsyncErrRestartWatermarkReport {
        let net = CellNet::new(0, seed, Plant::None);
        let drive;
        let previous_watermark;
        {
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
            cell.request(fd, &[b"SET", b"stable:1", b"kept"], b"+OK\r\n");
            previous_watermark = cell.plane.durability_watermark();
            assert!(previous_watermark > 0);

            net.borrow_mut().fail_next_file_op(SimFileOpKind::Sync, libc::EIO);
            net.borrow_mut().client_send(fd, &resp_command(&[b"SET", b"failed:1", b"lost"]));
            drive = cell.run_until_fail_stop_or_bound(fd);
        }

        net.borrow_mut().power_cut(power_cut_seed);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public fsync_err restart-watermark namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"ledger"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Always)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install public fsync_err restart-watermark recovered namespace catalog");
        let catalog = first_boot_segment_catalog();
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("public fsync_err restart-watermark active-tail replay after sim power cut");
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        let got = cell.request_at_least(fd, &[b"GET", b"stable:1"], b"$-1\r\n".len());
        let stable_value_survived = if got == b"$-1\r\n" {
            false
        } else {
            assert_eq!(got, bulk_reply(b"kept"));
            true
        };
        let got = cell.request_at_least(fd, &[b"GET", b"failed:1"], b"$-1\r\n".len());
        let failed_value_absent = got == b"$-1\r\n";
        cell.close(fd);

        PublicFsyncErrRestartWatermarkReport {
            seed,
            power_cut_seed,
            fail_stopped: drive.fail_stopped,
            panic_message: drive.panic_message,
            previous_watermark,
            watermark_before_fail_stop: drive.watermark,
            reply_bytes_before_fail_stop: drive.reply_bytes,
            replay_frames: replay.frames,
            replay_records: replay.records,
            stable_value_survived,
            failed_value_absent,
            active_offset_bytes,
        }
    }

    pub(super) fn public_log_append_write_fault_report(
        seed: u64,
        power_cut_seed: u64,
    ) -> PublicLogAppendWriteFaultReport {
        let net = CellNet::new(0, seed, Plant::None);
        let drive;
        {
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
            net.borrow_mut().fail_next_file_op(SimFileOpKind::Write, libc::EIO);
            net.borrow_mut().client_send(fd, &resp_command(&[b"SET", b"order:1", b"paid"]));
            drive = cell.run_until_fail_stop_or_bound(fd);
        }

        net.borrow_mut().power_cut(power_cut_seed);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public log append write-fault namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"ledger"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Always)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install public log append write-fault recovered namespace catalog");
        let catalog = first_boot_segment_catalog();
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("public log append write-fault active-tail replay after sim power cut");
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        let got = cell.request_at_least(fd, &[b"GET", b"order:1"], b"$-1\r\n".len());
        let value_survived = if got == b"$-1\r\n" {
            false
        } else {
            assert_eq!(got, bulk_reply(b"paid"));
            true
        };
        cell.close(fd);

        PublicLogAppendWriteFaultReport {
            seed,
            power_cut_seed,
            fail_stopped: drive.fail_stopped,
            panic_message: drive.panic_message,
            reply_bytes_before_fail_stop: drive.reply_bytes,
            watermark_before_fail_stop: drive.watermark,
            replay_frames: replay.frames,
            replay_records: replay.records,
            value_survived,
        }
    }

    pub(super) fn public_power_cut_after_seal_recovery_report(
        seed: u64,
    ) -> PublicSealRecoveryReport {
        public_seal_recovery_report(
            seed,
            PublicSealWorkload {
                key_a: b"seal:a",
                value_a: vec![b'a'; 88],
                key_b: b"seal:b",
                value_b: vec![b'b'; 88],
                key_c: b"seal:c",
                value_c: b"rotated".to_vec(),
                expected_sealed_segment_len_bytes: 256,
                power_cut_mask: 0xA11C_5EA1,
            },
        )
    }

    pub(super) fn public_power_cut_after_non_exact_seal_recovery_report(
        seed: u64,
    ) -> PublicSealRecoveryReport {
        public_seal_recovery_report(
            seed,
            PublicSealWorkload {
                key_a: b"tail:a",
                value_a: vec![b'a'; 88],
                key_b: b"tail:b",
                value_b: vec![b'b'; 40],
                key_c: b"tail:c",
                value_c: vec![b'c'; 9],
                expected_sealed_segment_len_bytes: 208,
                power_cut_mask: 0xA11C_5EA2,
            },
        )
    }

    struct PublicSealWorkload {
        key_a: &'static [u8],
        value_a: Vec<u8>,
        key_b: &'static [u8],
        value_b: Vec<u8>,
        key_c: &'static [u8],
        value_c: Vec<u8>,
        expected_sealed_segment_len_bytes: u64,
        power_cut_mask: u64,
    }

    fn public_seal_recovery_report(
        seed: u64,
        workload: PublicSealWorkload,
    ) -> PublicSealRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let log_config = m2_seal_log_config();
        let watermark_at_cut;
        let segment_zero_path = log_segment_path(LogSegmentId::ZERO);
        {
            let mut cell =
                PublicSimCell::first_boot_with_config(Rc::clone(&net), seed, &log_config);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
            cell.request(fd, &[b"SET", workload.key_a, workload.value_a.as_slice()], b"+OK\r\n");
            cell.run_iterations(16);
            assert!(
                stable_file_exists(&net, &log_segment_path(LogSegmentId::new(1).unwrap())),
                "segment maintenance must durably prepare segment 1 before rotation"
            );
            cell.request(fd, &[b"SET", workload.key_b, workload.value_b.as_slice()], b"+OK\r\n");
            cell.request(fd, &[b"SET", workload.key_c, workload.value_c.as_slice()], b"+OK\r\n");
            assert!(
                stable_file_has_nonzero_bytes(
                    &net,
                    &log_segment_path(LogSegmentId::new(1).unwrap()),
                ),
                "rotated segment must contain synced frame bytes before power cut"
            );
            assert_eq!(
                stable_file_len(&net, &segment_zero_path),
                Some(workload.expected_sealed_segment_len_bytes),
                "sealed segment 0 must be materialized at the expected used length"
            );
            watermark_at_cut = cell.plane.durability_watermark();
            assert!(watermark_at_cut > 0);
            cell.close(fd);
        }

        net.borrow_mut().power_cut(seed ^ workload.power_cut_mask);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load power_cut_after_seal namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"ledger"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Always)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install power_cut_after_seal recovered namespace catalog");
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &log_config,
            &two_segment_catalog(),
            &mut recovered,
            &mut completions,
        )
        .expect("power_cut_after_seal two-segment replay after sim power cut");
        let active_segment = writer.active_segment().get();
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered_with_config(
            Rc::clone(&net),
            seed,
            recovered,
            writer,
            &log_config,
        );
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        let mut survived_keys = 0u64;
        for (key, value) in [
            (workload.key_a, workload.value_a.as_slice()),
            (workload.key_b, workload.value_b.as_slice()),
            (workload.key_c, workload.value_c.as_slice()),
        ] {
            let got = cell.request_at_least(fd, &[b"GET", key], bulk_reply(value).len());
            assert_eq!(got, bulk_reply(value));
            survived_keys += 1;
        }
        cell.close(fd);

        PublicSealRecoveryReport {
            seed,
            replay_frames: replay.frames,
            replay_records: replay.records,
            active_segment,
            active_offset_bytes,
            sealed_segment_len_bytes: workload.expected_sealed_segment_len_bytes,
            survived_keys,
            watermark_at_cut,
        }
    }

    pub(super) fn public_torn_final_frame_recovery_report(
        seed: u64,
    ) -> PublicTornFinalFrameRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let mut writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("first boot log writer for torn-final recovery row");
        let mut durability = DurabilityCell::with_capacity(1024).unwrap();

        let stable = stage_default_string_frame(
            &mut driver,
            &mut pool,
            &mut writer,
            &mut durability,
            b"stable",
            b"ok",
        );
        let corrupt = stage_default_string_frame(
            &mut driver,
            &mut pool,
            &mut writer,
            &mut durability,
            b"torn",
            b"lost",
        );
        corrupt_frame_crc_byte(&mut driver, &mut pool, LogSegmentId::ZERO, corrupt, 0x00_D220);
        assert!(completions.is_empty());

        net.borrow_mut().power_cut(seed ^ 0xA11C_5147);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let catalog = first_boot_segment_catalog();
        let mut recovered = Keyspace::new(StoreConfig::default());
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("torn-final recovery should keep stable prefix");
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(pool.reconcile(), Ok(()));

        PublicTornFinalFrameRecoveryReport {
            seed,
            stable_frame_end_bytes: stable.frame_end().offset(),
            corrupt_frame_offset_bytes: corrupt.frame_start().offset(),
            replay_frames: replay.frames,
            replay_records: replay.records,
            active_offset_bytes,
            torn_tail_truncated: active_offset_bytes == stable.frame_end().offset(),
            stable_value_survived: recovered.db_mut(0).get(b"stable", Nanos(0))
                == Some(b"ok".as_slice()),
            corrupt_value_absent: recovered.db_mut(0).get(b"torn", Nanos(0)).is_none(),
        }
    }

    pub(super) fn public_active_tail_later_magic_recovery_report(
        seed: u64,
    ) -> PublicActiveTailLaterMagicRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let mut writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("first boot log writer for active-tail later-magic row");
        let mut durability = DurabilityCell::with_capacity(1024).unwrap();

        let stable = stage_default_string_frame(
            &mut driver,
            &mut pool,
            &mut writer,
            &mut durability,
            b"stable",
            b"ok",
        );
        let corrupt = stage_default_string_frame(
            &mut driver,
            &mut pool,
            &mut writer,
            &mut durability,
            b"corrupt",
            b"bad",
        );
        let later = stage_default_string_frame(
            &mut driver,
            &mut pool,
            &mut writer,
            &mut durability,
            b"after",
            b"later",
        );
        corrupt_frame_crc_byte(&mut driver, &mut pool, LogSegmentId::ZERO, corrupt, 0x00_D230);
        assert!(completions.is_empty());

        net.borrow_mut().power_cut(seed ^ 0xA11C_5148);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let catalog = first_boot_segment_catalog();
        let mut recovered = Keyspace::new(StoreConfig::default());
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("active-tail later magic should truncate to stable prefix");
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(pool.reconcile(), Ok(()));

        PublicActiveTailLaterMagicRecoveryReport {
            seed,
            stable_frame_end_bytes: stable.frame_end().offset(),
            corrupt_offset_bytes: corrupt.frame_start().offset(),
            later_frame_offset_bytes: later.frame_start().offset(),
            replay_frames: replay.frames,
            replay_records: replay.records,
            active_offset_bytes,
            active_tail_truncated: active_offset_bytes == stable.frame_end().offset(),
            stable_value_survived: recovered.db_mut(0).get(b"stable", Nanos(0))
                == Some(b"ok".as_slice()),
            corrupt_value_absent: recovered.db_mut(0).get(b"corrupt", Nanos(0)).is_none(),
            later_value_absent: recovered.db_mut(0).get(b"after", Nanos(0)).is_none(),
        }
    }

    #[cfg(test)]
    pub(super) fn public_preallocate_enospc_degrade_report(
        seed: u64,
    ) -> PublicPreallocateEnospcReport {
        let net = CellNet::new(0, seed, Plant::None);
        let log_config = m2_seal_log_config();
        let durable_value = vec![b'a'; 88];
        let durable_refusal =
            b"-ERR durable write rejected: log preallocation failed with ENOSPC\r\n";
        let durable_refused;
        let memory_served_after_degrade;
        let watermark_at_degrade;
        {
            let mut cell =
                PublicSimCell::first_boot_with_config(Rc::clone(&net), seed, &log_config);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"CREATE", b"cache"], b"+OK\r\n");
            cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");

            net.borrow_mut().fail_next_file_op(SimFileOpKind::Preallocate, libc::ENOSPC);
            cell.request(fd, &[b"SET", b"seal:a", durable_value.as_slice()], b"+OK\r\n");
            watermark_at_degrade = cell.plane.durability_watermark();
            assert!(watermark_at_degrade > 0);
            cell.run_iterations(16);

            let got =
                cell.request_at_least(fd, &[b"SET", b"seal:b", b"late"], durable_refusal.len());
            durable_refused = got == durable_refusal;
            assert_eq!(got, durable_refusal);
            cell.request(fd, &[b"GET", b"seal:b"], b"$-1\r\n");

            cell.request(fd, &[b"INF.NS", b"USE", b"cache"], b"+OK\r\n");
            cell.request(fd, &[b"SET", b"session:1", b"hot"], b"+OK\r\n");
            let got = cell.request_at_least(fd, &[b"GET", b"session:1"], bulk_reply(b"hot").len());
            memory_served_after_degrade = got == bulk_reply(b"hot");
            assert!(memory_served_after_degrade);
            cell.close(fd);
        }

        net.borrow_mut().power_cut(seed ^ 0xA11C_E052);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load preallocate ENOSPC namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"ledger"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Always)
        }));
        assert!(
            loaded.specs().iter().any(|spec| spec.name == b"cache" && spec.mode == NsMode::Memory)
        );

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install preallocate ENOSPC recovered catalog");
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &log_config,
            &first_boot_segment_catalog(),
            &mut recovered,
            &mut completions,
        )
        .expect("preallocate ENOSPC active-tail replay after sim power cut");
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered_with_config(
            Rc::clone(&net),
            seed,
            recovered,
            writer,
            &log_config,
        );
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        let got = cell.request_at_least(
            fd,
            &[b"GET", b"seal:a"],
            bulk_reply(durable_value.as_slice()).len(),
        );
        let durable_value_survived = got == bulk_reply(durable_value.as_slice());
        assert!(durable_value_survived);
        let got = cell.request_at_least(fd, &[b"GET", b"seal:b"], b"$-1\r\n".len());
        let refused_value_absent = got == b"$-1\r\n";
        assert!(refused_value_absent);
        cell.request(fd, &[b"INF.NS", b"USE", b"cache"], b"+OK\r\n");
        let got = cell.request_at_least(fd, &[b"GET", b"session:1"], b"$-1\r\n".len());
        let memory_value_absent_after_restart = got == b"$-1\r\n";
        assert!(memory_value_absent_after_restart);
        cell.close(fd);

        PublicPreallocateEnospcReport {
            seed,
            durable_refused,
            memory_served_after_degrade,
            replay_frames: replay.frames,
            replay_records: replay.records,
            durable_value_survived,
            refused_value_absent,
            memory_value_absent_after_restart,
            watermark_at_degrade,
        }
    }

    pub(super) fn public_manifest_checkpoint_tail_recovery_report(
        seed: u64,
    ) -> PublicManifestRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let checkpoint_id = LogCheckpointId::new(13).unwrap();
        let now = Nanos(10_000);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let mut writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("first boot log writer for manifest recovery runner row");

        let catalog = durable_namespace_catalog();
        let mut checkpoint_keyspace = Keyspace::new(StoreConfig::default());
        checkpoint_keyspace
            .ns_replace_with_recovered_catalog(catalog)
            .expect("install checkpoint catalog");
        checkpoint_keyspace
            .durable_named_db_mut(NsId::new(16))
            .expect("checkpoint durable namespace")
            .set(b"snapshot", b"image", SetOptions::default(), now)
            .expect("set checkpoint snapshot value");

        let mut durability = DurabilityCell::with_capacity(1024).unwrap();
        durability
            .stage_mutation_effect(
                LogNamespaceId::new(16),
                MutationEffect::StringPostImage {
                    key: b"before",
                    value: b"skip",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage pre-checkpoint mutation");
        drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        durability.stage_checkpoint_begin(checkpoint_id).expect("stage checkpoint begin");
        let begin = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability)
            .frame_start();
        let checkpoint = LogCheckpointRef::new(checkpoint_id, begin);

        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 11);
        let data_fd = open_dir(&mut driver, &mut pool, root_fd, M2_RECOVERY_DATA_ROOT, 12);
        let shard_fd = open_dir(&mut driver, &mut pool, data_fd, "shard-0", 13);
        let ckpt_fd = open_dir(&mut driver, &mut pool, shard_fd, "ckpt", 14);
        let published = publish_checkpoint_keyspace_snapshot_image(
            &mut driver,
            &mut pool,
            CellId(0),
            checkpoint,
            &checkpoint_keyspace,
            CheckpointKeyspacePublishConfig::new(
                CheckpointKeyspaceSnapshotConfig::new(now),
                CheckpointImagePublishConfig::new(ckpt_fd, M2_CHECKPOINT_IMAGE_TOKEN_SLOT)
                    .with_max_reaps(8),
            ),
            &mut completions,
        )
        .expect("publish manifest recovery checkpoint image");
        assert_eq!(published.snapshot().records_emitted(), 1);

        durability
            .stage_mutation_effect(
                LogNamespaceId::new(16),
                MutationEffect::StringPostImage {
                    key: b"tail",
                    value: b"after",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage post-checkpoint tail mutation");
        let tail = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        let manifest = LogRecoveryManifest::new(checkpoint, first_boot_segment_catalog())
            .expect("recovery manifest");
        publish_recovery_manifest(
            &mut driver,
            &mut pool,
            &manifest,
            RecoveryManifestPublishConfig::new(ckpt_fd, M2_RECOVERY_MANIFEST_TOKEN_SLOT)
                .with_max_reaps(8),
            &mut completions,
        )
        .expect("publish manifest recovery root");
        assert!(completions.is_empty());

        net.borrow_mut().power_cut(seed ^ 0xA11C_E065);

        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded_manifest = load_recovery_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("load manifest recovery root after sim power cut")
        .expect("manifest is present after publish");
        assert_eq!(loaded_manifest, manifest);

        let mut recovered = Keyspace::new(StoreConfig::default());
        let (writer, applied, replay) = open_recovered_log_writer_replaying_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &loaded_manifest,
            &mut recovered,
            &mut completions,
        )
        .expect("recover checkpoint-plus-tail after sim power cut");
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(active_offset_bytes, tail.frame_end().offset());
        assert_eq!(applied.records().records, 1);
        assert_eq!(replay.records, 1);
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        cell.request(fd, &[b"GET", b"snapshot"], b"$5\r\nimage\r\n");
        cell.request(fd, &[b"GET", b"tail"], b"$5\r\nafter\r\n");
        cell.request(fd, &[b"GET", b"before"], b"$-1\r\n");
        cell.close(fd);

        PublicManifestRecoveryReport {
            seed,
            checkpoint_records: applied.records().records,
            replay_frames: replay.frames,
            replay_records: replay.records,
            active_offset_bytes,
        }
    }

    pub(super) fn public_manifest_rename_fail_recovery_report(
        seed: u64,
    ) -> PublicManifestRenameFailRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let checkpoint_id = LogCheckpointId::new(14).unwrap();
        let now = Nanos(11_000);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let mut writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("first boot log writer for manifest rename-fail runner row");

        let mut durability = DurabilityCell::with_capacity(1024).unwrap();
        durability
            .stage_mutation_effect(
                LogNamespaceId::new(0),
                MutationEffect::StringPostImage {
                    key: b"before",
                    value: b"full",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage pre-checkpoint default-namespace mutation");
        drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        durability.stage_checkpoint_begin(checkpoint_id).expect("stage checkpoint begin");
        let begin = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability)
            .frame_start();
        let checkpoint = LogCheckpointRef::new(checkpoint_id, begin);

        let mut checkpoint_keyspace = Keyspace::new(StoreConfig::default());
        checkpoint_keyspace
            .db_mut(0)
            .set(b"snapshot", b"image", SetOptions::default(), now)
            .expect("set default checkpoint snapshot value");

        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 21);
        let data_fd = open_dir(&mut driver, &mut pool, root_fd, M2_RECOVERY_DATA_ROOT, 22);
        let shard_fd = open_dir(&mut driver, &mut pool, data_fd, "shard-0", 23);
        let ckpt_fd = open_dir(&mut driver, &mut pool, shard_fd, "ckpt", 24);
        let published = publish_checkpoint_keyspace_snapshot_image(
            &mut driver,
            &mut pool,
            CellId(0),
            checkpoint,
            &checkpoint_keyspace,
            CheckpointKeyspacePublishConfig::new(
                CheckpointKeyspaceSnapshotConfig::new(now),
                CheckpointImagePublishConfig::new(ckpt_fd, M2_CHECKPOINT_IMAGE_TOKEN_SLOT)
                    .with_max_reaps(8),
            ),
            &mut completions,
        )
        .expect("publish inert checkpoint image before failed manifest rename");
        assert_eq!(published.snapshot().records_emitted(), 1);

        durability
            .stage_mutation_effect(
                LogNamespaceId::new(0),
                MutationEffect::StringPostImage {
                    key: b"tail",
                    value: b"after",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage post-checkpoint default-namespace tail mutation");
        let tail = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        let manifest = LogRecoveryManifest::new(checkpoint, first_boot_segment_catalog())
            .expect("recovery manifest");
        net.borrow_mut().fail_next_file_op(SimFileOpKind::Rename, libc::EIO);
        let publish_error = publish_recovery_manifest(
            &mut driver,
            &mut pool,
            &manifest,
            RecoveryManifestPublishConfig::new(ckpt_fd, M2_RECOVERY_MANIFEST_TOKEN_SLOT)
                .with_max_reaps(8),
            &mut completions,
        )
        .expect_err("manifest rename fault must fail publication");
        assert!(matches!(
            publish_error,
            RecoveryManifestPublishError::Rename { errno: libc::EIO, .. }
        ));
        assert!(completions.is_empty());

        net.borrow_mut().power_cut(seed ^ 0xA11C_E066);

        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded_manifest = load_recovery_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("probe manifest after failed rename and power cut");
        assert!(loaded_manifest.is_none());

        let catalog = first_boot_segment_catalog();
        let mut recovered = Keyspace::new(StoreConfig::default());
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("full-log replay after failed manifest rename");
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(active_offset_bytes, tail.frame_end().offset());
        assert_eq!(replay.records, 2);
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"GET", b"snapshot"], b"$-1\r\n");
        cell.request(fd, &[b"GET", b"before"], b"$4\r\nfull\r\n");
        cell.request(fd, &[b"GET", b"tail"], b"$5\r\nafter\r\n");
        cell.close(fd);

        PublicManifestRenameFailRecoveryReport {
            seed,
            checkpoint_records: published.snapshot().records_emitted() as u64,
            manifest_present_after_recovery: loaded_manifest.is_some(),
            replay_frames: replay.frames,
            replay_records: replay.records,
            active_offset_bytes,
        }
    }

    pub(super) fn public_manifest_dir_fsync_fail_recovery_report(
        seed: u64,
    ) -> PublicManifestDirFsyncFailRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let checkpoint_id = LogCheckpointId::new(15).unwrap();
        let now = Nanos(12_000);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let mut writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("first boot log writer for manifest dir-fsync runner row");

        let mut durability = DurabilityCell::with_capacity(1024).unwrap();
        durability
            .stage_mutation_effect(
                LogNamespaceId::new(0),
                MutationEffect::StringPostImage {
                    key: b"before",
                    value: b"full",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage pre-checkpoint default-namespace mutation");
        drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        durability.stage_checkpoint_begin(checkpoint_id).expect("stage checkpoint begin");
        let begin = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability)
            .frame_start();
        let checkpoint = LogCheckpointRef::new(checkpoint_id, begin);

        let mut checkpoint_keyspace = Keyspace::new(StoreConfig::default());
        checkpoint_keyspace
            .db_mut(0)
            .set(b"snapshot", b"image", SetOptions::default(), now)
            .expect("set default checkpoint snapshot value");

        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 31);
        let data_fd = open_dir(&mut driver, &mut pool, root_fd, M2_RECOVERY_DATA_ROOT, 32);
        let shard_fd = open_dir(&mut driver, &mut pool, data_fd, "shard-0", 33);
        let ckpt_fd = open_dir(&mut driver, &mut pool, shard_fd, "ckpt", 34);
        let published = publish_checkpoint_keyspace_snapshot_image(
            &mut driver,
            &mut pool,
            CellId(0),
            checkpoint,
            &checkpoint_keyspace,
            CheckpointKeyspacePublishConfig::new(
                CheckpointKeyspaceSnapshotConfig::new(now),
                CheckpointImagePublishConfig::new(ckpt_fd, M2_CHECKPOINT_IMAGE_TOKEN_SLOT)
                    .with_max_reaps(8),
            ),
            &mut completions,
        )
        .expect("publish inert checkpoint image before failed manifest dir fsync");
        assert_eq!(published.snapshot().records_emitted(), 1);

        durability
            .stage_mutation_effect(
                LogNamespaceId::new(0),
                MutationEffect::StringPostImage {
                    key: b"tail",
                    value: b"after",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage post-checkpoint default-namespace tail mutation");
        let tail = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        let manifest = LogRecoveryManifest::new(checkpoint, first_boot_segment_catalog())
            .expect("recovery manifest");
        net.borrow_mut().fail_next_file_sync(FileSyncMode::Full, libc::EIO);
        let publish_error = publish_recovery_manifest(
            &mut driver,
            &mut pool,
            &manifest,
            RecoveryManifestPublishConfig::new(ckpt_fd, M2_RECOVERY_MANIFEST_TOKEN_SLOT)
                .with_max_reaps(8),
            &mut completions,
        )
        .expect_err("manifest dir fsync fault must fail publication");
        assert!(matches!(
            publish_error,
            RecoveryManifestPublishError::SyncDir { errno: libc::EIO, .. }
        ));
        assert!(completions.is_empty());

        net.borrow_mut().power_cut(seed ^ 0xA11C_E067);

        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded_manifest = load_recovery_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("probe manifest after failed dir fsync and power cut");
        assert!(loaded_manifest.is_none());

        let catalog = first_boot_segment_catalog();
        let mut recovered = Keyspace::new(StoreConfig::default());
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &catalog,
            &mut recovered,
            &mut completions,
        )
        .expect("full-log replay after failed manifest dir fsync");
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(active_offset_bytes, tail.frame_end().offset());
        assert_eq!(replay.records, 2);
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"GET", b"snapshot"], b"$-1\r\n");
        cell.request(fd, &[b"GET", b"before"], b"$4\r\nfull\r\n");
        cell.request(fd, &[b"GET", b"tail"], b"$5\r\nafter\r\n");
        cell.close(fd);

        PublicManifestDirFsyncFailRecoveryReport {
            seed,
            checkpoint_records: published.snapshot().records_emitted() as u64,
            manifest_present_after_recovery: loaded_manifest.is_some(),
            replay_frames: replay.frames,
            replay_records: replay.records,
            active_offset_bytes,
        }
    }

    pub(super) fn public_live_checkpoint_wait_dir_fsync_fail_report(
        seed: u64,
    ) -> PublicLiveCheckpointDirFsyncFailReport {
        let net = CellNet::new(0, seed, Plant::None);
        let checkpoint_image_path = format!(
            "./{}/shard-0/ckpt/{}",
            M2_RECOVERY_DATA_ROOT,
            LogCheckpointId::FIRST_LIVE.file_name()
        );
        let drive;
        {
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
            cell.request(fd, &[b"SET", b"before", b"full"], b"+OK\r\n");
            cell.request(fd, &[b"SET", b"tail", b"after"], b"+OK\r\n");
            net.borrow_mut().client_send(fd, &resp_command(&[b"INF.CKPT", b"WAIT"]));
            drive = cell.run_until_fail_stop_after_stable_path(fd, &checkpoint_image_path);
        }

        net.borrow_mut().power_cut(seed ^ 0xA11C_E090);

        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded_manifest = load_recovery_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("probe recovery MANIFEST after live checkpoint dir-fsync fail-stop");
        assert!(loaded_manifest.is_none());

        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load live checkpoint dir-fsync namespace catalog after sim power cut");
        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install live checkpoint dir-fsync recovered namespace catalog");
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &first_boot_segment_catalog(),
            &mut recovered,
            &mut completions,
        )
        .expect("full-log replay after live checkpoint dir-fsync fail-stop");
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        let before = bulk_reply(b"full");
        let got_before = cell.request_at_least(fd, &[b"GET", b"before"], before.len());
        let after = bulk_reply(b"after");
        let got_after = cell.request_at_least(fd, &[b"GET", b"tail"], after.len());
        cell.close(fd);

        PublicLiveCheckpointDirFsyncFailReport {
            seed,
            fail_stopped: drive.fail_stopped,
            panic_message: drive.panic_message,
            reply_bytes_before_fail_stop: drive.reply_bytes,
            watermark_before_fail_stop: drive.watermark,
            manifest_present_after_recovery: loaded_manifest.is_some(),
            replay_frames: replay.frames,
            replay_records: replay.records,
            before_value_survived: got_before == before,
            tail_value_survived: got_after == after,
            active_offset_bytes,
        }
    }

    pub(super) fn public_manifest_replacement_dir_fsync_fail_recovery_report(
        seed: u64,
    ) -> PublicManifestReplacementFailRecoveryReport {
        public_manifest_replacement_fail_recovery_report(seed, ManifestReplacementFailure::DirFsync)
    }

    pub(super) fn public_checkpoint_write_enospc_recovery_report(
        seed: u64,
    ) -> PublicCheckpointWriteEnospcReport {
        let net = CellNet::new(0, seed, Plant::None);
        let old_checkpoint_id = LogCheckpointId::new(18).unwrap();
        let new_checkpoint_id = LogCheckpointId::new(19).unwrap();
        let now = Nanos(14_000);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let mut writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("first boot log writer for checkpoint ENOSPC row");

        let mut durability = DurabilityCell::with_capacity(1024).unwrap();
        durability.stage_checkpoint_begin(old_checkpoint_id).expect("stage old checkpoint begin");
        let old_begin =
            drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability)
                .frame_start();
        let old_checkpoint = LogCheckpointRef::new(old_checkpoint_id, old_begin);

        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 51);
        let data_fd = open_dir(&mut driver, &mut pool, root_fd, M2_RECOVERY_DATA_ROOT, 52);
        let shard_fd = open_dir(&mut driver, &mut pool, data_fd, "shard-0", 53);
        let ckpt_fd = open_dir(&mut driver, &mut pool, shard_fd, "ckpt", 54);
        let mut old_keyspace = Keyspace::new(StoreConfig::default());
        old_keyspace
            .db_mut(0)
            .set(b"stable", b"old", SetOptions::default(), now)
            .expect("set old checkpoint value");
        let old_published = publish_checkpoint_keyspace_snapshot_image(
            &mut driver,
            &mut pool,
            CellId(0),
            old_checkpoint,
            &old_keyspace,
            CheckpointKeyspacePublishConfig::new(
                CheckpointKeyspaceSnapshotConfig::new(now),
                CheckpointImagePublishConfig::new(ckpt_fd, M2_CHECKPOINT_IMAGE_TOKEN_SLOT)
                    .with_max_reaps(8),
            ),
            &mut completions,
        )
        .expect("publish old checkpoint image before checkpoint ENOSPC");
        assert_eq!(old_published.snapshot().records_emitted(), 1);

        let old_manifest = LogRecoveryManifest::new(old_checkpoint, first_boot_segment_catalog())
            .expect("old recovery manifest");
        publish_recovery_manifest(
            &mut driver,
            &mut pool,
            &old_manifest,
            RecoveryManifestPublishConfig::new(ckpt_fd, M2_RECOVERY_MANIFEST_TOKEN_SLOT)
                .with_max_reaps(8),
            &mut completions,
        )
        .expect("publish old manifest recovery root before checkpoint ENOSPC");

        durability
            .stage_mutation_effect(
                LogNamespaceId::new(0),
                MutationEffect::StringPostImage {
                    key: b"between",
                    value: b"log",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage old-manifest tail before checkpoint ENOSPC");
        drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        durability.stage_checkpoint_begin(new_checkpoint_id).expect("stage new checkpoint begin");
        let new_begin =
            drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability)
                .frame_start();
        let new_checkpoint = LogCheckpointRef::new(new_checkpoint_id, new_begin);

        let mut new_keyspace = Keyspace::new(StoreConfig::default());
        new_keyspace
            .db_mut(0)
            .set(b"new_snapshot", b"image", SetOptions::default(), now)
            .expect("set new checkpoint value");
        net.borrow_mut().fail_next_file_op(SimFileOpKind::Write, libc::ENOSPC);
        let publish_error = publish_checkpoint_keyspace_snapshot_image(
            &mut driver,
            &mut pool,
            CellId(0),
            new_checkpoint,
            &new_keyspace,
            CheckpointKeyspacePublishConfig::new(
                CheckpointKeyspaceSnapshotConfig::new(now),
                CheckpointImagePublishConfig::new(ckpt_fd, M2_CHECKPOINT_IMAGE_TOKEN_SLOT)
                    .with_max_reaps(8),
            ),
            &mut completions,
        )
        .expect_err("checkpoint image ENOSPC must fail publication");
        let checkpoint_publish_failed_enospc = matches!(
            publish_error,
            CheckpointKeyspacePublishError::Publish(CheckpointImagePublishError::WriteTemp {
                errno: libc::ENOSPC,
                ..
            })
        );
        assert!(checkpoint_publish_failed_enospc);
        assert!(completions.is_empty());

        net.borrow_mut().power_cut(seed ^ 0xA11C_E075);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded_manifest = load_recovery_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("load manifest after checkpoint ENOSPC")
        .expect("old manifest remains present after checkpoint ENOSPC");
        let loaded_old_manifest = loaded_manifest == old_manifest;
        assert!(loaded_old_manifest);

        let mut recovered = Keyspace::new(StoreConfig::default());
        let (writer, applied, replay) = open_recovered_log_writer_replaying_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &loaded_manifest,
            &mut recovered,
            &mut completions,
        )
        .expect("recover through old manifest after checkpoint ENOSPC");
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(applied.records().records, 1);
        assert_eq!(replay.records, 1);
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"GET", b"stable"], b"$3\r\nold\r\n");
        cell.request(fd, &[b"GET", b"between"], b"$3\r\nlog\r\n");
        cell.request(fd, &[b"GET", b"new_snapshot"], b"$-1\r\n");
        cell.close(fd);

        PublicCheckpointWriteEnospcReport {
            seed,
            old_checkpoint_records: old_published.snapshot().records_emitted() as u64,
            checkpoint_publish_failed_enospc,
            loaded_old_manifest,
            replay_frames: replay.frames,
            replay_records: replay.records,
            active_offset_bytes,
        }
    }

    pub(super) fn public_manifest_replacement_rename_fail_recovery_report(
        seed: u64,
    ) -> PublicManifestReplacementFailRecoveryReport {
        public_manifest_replacement_fail_recovery_report(seed, ManifestReplacementFailure::Rename)
    }

    fn public_manifest_replacement_fail_recovery_report(
        seed: u64,
        failure: ManifestReplacementFailure,
    ) -> PublicManifestReplacementFailRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let old_checkpoint_id = LogCheckpointId::new(16).unwrap();
        let new_checkpoint_id = LogCheckpointId::new(17).unwrap();
        let now = Nanos(13_000);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let mut writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("first boot log writer for manifest replacement row");

        let mut durability = DurabilityCell::with_capacity(1024).unwrap();
        durability.stage_checkpoint_begin(old_checkpoint_id).expect("stage old checkpoint begin");
        let old_begin =
            drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability)
                .frame_start();
        let old_checkpoint = LogCheckpointRef::new(old_checkpoint_id, old_begin);

        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 41);
        let data_fd = open_dir(&mut driver, &mut pool, root_fd, M2_RECOVERY_DATA_ROOT, 42);
        let shard_fd = open_dir(&mut driver, &mut pool, data_fd, "shard-0", 43);
        let ckpt_fd = open_dir(&mut driver, &mut pool, shard_fd, "ckpt", 44);
        let mut old_keyspace = Keyspace::new(StoreConfig::default());
        old_keyspace
            .db_mut(0)
            .set(b"stable", b"old", SetOptions::default(), now)
            .expect("set old checkpoint value");
        let old_published = publish_checkpoint_keyspace_snapshot_image(
            &mut driver,
            &mut pool,
            CellId(0),
            old_checkpoint,
            &old_keyspace,
            CheckpointKeyspacePublishConfig::new(
                CheckpointKeyspaceSnapshotConfig::new(now),
                CheckpointImagePublishConfig::new(ckpt_fd, M2_CHECKPOINT_IMAGE_TOKEN_SLOT)
                    .with_max_reaps(8),
            ),
            &mut completions,
        )
        .expect("publish old checkpoint image");
        assert_eq!(old_published.snapshot().records_emitted(), 1);

        let old_manifest = LogRecoveryManifest::new(old_checkpoint, first_boot_segment_catalog())
            .expect("old recovery manifest");
        publish_recovery_manifest(
            &mut driver,
            &mut pool,
            &old_manifest,
            RecoveryManifestPublishConfig::new(ckpt_fd, M2_RECOVERY_MANIFEST_TOKEN_SLOT)
                .with_max_reaps(8),
            &mut completions,
        )
        .expect("publish old manifest recovery root");

        durability
            .stage_mutation_effect(
                LogNamespaceId::new(0),
                MutationEffect::StringPostImage {
                    key: b"between",
                    value: b"log",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage tail after old manifest");
        drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        durability.stage_checkpoint_begin(new_checkpoint_id).expect("stage new checkpoint begin");
        let new_begin =
            drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability)
                .frame_start();
        let new_checkpoint = LogCheckpointRef::new(new_checkpoint_id, new_begin);

        let mut new_keyspace = Keyspace::new(StoreConfig::default());
        new_keyspace
            .db_mut(0)
            .set(b"new_snapshot", b"image", SetOptions::default(), now)
            .expect("set new checkpoint-only value");
        let new_published = publish_checkpoint_keyspace_snapshot_image(
            &mut driver,
            &mut pool,
            CellId(0),
            new_checkpoint,
            &new_keyspace,
            CheckpointKeyspacePublishConfig::new(
                CheckpointKeyspaceSnapshotConfig::new(now),
                CheckpointImagePublishConfig::new(ckpt_fd, M2_CHECKPOINT_IMAGE_TOKEN_SLOT)
                    .with_max_reaps(8),
            ),
            &mut completions,
        )
        .expect("publish new checkpoint image before failed replacement manifest");
        assert_eq!(new_published.snapshot().records_emitted(), 1);

        durability
            .stage_mutation_effect(
                LogNamespaceId::new(0),
                MutationEffect::StringPostImage {
                    key: b"tail",
                    value: b"after",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage tail after new checkpoint");
        let tail = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        let new_manifest = LogRecoveryManifest::new(new_checkpoint, first_boot_segment_catalog())
            .expect("new recovery manifest");
        match failure {
            ManifestReplacementFailure::Rename => {
                net.borrow_mut().fail_next_file_op(SimFileOpKind::Rename, libc::EIO);
            }
            ManifestReplacementFailure::DirFsync => {
                net.borrow_mut().fail_next_file_sync(FileSyncMode::Full, libc::EIO);
            }
        }
        let publish_error = publish_recovery_manifest(
            &mut driver,
            &mut pool,
            &new_manifest,
            RecoveryManifestPublishConfig::new(ckpt_fd, M2_RECOVERY_MANIFEST_TOKEN_SLOT)
                .with_max_reaps(8),
            &mut completions,
        )
        .expect_err("replacement manifest fault must fail publication");
        match failure {
            ManifestReplacementFailure::Rename => {
                assert!(matches!(
                    publish_error,
                    RecoveryManifestPublishError::Rename { errno: libc::EIO, .. }
                ));
            }
            ManifestReplacementFailure::DirFsync => {
                assert!(matches!(
                    publish_error,
                    RecoveryManifestPublishError::SyncDir { errno: libc::EIO, .. }
                ));
            }
        }
        assert!(completions.is_empty());

        let power_cut_salt = match failure {
            ManifestReplacementFailure::Rename => 0xA11C_E069,
            ManifestReplacementFailure::DirFsync => 0xA11C_E068,
        };
        net.borrow_mut().power_cut(seed ^ power_cut_salt);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded_manifest = load_recovery_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("load manifest after failed replacement dir fsync")
        .expect("old manifest remains present");
        let loaded_old_manifest = loaded_manifest == old_manifest;
        assert!(loaded_old_manifest);

        let mut recovered = Keyspace::new(StoreConfig::default());
        let (writer, applied, replay) = open_recovered_log_writer_replaying_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &loaded_manifest,
            &mut recovered,
            &mut completions,
        )
        .expect("recover through old manifest after failed replacement");
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(active_offset_bytes, tail.frame_end().offset());
        assert_eq!(applied.records().records, 1);
        assert_eq!(replay.records, 2);
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"GET", b"stable"], b"$3\r\nold\r\n");
        cell.request(fd, &[b"GET", b"between"], b"$3\r\nlog\r\n");
        cell.request(fd, &[b"GET", b"tail"], b"$5\r\nafter\r\n");
        cell.request(fd, &[b"GET", b"new_snapshot"], b"$-1\r\n");
        cell.close(fd);

        PublicManifestReplacementFailRecoveryReport {
            seed,
            old_checkpoint_records: applied.records().records,
            new_checkpoint_records: new_published.snapshot().records_emitted() as u64,
            loaded_old_manifest,
            replay_frames: replay.frames,
            replay_records: replay.records,
            active_offset_bytes,
        }
    }

    pub fn public_fsync_err_process_fail_stop(seed: u64) {
        let net = CellNet::new(0, seed, Plant::None);
        let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
        let fd = cell.connect();
        cell.request(
            fd,
            &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
            b"+OK\r\n",
        );
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        net.borrow_mut().fail_next_file_op(SimFileOpKind::Sync, libc::EIO);
        net.borrow_mut().client_send(fd, &resp_command(&[b"SET", b"order:1", b"paid"]));
        cell.run_until_process_fail_stop(fd, "fsync_err");
    }

    pub fn public_log_append_write_fault_process_fail_stop(seed: u64) {
        let net = CellNet::new(0, seed, Plant::None);
        let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
        let fd = cell.connect();
        cell.request(
            fd,
            &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
            b"+OK\r\n",
        );
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        net.borrow_mut().fail_next_file_op(SimFileOpKind::Write, libc::EIO);
        net.borrow_mut().client_send(fd, &resp_command(&[b"SET", b"order:1", b"paid"]));
        cell.run_until_process_fail_stop(fd, "log_append_short_write");
    }

    pub fn public_live_checkpoint_wait_dir_fsync_process_fail_stop(seed: u64) {
        let net = CellNet::new(0, seed, Plant::None);
        let checkpoint_image_path = format!(
            "./{}/shard-0/ckpt/{}",
            M2_RECOVERY_DATA_ROOT,
            LogCheckpointId::FIRST_LIVE.file_name()
        );
        let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
        let fd = cell.connect();
        cell.request(
            fd,
            &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
            b"+OK\r\n",
        );
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        cell.request(fd, &[b"SET", b"before", b"full"], b"+OK\r\n");
        cell.request(fd, &[b"SET", b"tail", b"after"], b"+OK\r\n");
        net.borrow_mut().client_send(fd, &resp_command(&[b"INF.CKPT", b"WAIT"]));
        cell.run_until_process_fail_stop_after_stable_path(
            fd,
            "live_checkpoint_dir_fsync",
            &checkpoint_image_path,
        );
    }

    fn m2_log_config() -> LogDataRootConfig {
        LogDataRootConfig::first_boot_sized(
            M2_RECOVERY_DATA_ROOT.to_string(),
            CellId(0),
            64 * 1024,
            1024,
            16 * 1024,
            M2_RECOVERY_BOOTSTRAP_TOKEN_SLOT,
            M2_RECOVERY_WRITER_TOKEN_SLOT,
        )
        .expect("valid m2 sim log segment config")
        .with_generations(0, 0)
        .with_wait(Wait::Poll)
        .with_max_reaps(8)
    }

    fn m2_seal_log_config() -> LogDataRootConfig {
        LogDataRootConfig::first_boot_sized(
            M2_RECOVERY_DATA_ROOT.to_string(),
            CellId(0),
            256,
            128,
            128,
            M2_RECOVERY_BOOTSTRAP_TOKEN_SLOT,
            M2_RECOVERY_WRITER_TOKEN_SLOT,
        )
        .expect("valid m2 seal-row segment config")
        .with_generations(0, 0)
        .with_wait(Wait::Poll)
        .with_max_reaps(8)
    }

    fn two_segment_catalog() -> LogSegmentCatalog {
        let names = [LogSegmentId::ZERO.file_name(), LogSegmentId::new(1).unwrap().file_name()];
        scan_log_segment_names(names.iter().map(String::as_str)).expect("two-segment catalog")
    }

    fn log_segment_path(segment: LogSegmentId) -> String {
        format!("./{}/shard-0/log/{}", M2_RECOVERY_DATA_ROOT, segment.file_name())
    }

    fn stable_file_has_nonzero_bytes(net: &Rc<std::cell::RefCell<CellNet>>, path: &str) -> bool {
        let net = net.borrow();
        let Some(inode) = net.stable_paths.get(path).copied() else {
            return false;
        };
        let Some(node) = net.nodes.get(&inode) else {
            return false;
        };
        node.kind == SimNodeKind::File && node.stable_bytes.iter().any(|byte| *byte != 0)
    }

    fn stable_file_len(net: &Rc<std::cell::RefCell<CellNet>>, path: &str) -> Option<u64> {
        let net = net.borrow();
        let inode = net.stable_paths.get(path).copied()?;
        let node = net.nodes.get(&inode)?;
        (node.kind == SimNodeKind::File).then_some(node.stable_len_bytes)
    }

    fn stable_file_exists(net: &Rc<std::cell::RefCell<CellNet>>, path: &str) -> bool {
        let net = net.borrow();
        let Some(inode) = net.stable_paths.get(path).copied() else {
            return false;
        };
        net.nodes.get(&inode).is_some_and(|node| node.kind == SimNodeKind::File)
    }

    fn m2_namespace_load_config() -> NamespaceCatalogDataRootLoadConfig {
        NamespaceCatalogDataRootLoadConfig::new(
            M2_RECOVERY_DATA_ROOT.to_string(),
            M2_RECOVERY_NS_CATALOG_TOKEN_SLOT,
        )
        .with_wait(Wait::Poll)
        .with_max_reaps(8)
    }

    fn m2_live_publish_config() -> NamespaceCatalogLivePublishConfig {
        NamespaceCatalogLivePublishConfig::new(
            M2_RECOVERY_DATA_ROOT.to_string(),
            M2_RECOVERY_NS_CATALOG_TOKEN_SLOT,
        )
    }

    fn token(slot: u32) -> CompletionToken {
        CompletionToken::new(TokenClass::File, slot, 0)
    }

    fn reap_one(driver: &mut SimDriver, pool: &mut BufferPool) -> CompletionResult {
        let mut out = Vec::new();
        let reaped = driver.submit_and_reap(pool, Wait::Poll, &mut out).unwrap();
        assert_eq!(reaped, 1);
        out.pop().unwrap().result
    }

    fn open_dir(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        dir: RawFd,
        name: &str,
        token_slot: u32,
    ) -> RawFd {
        driver.push(IoOp::FileOpen {
            dir,
            name: name.to_string(),
            mode: FileOpenMode::Directory,
            token: token(token_slot),
        });
        match reap_one(driver, pool) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        }
    }

    fn open_file_read_write(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        name: &str,
        token_slot: u32,
    ) -> RawFd {
        let name = name.strip_prefix("./").unwrap_or(name);
        driver.push(IoOp::FileOpen {
            dir: libc::AT_FDCWD,
            name: name.to_string(),
            mode: FileOpenMode::ReadWrite,
            token: token(token_slot),
        });
        match reap_one(driver, pool) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        }
    }

    fn close_file(driver: &mut SimDriver, pool: &mut BufferPool, fd: RawFd, token_slot: u32) {
        driver.push(IoOp::FileClose { fd, token: token(token_slot) });
        assert!(matches!(reap_one(driver, pool), CompletionResult::FileClosed));
    }

    fn sync_fd(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        fd: RawFd,
        mode: FileSyncMode,
        token_slot: u32,
    ) {
        driver.push(IoOp::FileSync { fd, mode, token: token(token_slot) });
        assert!(matches!(reap_one(driver, pool), CompletionResult::FileDone));
    }

    fn read_file_bytes_at(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        fd: RawFd,
        offset_bytes: u64,
        len: u32,
        token_slot: u32,
    ) -> Vec<u8> {
        let buf = pool.try_lease(LeaseKind::Recv).expect("read buffer");
        driver.push(IoOp::FileReadAt { fd, offset_bytes, buf, len, token: token(token_slot) });
        match reap_one(driver, pool) {
            CompletionResult::FileRead { buf: got, len } => {
                assert_eq!(got, buf);
                let bytes = pool.bytes(got)[..len as usize].to_vec();
                pool.release(got);
                bytes
            }
            other => panic!("unexpected completion {other:?}"),
        }
    }

    fn write_file_bytes_at(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        fd: RawFd,
        offset_bytes: u64,
        bytes: &[u8],
        token_slot: u32,
    ) {
        let buf = pool.try_lease(LeaseKind::Send).expect("write buffer");
        pool.bytes_mut(buf)[..bytes.len()].copy_from_slice(bytes);
        driver.push(IoOp::FileWriteAt {
            fd,
            offset_bytes,
            buf,
            len: bytes.len() as u32,
            token: token(token_slot),
        });
        match reap_one(driver, pool) {
            CompletionResult::FileWritten { buf: got } => {
                assert_eq!(got, buf);
                pool.release(got);
            }
            other => panic!("unexpected completion {other:?}"),
        }
    }

    fn corrupt_frame_crc_byte(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        segment: LogSegmentId,
        frame: LogFrameMeta,
        token_slot: u32,
    ) {
        let path = log_segment_path(segment);
        let offset = u64::from(frame.frame_end().offset() - 1);
        let fd = open_file_read_write(driver, pool, &path, token_slot);
        let mut byte = read_file_bytes_at(driver, pool, fd, offset, 1, token_slot + 1);
        assert_eq!(byte.len(), 1);
        byte[0] ^= 0x80;
        write_file_bytes_at(driver, pool, fd, offset, &byte, token_slot + 2);
        sync_fd(driver, pool, fd, FileSyncMode::DataOnly, token_slot + 3);
        close_file(driver, pool, fd, token_slot + 4);
    }

    fn drive_synced_log_frame(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        writer: &mut LogWriteIo,
        durability: &mut DurabilityCell,
    ) -> LogFrameMeta {
        let mut ops = Vec::new();
        let queued = writer
            .queue_frame_synced(durability, pool, &mut ops, FileSyncMode::DataOnly)
            .expect("queue synced log frame")
            .expect("staged frame");
        let mut durable = None;

        while durable.is_none() {
            let op = ops.pop().expect("pending log op");
            driver.push(op);

            let mut completions = Vec::new();
            let reaped = driver.submit_and_reap(pool, Wait::Poll, &mut completions).unwrap();
            assert_eq!(reaped, 1);
            let completion = completions.pop().expect("log completion");
            assert!(completions.is_empty());

            match writer.on_completion(pool, &mut ops, completion).expect("log completion") {
                LogWriteCompletion::SyncQueued { .. } => {}
                LogWriteCompletion::SealProgress { .. } => {}
                LogWriteCompletion::SealFinalized { .. } => {}
                LogWriteCompletion::FrameDurable(meta) => durable = Some(meta),
                LogWriteCompletion::FrameWritten(_) => {
                    panic!("synced frame completed without fdatasync")
                }
            }
        }

        let durable = durable.unwrap();
        assert_eq!(durable, queued);
        durable
    }

    fn stage_default_string_frame(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        writer: &mut LogWriteIo,
        durability: &mut DurabilityCell,
        key: &[u8],
        value: &[u8],
    ) -> LogFrameMeta {
        durability
            .stage_mutation_effect(
                LogNamespaceId::new(0),
                MutationEffect::StringPostImage { key, value, expire_at_ms: None, raw: false },
            )
            .expect("stage default namespace mutation");
        drive_synced_log_frame(driver, pool, writer, durability)
    }

    fn durable_namespace_catalog() -> NsCatalog {
        NsCatalog::new(
            NsId::new(18),
            vec![
                NsSpec {
                    id: NsId::new(16),
                    name: b"ledger".to_vec(),
                    mode: NsMode::Durable,
                    fsync: Some(NsFsyncPolicy::Always),
                    policy: None,
                    maxmemory: None,
                },
                NsSpec {
                    id: NsId::new(17),
                    name: b"sessions".to_vec(),
                    mode: NsMode::Durable,
                    fsync: Some(NsFsyncPolicy::Everysec),
                    policy: None,
                    maxmemory: None,
                },
            ],
        )
        .expect("durable namespace catalog")
    }

    fn resp_command(argv: &[&[u8]]) -> Vec<u8> {
        let mut wire = format!("*{}\r\n", argv.len()).into_bytes();
        for arg in argv {
            wire.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
            wire.extend_from_slice(arg);
            wire.extend_from_slice(b"\r\n");
        }
        wire
    }

    fn bulk_reply(value: &[u8]) -> Vec<u8> {
        let mut reply = format!("${}\r\n", value.len()).into_bytes();
        reply.extend_from_slice(value);
        reply.extend_from_slice(b"\r\n");
        reply
    }

    fn public_keyspace_state(
        cell: &mut PublicSimCell,
        fd: RawFd,
        key_space: u64,
    ) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut state = BTreeMap::new();
        for key_idx in 0..key_space {
            let key = format!("k:{key_idx}").into_bytes();
            let reply = cell.request_reply(fd, &[b"GET", key.as_slice()]);
            if let Some(value) = parse_bulk_reply(&reply) {
                state.insert(key, value);
            }
        }
        state
    }

    fn parse_bulk_reply(reply: &[u8]) -> Option<Vec<u8>> {
        if reply == b"$-1\r\n" {
            return None;
        }
        assert_eq!(reply.first(), Some(&b'$'));
        let header_end = find_crlf(reply).expect("bulk reply header must end in CRLF");
        let len = parse_decimal_usize(&reply[1..header_end]);
        let body_start = header_end + 2;
        let body_end = body_start + len;
        assert_eq!(reply.len(), body_end + 2);
        assert_eq!(&reply[body_end..], b"\r\n");
        Some(reply[body_start..body_end].to_vec())
    }

    fn resp_reply_complete(reply: &[u8]) -> bool {
        let Some(kind) = reply.first().copied() else {
            return false;
        };
        match kind {
            b'+' | b'-' | b':' => find_crlf(reply).is_some(),
            b'$' => bulk_reply_complete(reply),
            _ => panic!("unexpected RESP reply kind {kind}"),
        }
    }

    fn bulk_reply_complete(reply: &[u8]) -> bool {
        let Some(header_end) = find_crlf(reply) else {
            return false;
        };
        if &reply[1..header_end] == b"-1" {
            return reply.len() >= 5;
        }
        let len = parse_decimal_usize(&reply[1..header_end]);
        reply.len() >= header_end + 2 + len + 2
    }

    fn find_crlf(bytes: &[u8]) -> Option<usize> {
        bytes.windows(2).position(|pair| pair == b"\r\n")
    }

    fn parse_decimal_usize(bytes: &[u8]) -> usize {
        assert!(!bytes.is_empty());
        let mut value = 0usize;
        for byte in bytes {
            assert!(byte.is_ascii_digit());
            value = value
                .checked_mul(10)
                .and_then(|next| next.checked_add(usize::from(byte - b'0')))
                .expect("decimal length overflow");
        }
        value
    }

    fn state_digest(state: &BTreeMap<Vec<u8>, Vec<u8>>) -> u64 {
        let mut bytes = Vec::new();
        for (key, value) in state {
            bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
            bytes.extend_from_slice(key);
            bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
            bytes.extend_from_slice(value);
        }
        hash64(&bytes, PUBLIC_RECOVERY_SWEEP_HASH_SEED)
    }

    fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
        value.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing string {key}"))
    }

    fn optional_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
        value.get(key).and_then(Value::as_str)
    }

    fn require_eq(value: &Value, key: &str, expected: &str) -> Result<(), String> {
        let actual = required_string(value, key)?;
        if actual == expected {
            Ok(())
        } else {
            Err(format!("{key}={actual:?}, expected {expected:?}"))
        }
    }

    fn matrix_seed(value: &Value, key: &str) -> Result<u64, String> {
        let text = required_string(value, key)?;
        text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).map_or_else(
            || text.parse().map_err(|error| format!("{key}: {error}")),
            |hex| u64::from_str_radix(hex, 16).map_err(|error| format!("{key}: {error}")),
        )
    }

    fn optional_seed(value: &Value, key: &str) -> Result<Option<u64>, String> {
        let Some(text) = optional_string(value, key) else {
            return Ok(None);
        };
        text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).map_or_else(
            || text.parse().map(Some).map_err(|error| format!("{key}: {error}")),
            |hex| u64::from_str_radix(hex, 16).map(Some).map_err(|error| format!("{key}: {error}")),
        )
    }

    fn sweep_config_from_row(row: &Value) -> Result<DurabilitySweepConfig, String> {
        let seed = matrix_seed(row, "seed")?;
        let seeds = matrix_seed(row, "sweep_seeds")?;
        let writes_per_seed = optional_seed(row, "writes_per_seed")?.unwrap_or(1);
        let key_space = optional_seed(row, "key_space")?.unwrap_or(1);
        Ok(DurabilitySweepConfig {
            seed,
            seeds,
            writes_per_seed,
            key_space,
            ..DurabilitySweepConfig::ci(seed)
        })
    }

    fn append_str(out: &mut Vec<u8>, value: &str) {
        append_u64(out, value.len() as u64);
        out.extend_from_slice(value.as_bytes());
    }

    fn append_u64(out: &mut Vec<u8>, value: u64) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn append_bool(out: &mut Vec<u8>, value: bool) {
        out.push(u8::from(value));
    }

    fn panic_payload(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(message) = payload.downcast_ref::<&str>() {
            (*message).to_string()
        } else if let Some(message) = payload.downcast_ref::<String>() {
            message.clone()
        } else {
            "non-string panic payload".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use inf_fabric::{Mesh, MeshConfig};
    use inf_foundation::CellId;
    use inf_foundation::time::{Nanos, VirtualClock};
    use inf_log::{
        CheckpointHeader, CheckpointId, CheckpointRef, CheckpointSectionKind, CheckpointSectionRef,
        FrameMeta, Lsn, NamespaceId, RecoveryManifest, SegmentCatalog, SegmentId,
        scan_segment_names,
    };
    use inf_runtime::{CellLoop, FileSyncMode, LoopConfig, TokenClass, Wait};
    use inf_server::checkpoint::{
        CheckpointImageLoadConfig, CheckpointImagePublishConfig, CheckpointKeyspacePublishConfig,
        CheckpointKeyspaceSnapshotConfig, load_checkpoint_image, publish_checkpoint_image,
        publish_checkpoint_keyspace_snapshot_image,
    };
    use inf_server::durability::DurabilityCell;
    use inf_server::log_bootstrap::{
        LogDataRootConfig, load_recovery_manifest_in_data_root,
        open_first_boot_log_writer_in_data_root, open_recovered_log_writer_replaying_in_data_root,
        open_recovered_log_writer_replaying_manifest_in_data_root,
    };
    use inf_server::log_writer::{LogWriteCompletion, LogWriteIo};
    use inf_server::manifest::{
        RecoveryManifestLoadConfig, RecoveryManifestPublishConfig, load_recovery_manifest,
        publish_recovery_manifest,
    };
    use inf_server::ns_catalog::{
        NamespaceCatalogDataRootLoadConfig, NamespaceCatalogDataRootPublishConfig,
        NamespaceCatalogLivePublishConfig, NamespaceCatalogLivePublisher,
        load_namespace_catalog_in_data_root, publish_namespace_catalog_in_data_root,
    };
    use inf_server::{NodeInfo, ServerPlane};
    use inf_store::{
        Keyspace, MutationEffect, NsCatalog, NsFsyncPolicy, NsId, NsMode, NsSpec, SetOptions,
        StoreConfig,
    };
    use toml::Value;

    const M2_CRASH_MATRIX_TOML: &str = include_str!("../../../tests/crash-matrix/m2.toml");
    const M2_RECOVERY_DATA_ROOT: &str = "m2-data";
    const M2_RECOVERY_BOOTSTRAP_TOKEN_SLOT: u32 = 0x00_D200;
    const M2_RECOVERY_WRITER_TOKEN_SLOT: u32 = 0x00_D201;
    const M2_RECOVERY_NS_CATALOG_TOKEN_SLOT: u32 = 0x00_D202;
    const M2_RECOVERY_MANIFEST_TOKEN_SLOT: u32 = 0x00_D203;
    const M2_CHECKPOINT_IMAGE_TOKEN_SLOT: u32 = 0x00_D204;
    const PUBLIC_SIM_MAX_ITERS: usize = 512;
    const PUBLIC_DIGEST_SWEEP_SEEDS: u64 = 8;
    const PUBLIC_DIGEST_SWEEP_OPS: usize = 24;
    const PUBLIC_DIGEST_SWEEP_KEYS: u64 = 8;
    const PUBLIC_EVERYSEC_LOSS_SEARCH_SEEDS: u64 = 32;
    const PUBLIC_EVERYSEC_WORKLOAD_SWEEP_SEEDS: u64 = 16;

    fn token(slot: u32) -> CompletionToken {
        CompletionToken::new(TokenClass::File, slot, 0)
    }

    fn reap_one(driver: &mut SimDriver, pool: &mut BufferPool) -> CompletionResult {
        let mut out = Vec::new();
        let reaped = driver.submit_and_reap(pool, Wait::Poll, &mut out).unwrap();
        assert_eq!(reaped, 1);
        out.pop().unwrap().result
    }

    fn open_file(driver: &mut SimDriver, pool: &mut BufferPool, name: &str) -> RawFd {
        driver.push(IoOp::FileOpen {
            dir: libc::AT_FDCWD,
            name: name.to_string(),
            mode: FileOpenMode::ReadWriteCreate,
            token: token(1),
        });
        match reap_one(driver, pool) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        }
    }

    fn open_existing_file(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        name: &str,
        token_slot: u32,
    ) -> CompletionResult {
        driver.push(IoOp::FileOpen {
            dir: libc::AT_FDCWD,
            name: name.to_string(),
            mode: FileOpenMode::ReadWrite,
            token: token(token_slot),
        });
        reap_one(driver, pool)
    }

    fn open_dir(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        dir: RawFd,
        name: &str,
        token_slot: u32,
    ) -> RawFd {
        driver.push(IoOp::FileOpen {
            dir,
            name: name.to_string(),
            mode: FileOpenMode::Directory,
            token: token(token_slot),
        });
        match reap_one(driver, pool) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        }
    }

    fn sync_fd(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        fd: RawFd,
        mode: FileSyncMode,
        token_slot: u32,
    ) {
        driver.push(IoOp::FileSync { fd, mode, token: token(token_slot) });
        assert!(matches!(reap_one(driver, pool), CompletionResult::FileDone));
    }

    fn write_bytes(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        fd: RawFd,
        offset_bytes: u64,
        bytes: &[u8],
        token_slot: u32,
    ) {
        let buf = pool.try_lease(LeaseKind::Send).unwrap();
        pool.bytes_mut(buf)[..bytes.len()].copy_from_slice(bytes);
        driver.push(IoOp::FileWriteAt {
            fd,
            offset_bytes,
            buf,
            len: bytes.len() as u32,
            token: token(token_slot),
        });
        match reap_one(driver, pool) {
            CompletionResult::FileWritten { buf: got } => {
                assert_eq!(got, buf);
                pool.release(got);
            }
            other => panic!("unexpected completion {other:?}"),
        }
    }

    fn read_bytes(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        fd: RawFd,
        len: u32,
        token_slot: u32,
    ) -> Vec<u8> {
        let buf = pool.try_lease(LeaseKind::Recv).unwrap();
        driver.push(IoOp::FileReadAt { fd, offset_bytes: 0, buf, len, token: token(token_slot) });
        match reap_one(driver, pool) {
            CompletionResult::FileRead { buf: got, len } => {
                let bytes = pool.bytes(got)[..len as usize].to_vec();
                pool.release(got);
                bytes
            }
            other => panic!("unexpected completion {other:?}"),
        }
    }

    fn m2_log_config() -> LogDataRootConfig {
        LogDataRootConfig::first_boot_sized(
            M2_RECOVERY_DATA_ROOT.to_string(),
            CellId(0),
            64 * 1024,
            1024,
            16 * 1024,
            M2_RECOVERY_BOOTSTRAP_TOKEN_SLOT,
            M2_RECOVERY_WRITER_TOKEN_SLOT,
        )
        .expect("valid m2 sim log segment config")
        .with_generations(0, 0)
        .with_wait(Wait::Poll)
        .with_max_reaps(8)
    }

    fn m2_namespace_publish_config() -> NamespaceCatalogDataRootPublishConfig {
        NamespaceCatalogDataRootPublishConfig::new(
            M2_RECOVERY_DATA_ROOT.to_string(),
            M2_RECOVERY_NS_CATALOG_TOKEN_SLOT,
        )
        .with_wait(Wait::Poll)
        .with_max_reaps(8)
    }

    fn m2_namespace_load_config() -> NamespaceCatalogDataRootLoadConfig {
        NamespaceCatalogDataRootLoadConfig::new(
            M2_RECOVERY_DATA_ROOT.to_string(),
            M2_RECOVERY_NS_CATALOG_TOKEN_SLOT,
        )
        .with_wait(Wait::Poll)
        .with_max_reaps(8)
    }

    fn m2_live_publish_config() -> NamespaceCatalogLivePublishConfig {
        NamespaceCatalogLivePublishConfig::new(
            M2_RECOVERY_DATA_ROOT.to_string(),
            M2_RECOVERY_NS_CATALOG_TOKEN_SLOT,
        )
    }

    fn single_segment_catalog() -> SegmentCatalog {
        let names = [SegmentId::ZERO.file_name()];
        scan_segment_names(names.iter().map(String::as_str)).expect("single segment catalog")
    }

    fn recovery_manifest() -> RecoveryManifest {
        RecoveryManifest::new(
            CheckpointRef::new(CheckpointId::ZERO, Lsn::new(0, 0)),
            single_segment_catalog(),
        )
        .expect("single-segment recovery manifest")
    }

    struct PublicSimCell {
        net: Rc<RefCell<CellNet>>,
        clock: Rc<VirtualClock>,
        cell_loop: CellLoop<SimDriver, Rc<VirtualClock>>,
        plane: ServerPlane,
    }

    impl PublicSimCell {
        fn first_boot(net: Rc<RefCell<CellNet>>, seed: u64) -> PublicSimCell {
            let mut driver = SimDriver::new(Rc::clone(&net));
            let mut pool = BufferPool::new(128, 4096);
            let mut completions = Vec::new();
            let writer = open_first_boot_log_writer_in_data_root(
                &mut driver,
                &mut pool,
                &m2_log_config(),
                &mut completions,
            )
            .expect("first boot public sim log writer");
            let publisher = NamespaceCatalogLivePublisher::new(m2_live_publish_config())
                .expect("public sim namespace catalog publisher");
            PublicSimCell::from_parts(
                net,
                seed,
                driver,
                pool,
                Keyspace::new(StoreConfig::default()),
                writer,
                Some(publisher),
            )
        }

        fn recovered(
            net: Rc<RefCell<CellNet>>,
            seed: u64,
            keyspace: Keyspace,
            writer: LogWriteIo,
        ) -> PublicSimCell {
            let driver = SimDriver::new(Rc::clone(&net));
            let pool = BufferPool::new(128, 4096);
            PublicSimCell::from_parts(net, seed, driver, pool, keyspace, writer, None)
        }

        fn from_parts(
            net: Rc<RefCell<CellNet>>,
            seed: u64,
            driver: SimDriver,
            pool: BufferPool,
            keyspace: Keyspace,
            writer: LogWriteIo,
            publisher: Option<NamespaceCatalogLivePublisher>,
        ) -> PublicSimCell {
            let clock = Rc::new(VirtualClock::new(Nanos(1)));
            let fabric = Mesh::new(1, MeshConfig { ring_capacity: 64, data_credits: 32 })
                .into_iter()
                .next()
                .expect("one fabric endpoint");
            let node = Rc::new(NodeInfo::default());
            node.rng_state.set(seed ^ 0xD2D2_D2D2);
            let mut plane = ServerPlane::new(
                CellId(0),
                1,
                listener_fd(0),
                keyspace,
                fabric,
                node,
                inf_server::NoopObserver,
                false,
            );
            plane.install_log_writer(writer);
            if let Some(publisher) = publisher {
                plane.install_namespace_catalog_publisher(publisher);
            }
            let config = LoopConfig { spin_iters: 4, ..Default::default() };
            let cell_loop = CellLoop::new(driver, Rc::clone(&clock), pool, config);
            PublicSimCell { net, clock, cell_loop, plane }
        }

        fn connect(&self) -> RawFd {
            self.net.borrow_mut().connect()
        }

        fn request(&mut self, fd: RawFd, argv: &[&[u8]], expected: &[u8]) {
            self.net.borrow_mut().client_send(fd, &resp_command(argv));
            let got = self.run_until_reply(fd, expected.len());
            assert_eq!(got, expected);
        }

        fn request_at_least(&mut self, fd: RawFd, argv: &[&[u8]], min_len: usize) -> Vec<u8> {
            self.net.borrow_mut().client_send(fd, &resp_command(argv));
            self.run_until_reply(fd, min_len)
        }

        fn run_until_reply(&mut self, fd: RawFd, expected_len: usize) -> Vec<u8> {
            let mut reply = Vec::new();
            for _ in 0..PUBLIC_SIM_MAX_ITERS {
                self.cell_loop.run_iteration(&mut self.plane).expect("public sim iteration");
                reply.extend(self.net.borrow_mut().client_recv(fd));
                if reply.len() >= expected_len {
                    return reply;
                }
                self.clock.advance(Nanos(1_000));
            }
            panic!("public sim request did not complete within bounded iterations");
        }

        fn run_iterations(&mut self, count: usize) {
            for _ in 0..count {
                self.cell_loop.run_iteration(&mut self.plane).expect("public sim iteration");
                self.clock.advance(Nanos(1_000));
            }
        }

        fn drive_everysec_timer_fsync(&mut self) {
            self.clock.advance(Nanos::from_secs(1) + Nanos::from_millis(1));
            for _ in 0..PUBLIC_SIM_MAX_ITERS {
                self.cell_loop.run_iteration(&mut self.plane).expect("public sim iteration");
                if self.plane.durability_watermark() > 0 {
                    return;
                }
                self.clock.advance(Nanos(1_000));
            }
            panic!("public sim everysec timer did not advance the durability watermark");
        }

        fn close(&mut self, fd: RawFd) {
            self.net.borrow_mut().client_close(fd);
            for _ in 0..64 {
                self.cell_loop.run_iteration(&mut self.plane).expect("public sim close iteration");
                if self.net.borrow().closed(fd) {
                    return;
                }
                self.clock.advance(Nanos(1_000));
            }
            panic!("public sim connection did not close within bounded iterations");
        }
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    struct PublicEverysecRecoveryReport {
        seed: u64,
        replay_frames: u64,
        replay_records: u64,
        value_survived: bool,
        watermark_at_cut: u64,
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    struct PublicAlwaysRecoveryReport {
        seed: u64,
        replay_frames: u64,
        replay_records: u64,
        value_survived: bool,
        watermark_at_cut: u64,
    }

    fn resp_command(argv: &[&[u8]]) -> Vec<u8> {
        let mut wire = format!("*{}\r\n", argv.len()).into_bytes();
        for arg in argv {
            wire.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
            wire.extend_from_slice(arg);
            wire.extend_from_slice(b"\r\n");
        }
        wire
    }

    fn bulk_reply(value: &[u8]) -> Vec<u8> {
        let mut reply = format!("${}\r\n", value.len()).into_bytes();
        reply.extend_from_slice(value);
        reply.extend_from_slice(b"\r\n");
        reply
    }

    fn matrix_runner_rows() -> Vec<Value> {
        let matrix = toml::from_str::<Value>(M2_CRASH_MATRIX_TOML).expect("m2 crash matrix parses");
        assert_eq!(matrix_string(&matrix, "status"), "partial-runner");
        matrix
            .get("runner_rows")
            .and_then(Value::as_array)
            .expect("m2 crash matrix runner rows")
            .clone()
    }

    fn matrix_string<'a>(value: &'a Value, key: &str) -> &'a str {
        value.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("missing string {key}"))
    }

    fn matrix_seed(value: &Value, key: &str) -> u64 {
        let text = matrix_string(value, key);
        text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).map_or_else(
            || text.parse().expect("decimal seed"),
            |hex| u64::from_str_radix(hex, 16).expect("hex seed"),
        )
    }

    fn test_sweep_config_from_row(row: &Value) -> crate::durability::DurabilitySweepConfig {
        let seed = matrix_seed(row, "seed");
        crate::durability::DurabilitySweepConfig {
            seed,
            seeds: matrix_seed(row, "sweep_seeds"),
            writes_per_seed: row
                .get("writes_per_seed")
                .and_then(Value::as_str)
                .map_or(1, |value| value.parse().expect("writes_per_seed")),
            key_space: row
                .get("key_space")
                .and_then(Value::as_str)
                .map_or(1, |value| value.parse().expect("key_space")),
            ..crate::durability::DurabilitySweepConfig::ci(seed)
        }
    }

    fn public_always_recovery_report(seed: u64) -> PublicAlwaysRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let watermark_at_cut;
        {
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
            cell.request(fd, &[b"SET", b"order:1", b"paid"], b"+OK\r\n");
            watermark_at_cut = cell.plane.durability_watermark();
            assert!(watermark_at_cut > 0);
            cell.close(fd);
        }

        net.borrow_mut().power_cut(seed ^ 0xA11C_E048);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public always namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"ledger"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Always)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered.ns_replace_with_recovered_catalog(loaded).expect("install recovered catalog");
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &single_segment_catalog(),
            &mut recovered,
            &mut completions,
        )
        .expect("public always active-tail replay after sim power cut");
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        let got = cell.request_at_least(fd, &[b"GET", b"order:1"], b"$-1\r\n".len());
        let value_survived = if got == b"$-1\r\n" {
            false
        } else {
            assert_eq!(got, bulk_reply(b"paid"));
            true
        };
        cell.close(fd);

        PublicAlwaysRecoveryReport {
            seed,
            replay_frames: replay.frames,
            replay_records: replay.records,
            value_survived,
            watermark_at_cut,
        }
    }

    fn public_everysec_recovery_report(
        seed: u64,
        flush_timer: bool,
    ) -> PublicEverysecRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let mut watermark_at_cut = 0;
        {
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), seed);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"sessions", b"MODE", b"durable", b"FSYNC", b"everysec"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"sessions"], b"+OK\r\n");
            cell.request(fd, &[b"SET", b"session:1", b"alive"], b"+OK\r\n");
            assert_eq!(cell.plane.durability_watermark(), 0);

            cell.run_iterations(1);
            assert_eq!(cell.plane.durability_watermark(), 0);
            if flush_timer {
                cell.drive_everysec_timer_fsync();
                watermark_at_cut = cell.plane.durability_watermark();
                assert!(watermark_at_cut > 0);
            }
        }

        net.borrow_mut().power_cut(seed ^ 0xA11C_E046);

        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public everysec namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.name == b"sessions"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Everysec)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install public everysec recovered namespace catalog");
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &single_segment_catalog(),
            &mut recovered,
            &mut completions,
        )
        .expect("public everysec active-tail replay after sim power cut");
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"sessions"], b"+OK\r\n");
        let got = cell.request_at_least(fd, &[b"GET", b"session:1"], b"$-1\r\n".len());
        let value_survived = if got == b"$-1\r\n" {
            false
        } else {
            assert_eq!(got, bulk_reply(b"alive"));
            true
        };
        cell.close(fd);

        PublicEverysecRecoveryReport {
            seed,
            replay_frames: replay.frames,
            replay_records: replay.records,
            value_survived,
            watermark_at_cut,
        }
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    struct PublicManifestRecoveryReport {
        checkpoint_records: u64,
        replay_frames: u64,
        replay_records: u64,
        active_offset_bytes: u32,
    }

    fn public_manifest_checkpoint_tail_recovery_report(seed: u64) -> PublicManifestRecoveryReport {
        let net = CellNet::new(0, seed, Plant::None);
        let checkpoint_id = CheckpointId::new(13).unwrap();
        let now = Nanos(10_000);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let mut writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("first boot log writer for manifest recovery proof");

        let catalog = durable_namespace_catalog();
        let mut checkpoint_keyspace = Keyspace::new(StoreConfig::default());
        checkpoint_keyspace
            .ns_replace_with_recovered_catalog(catalog)
            .expect("install checkpoint catalog");
        checkpoint_keyspace
            .durable_named_db_mut(NsId::new(16))
            .expect("checkpoint durable namespace")
            .set(b"snapshot", b"image", SetOptions::default(), now)
            .expect("set checkpoint snapshot value");

        let mut durability = DurabilityCell::with_capacity(1024).unwrap();
        durability
            .stage_mutation_effect(
                NamespaceId::new(16),
                MutationEffect::StringPostImage {
                    key: b"before",
                    value: b"skip",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage pre-checkpoint mutation");
        drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        durability.stage_checkpoint_begin(checkpoint_id).expect("stage checkpoint begin");
        let begin = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability)
            .frame_start();
        let checkpoint = CheckpointRef::new(checkpoint_id, begin);

        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 11);
        let data_fd = open_dir(&mut driver, &mut pool, root_fd, M2_RECOVERY_DATA_ROOT, 12);
        let shard_fd = open_dir(&mut driver, &mut pool, data_fd, "shard-0", 13);
        let ckpt_fd = open_dir(&mut driver, &mut pool, shard_fd, "ckpt", 14);
        let published = publish_checkpoint_keyspace_snapshot_image(
            &mut driver,
            &mut pool,
            CellId(0),
            checkpoint,
            &checkpoint_keyspace,
            CheckpointKeyspacePublishConfig::new(
                CheckpointKeyspaceSnapshotConfig::new(now),
                CheckpointImagePublishConfig::new(ckpt_fd, M2_CHECKPOINT_IMAGE_TOKEN_SLOT)
                    .with_max_reaps(8),
            ),
            &mut completions,
        )
        .expect("publish manifest recovery checkpoint image");
        assert_eq!(published.snapshot().records_emitted(), 1);

        durability
            .stage_mutation_effect(
                NamespaceId::new(16),
                MutationEffect::StringPostImage {
                    key: b"tail",
                    value: b"after",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage post-checkpoint tail mutation");
        let tail = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);

        let manifest =
            RecoveryManifest::new(checkpoint, single_segment_catalog()).expect("recovery manifest");
        publish_recovery_manifest(
            &mut driver,
            &mut pool,
            &manifest,
            RecoveryManifestPublishConfig::new(ckpt_fd, M2_RECOVERY_MANIFEST_TOKEN_SLOT)
                .with_max_reaps(8),
            &mut completions,
        )
        .expect("publish manifest recovery root");
        assert!(completions.is_empty());

        net.borrow_mut().power_cut(seed ^ 0xA11C_E065);

        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded_manifest = load_recovery_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &mut completions,
        )
        .expect("load manifest recovery root after sim power cut")
        .expect("manifest is present after publish");
        assert_eq!(loaded_manifest, manifest);

        let mut recovered = Keyspace::new(StoreConfig::default());
        let (writer, applied, replay) = open_recovered_log_writer_replaying_manifest_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &loaded_manifest,
            &mut recovered,
            &mut completions,
        )
        .expect("recover checkpoint-plus-tail after sim power cut");
        let active_offset_bytes = writer.active_offset_bytes();
        assert_eq!(active_offset_bytes, tail.frame_end().offset());
        assert_eq!(applied.records().records, 1);
        assert_eq!(replay.records, 1);
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), seed, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        cell.request(fd, &[b"GET", b"snapshot"], b"$5\r\nimage\r\n");
        cell.request(fd, &[b"GET", b"tail"], b"$5\r\nafter\r\n");
        cell.request(fd, &[b"GET", b"before"], b"$-1\r\n");
        cell.close(fd);

        PublicManifestRecoveryReport {
            checkpoint_records: applied.records().records,
            replay_frames: replay.frames,
            replay_records: replay.records,
            active_offset_bytes,
        }
    }

    fn drive_synced_log_frame(
        driver: &mut SimDriver,
        pool: &mut BufferPool,
        writer: &mut LogWriteIo,
        durability: &mut DurabilityCell,
    ) -> FrameMeta {
        let mut ops = Vec::new();
        let queued = writer
            .queue_frame_synced(durability, pool, &mut ops, FileSyncMode::DataOnly)
            .expect("queue synced log frame")
            .expect("staged frame");
        let mut durable = None;

        while durable.is_none() {
            let op = ops.pop().expect("pending log op");
            driver.push(op);

            let mut completions = Vec::new();
            let reaped = driver.submit_and_reap(pool, Wait::Poll, &mut completions).unwrap();
            assert_eq!(reaped, 1);
            let completion = completions.pop().expect("log completion");
            assert!(completions.is_empty());

            match writer.on_completion(pool, &mut ops, completion).expect("log completion") {
                LogWriteCompletion::SyncQueued { .. } => {}
                LogWriteCompletion::SealProgress { .. } => {}
                LogWriteCompletion::SealFinalized { .. } => {}
                LogWriteCompletion::FrameDurable(meta) => durable = Some(meta),
                LogWriteCompletion::FrameWritten(_) => {
                    panic!("synced frame completed without fdatasync")
                }
            }
        }

        let durable = durable.unwrap();
        assert_eq!(durable, queued);
        durable
    }

    fn read_existing_sector_image(seed: u64) -> Vec<u8> {
        let net = CellNet::new(0, 31, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 2048);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        let fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");
        sync_fd(&mut driver, &mut pool, root_fd, FileSyncMode::Full, 2);
        let base = vec![b'A'; SIM_DISK_SECTOR_BYTES * 2];
        write_bytes(&mut driver, &mut pool, fd, 0, &base, 3);
        sync_fd(&mut driver, &mut pool, fd, FileSyncMode::DataOnly, 4);
        let overwrite = vec![b'B'; SIM_DISK_SECTOR_BYTES * 2];
        write_bytes(&mut driver, &mut pool, fd, 0, &overwrite, 5);

        net.borrow_mut().power_cut(seed);
        let fd = match open_existing_file(&mut driver, &mut pool, "seg-000000.ilog", 6) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        };
        read_bytes(&mut driver, &mut pool, fd, (SIM_DISK_SECTOR_BYTES * 2) as u32, 7)
    }

    fn scripted_power_cut_disk_image(seed: u64) -> Vec<u8> {
        let net = CellNet::new(0, 0xD15C_0018, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(6, SIM_DISK_SECTOR_BYTES * 2);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        let log_fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");
        sync_fd(&mut driver, &mut pool, root_fd, FileSyncMode::Full, 2);

        let stable_log =
            [vec![b'A'; SIM_DISK_SECTOR_BYTES], vec![b'B'; SIM_DISK_SECTOR_BYTES]].concat();
        write_bytes(&mut driver, &mut pool, log_fd, 0, &stable_log, 3);
        sync_fd(&mut driver, &mut pool, log_fd, FileSyncMode::DataOnly, 4);

        let overwrite = vec![b'C'; SIM_DISK_SECTOR_BYTES * 2];
        write_bytes(&mut driver, &mut pool, log_fd, 0, &overwrite, 5);
        let suffix = vec![b'D'; SIM_DISK_SECTOR_BYTES];
        write_bytes(&mut driver, &mut pool, log_fd, SIM_DISK_SECTOR_BYTES as u64, &suffix, 6);

        let manifest_fd = open_file(&mut driver, &mut pool, "manifest.old");
        write_bytes(&mut driver, &mut pool, manifest_fd, 0, b"manifest-v1", 7);
        sync_fd(&mut driver, &mut pool, manifest_fd, FileSyncMode::DataOnly, 8);
        sync_fd(&mut driver, &mut pool, root_fd, FileSyncMode::Full, 9);

        driver.push(IoOp::FileRename {
            old_dir: root_fd,
            old_name: "manifest.old".to_string(),
            new_dir: root_fd,
            new_name: "MANIFEST".to_string(),
            token: token(10),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        let temp_fd = open_file(&mut driver, &mut pool, "lost.tmp");
        write_bytes(&mut driver, &mut pool, temp_fd, 0, b"drop-me", 11);
        sync_fd(&mut driver, &mut pool, temp_fd, FileSyncMode::DataOnly, 12);

        net.borrow_mut().power_cut(seed);
        assert_eq!(pool.reconcile(), Ok(()));
        net.borrow().deterministic_disk_image()
    }

    fn durable_namespace_catalog() -> NsCatalog {
        NsCatalog::new(
            NsId::new(18),
            vec![
                NsSpec {
                    id: NsId::new(16),
                    name: b"ledger".to_vec(),
                    mode: NsMode::Durable,
                    fsync: Some(NsFsyncPolicy::Always),
                    policy: None,
                    maxmemory: None,
                },
                NsSpec {
                    id: NsId::new(17),
                    name: b"sessions".to_vec(),
                    mode: NsMode::Durable,
                    fsync: Some(NsFsyncPolicy::Everysec),
                    policy: None,
                    maxmemory: None,
                },
            ],
        )
        .expect("durable namespace catalog")
    }

    #[test]
    fn file_create_dir_open_and_sync_honors_parent_dirs() {
        let net = CellNet::new(0, 5, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 256);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);

        driver.push(IoOp::FileCreateDir {
            dir: root_fd,
            name: "cell-0".to_string(),
            mode: 0o755,
            token: token(2),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        let root_inode = net.borrow().fds[&root_fd];
        assert!(!net.borrow().nodes[&root_inode].synced);

        driver.push(IoOp::FileSync { fd: root_fd, mode: FileSyncMode::Full, token: token(3) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));
        assert!(net.borrow().nodes[&root_inode].synced);

        let cell_fd = open_dir(&mut driver, &mut pool, root_fd, "cell-0", 4);
        driver.push(IoOp::FileCreateDir {
            dir: cell_fd,
            name: "log".to_string(),
            mode: 0o700,
            token: token(5),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        let log_inode = net.borrow().paths["./cell-0/log"];
        assert_eq!(net.borrow().nodes[&log_inode].kind, SimNodeKind::Directory);
        assert!(!net.borrow().nodes[&net.borrow().fds[&cell_fd]].synced);
        let _log_fd = open_dir(&mut driver, &mut pool, cell_fd, "log", 6);
    }

    #[test]
    fn namespace_catalog_survives_sim_power_cut_after_publish() {
        let net = CellNet::new(0, 0x51A7, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 64);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        driver.push(IoOp::FileCreateDir {
            dir: root_fd,
            name: "data".to_string(),
            mode: 0o700,
            token: token(2),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));
        sync_fd(&mut driver, &mut pool, root_fd, FileSyncMode::Full, 3);
        let catalog = durable_namespace_catalog();
        let mut completions = Vec::new();

        publish_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &catalog,
            &NamespaceCatalogDataRootPublishConfig::new("data".to_string(), 5).with_max_reaps(4),
            &mut completions,
        )
        .expect("publish namespace catalog");

        assert!(completions.is_empty());
        net.borrow_mut().power_cut(0xA11CE);
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &NamespaceCatalogDataRootLoadConfig::new("data".to_string(), 6).with_max_reaps(4),
            &mut completions,
        )
        .expect("load namespace catalog after power cut");

        assert_eq!(loaded, catalog);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn recovery_manifest_survives_sim_power_cut_after_publish() {
        let net = CellNet::new(0, 0x5A54, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 64);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        let manifest = recovery_manifest();
        let mut completions = Vec::new();

        publish_recovery_manifest(
            &mut driver,
            &mut pool,
            &manifest,
            RecoveryManifestPublishConfig::new(root_fd, M2_RECOVERY_MANIFEST_TOKEN_SLOT)
                .with_max_reaps(4),
            &mut completions,
        )
        .expect("publish recovery manifest");

        assert!(completions.is_empty());
        net.borrow_mut().power_cut(0xA11C_E054);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 2);
        let loaded = load_recovery_manifest(
            &mut driver,
            &mut pool,
            RecoveryManifestLoadConfig::new(root_fd, M2_RECOVERY_MANIFEST_TOKEN_SLOT + 1)
                .with_max_reaps(4),
            &mut completions,
        )
        .expect("load recovery manifest after power cut");

        assert_eq!(loaded, Some(manifest));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn checkpoint_image_survives_sim_power_cut_after_publish() {
        let net = CellNet::new(0, 0x1C4B, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 32);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        let checkpoint = CheckpointRef::new(CheckpointId::new(7).unwrap(), Lsn::new(2, 64));
        let namespaces = [NamespaceId::new(1), NamespaceId::new(16)];
        let catalog = b"catalog:v1:ns=1,16";
        let records = b"records:v1:key=user:7,value=paid";
        let sections = [
            CheckpointSectionRef::new(0, CheckpointSectionKind::NamespaceCatalog, catalog).unwrap(),
            CheckpointSectionRef::new(1, CheckpointSectionKind::Records, records).unwrap(),
        ];
        let header =
            CheckpointHeader::new(CellId(0), checkpoint, sections.len() as u32, &namespaces)
                .unwrap();
        let mut completions = Vec::new();

        let published = publish_checkpoint_image(
            &mut driver,
            &mut pool,
            header,
            &sections,
            CheckpointImagePublishConfig::new(root_fd, M2_CHECKPOINT_IMAGE_TOKEN_SLOT)
                .with_max_reaps(4),
            &mut completions,
        )
        .expect("publish checkpoint image");

        assert!(completions.is_empty());
        net.borrow_mut().power_cut(0xA11C_E055);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 2);
        let loaded = load_checkpoint_image(
            &mut driver,
            &mut pool,
            checkpoint,
            CheckpointImageLoadConfig::new(root_fd, M2_CHECKPOINT_IMAGE_TOKEN_SLOT + 1)
                .with_max_reaps(4),
            &mut completions,
        )
        .expect("load checkpoint image after power cut");

        assert_eq!(loaded.cell(), CellId(0));
        assert_eq!(loaded.checkpoint(), checkpoint);
        assert_eq!(loaded.namespaces(), namespaces.as_slice());
        assert_eq!(loaded.sections().len(), sections.len());
        assert_eq!(loaded.sections()[0].kind(), CheckpointSectionKind::NamespaceCatalog);
        assert_eq!(loaded.sections()[0].payload_len(), catalog.len() as u32);
        assert_eq!(loaded.sections()[1].kind(), CheckpointSectionKind::Records);
        assert_eq!(loaded.sections()[1].payload_len(), records.len() as u32);
        assert_eq!(loaded.footer(), published.footer());
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn durable_named_namespace_log_survives_sim_power_cut_and_replays() {
        let net = CellNet::new(0, 0xD2D2, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(8, 4096);
        let log_config = m2_log_config();
        let catalog = durable_namespace_catalog();
        let mut completions = Vec::new();

        let mut writer = open_first_boot_log_writer_in_data_root(
            &mut driver,
            &mut pool,
            &log_config,
            &mut completions,
        )
        .expect("first boot log writer in sim data root");
        publish_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &catalog,
            &m2_namespace_publish_config(),
            &mut completions,
        )
        .expect("publish durable namespace catalog");

        let mut durability = DurabilityCell::with_capacity(1024).unwrap();
        durability
            .stage_mutation_effect(
                NamespaceId::new(16),
                MutationEffect::StringPostImage {
                    key: b"order:1",
                    value: b"paid",
                    expire_at_ms: None,
                    raw: false,
                },
            )
            .expect("stage named durable mutation");
        let written = drive_synced_log_frame(&mut driver, &mut pool, &mut writer, &mut durability);
        assert_eq!(written.frame_start(), inf_log::Lsn::new(0, 0));

        net.borrow_mut().power_cut(0xA11C_E042);

        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load namespace catalog after sim power cut");
        assert_eq!(loaded, catalog);

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install recovered namespace catalog");
        let (recovered_writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &log_config,
            &single_segment_catalog(),
            &mut recovered,
            &mut completions,
        )
        .expect("replay active log tail after sim power cut");

        assert_eq!(recovered_writer.active_segment(), SegmentId::ZERO);
        assert_eq!(recovered_writer.active_offset_bytes(), written.frame_end().offset());
        assert_eq!(replay.frames, 1);
        assert_eq!(replay.records, 1);
        assert_eq!(
            recovered
                .durable_named_db_mut(NsId::new(16))
                .expect("recovered durable namespace")
                .get(b"order:1", Nanos(0)),
            Some(b"paid".as_slice())
        );
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn public_durable_namespace_write_recovers_after_sim_power_cut() {
        let net = CellNet::new(0, 0xD244, Plant::None);
        {
            let mut cell = PublicSimCell::first_boot(Rc::clone(&net), 0xD244);
            let fd = cell.connect();
            cell.request(
                fd,
                &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable", b"FSYNC", b"always"],
                b"+OK\r\n",
            );
            cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
            cell.request(fd, &[b"SET", b"order:1", b"paid"], b"+OK\r\n");
            assert!(cell.plane.durability_watermark() > 0);
            cell.close(fd);
        }

        net.borrow_mut().power_cut(0xA11C_E044);

        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(128, 4096);
        let mut completions = Vec::new();
        let loaded = load_namespace_catalog_in_data_root(
            &mut driver,
            &mut pool,
            &m2_namespace_load_config(),
            &mut completions,
        )
        .expect("load public namespace catalog after sim power cut");
        assert!(loaded.specs().iter().any(|spec| {
            spec.id == NsId::new(16)
                && spec.name == b"ledger"
                && spec.mode == NsMode::Durable
                && spec.fsync == Some(NsFsyncPolicy::Always)
        }));

        let mut recovered = Keyspace::new(StoreConfig::default());
        recovered
            .ns_replace_with_recovered_catalog(loaded)
            .expect("install public recovered namespace catalog");
        let (writer, replay) = open_recovered_log_writer_replaying_in_data_root(
            &mut driver,
            &mut pool,
            &m2_log_config(),
            &single_segment_catalog(),
            &mut recovered,
            &mut completions,
        )
        .expect("public active-tail replay after sim power cut");
        assert_eq!(replay.frames, 1);
        assert_eq!(replay.records, 1);
        assert_eq!(pool.reconcile(), Ok(()));

        let mut cell = PublicSimCell::recovered(Rc::clone(&net), 0xD244, recovered, writer);
        let fd = cell.connect();
        cell.request(fd, &[b"INF.NS", b"USE", b"ledger"], b"+OK\r\n");
        cell.request(fd, &[b"GET", b"order:1"], b"$4\r\npaid\r\n");
        cell.close(fd);
    }

    #[test]
    fn public_manifest_checkpoint_tail_recovery_serves_checkpoint_and_tail_after_restart() {
        let report = public_manifest_checkpoint_tail_recovery_report(0xD265);

        assert_eq!(report.checkpoint_records, 1);
        assert_eq!(report.replay_frames, 2);
        assert_eq!(report.replay_records, 1);
        assert!(report.active_offset_bytes > 0);
    }

    #[test]
    fn public_always_batched_pipeline_power_cut_replays_one_frame() {
        let report =
            super::crash_matrix::public_always_batched_pipeline_recovery_report(0xBA7C0017);

        assert_eq!(report.replay_frames, 1);
        assert_eq!(report.replay_records, 2);
        assert_eq!(report.survived_keys, 2);
        assert!(report.watermark_at_cut > 0);
    }

    #[test]
    fn public_power_cut_after_seal_recovers_rotated_segment() {
        let report = super::crash_matrix::public_power_cut_after_seal_recovery_report(0xD272);

        assert_eq!(report.replay_frames, 3);
        assert_eq!(report.replay_records, 3);
        assert_eq!(report.active_segment, 1);
        assert!(report.active_offset_bytes > 0);
        assert_eq!(report.sealed_segment_len_bytes, 256);
        assert_eq!(report.survived_keys, 3);
        assert!(report.watermark_at_cut > 0);
    }

    #[test]
    fn public_power_cut_after_non_exact_seal_recovers_truncated_segment() {
        let report =
            super::crash_matrix::public_power_cut_after_non_exact_seal_recovery_report(0xD273);

        assert_eq!(report.replay_frames, 3);
        assert_eq!(report.replay_records, 3);
        assert_eq!(report.active_segment, 1);
        assert!(report.active_offset_bytes > 0);
        assert_eq!(report.sealed_segment_len_bytes, 208);
        assert_eq!(report.survived_keys, 3);
        assert!(report.watermark_at_cut > 0);
    }

    #[test]
    fn public_log_append_write_fault_fail_stop() {
        let report = super::crash_matrix::public_log_append_write_fault_report(0xD279, 0xA11C_E079);

        assert!(report.fail_stopped);
        assert!(report.panic_message.contains("file write"));
        assert!(report.panic_message.contains("errno 5"));
        assert_eq!(report.reply_bytes_before_fail_stop, 0);
        assert_eq!(report.watermark_before_fail_stop, 0);
        assert_eq!(report.replay_frames, 0);
        assert_eq!(report.replay_records, 0);
        assert!(!report.value_survived);
    }

    #[test]
    fn public_fsync_err_restart_recovers_previous_watermark() {
        let report =
            super::crash_matrix::public_fsync_err_restart_watermark_report(0xF5E10081, 0xF5E18000);

        assert!(report.fail_stopped);
        assert!(report.panic_message.contains("fdatasync"));
        assert!(report.panic_message.contains("errno 5"));
        assert!(report.previous_watermark > 0);
        assert_eq!(report.watermark_before_fail_stop, report.previous_watermark);
        assert_eq!(report.reply_bytes_before_fail_stop, 0);
        assert_eq!(report.replay_frames, 1);
        assert_eq!(report.replay_records, 1);
        assert!(report.stable_value_survived);
        assert!(report.failed_value_absent);
        assert_eq!(u64::from(report.active_offset_bytes), report.previous_watermark);
    }

    #[test]
    fn public_torn_final_frame_recovers_stable_prefix() {
        let report = super::crash_matrix::public_torn_final_frame_recovery_report(0xD277);

        assert!(report.torn_tail_truncated);
        assert!(report.stable_value_survived);
        assert!(report.corrupt_value_absent);
        assert_eq!(report.replay_frames, 1);
        assert_eq!(report.replay_records, 1);
        assert_eq!(report.active_offset_bytes, report.stable_frame_end_bytes);
        assert!(report.corrupt_frame_offset_bytes >= report.stable_frame_end_bytes);
    }

    #[test]
    fn public_active_tail_later_magic_truncates_prefix() {
        let report = super::crash_matrix::public_active_tail_later_magic_recovery_report(0xD278);

        assert!(report.active_tail_truncated);
        assert!(report.stable_value_survived);
        assert!(report.corrupt_value_absent);
        assert!(report.later_value_absent);
        assert_eq!(report.replay_frames, 1);
        assert_eq!(report.replay_records, 1);
        assert_eq!(report.active_offset_bytes, report.stable_frame_end_bytes);
        assert!(report.later_frame_offset_bytes > report.corrupt_offset_bytes);
    }

    #[test]
    fn public_preallocate_enospc_degrades_durable_writes_only() {
        let report = super::crash_matrix::public_preallocate_enospc_degrade_report(0xD274);

        assert!(report.durable_refused);
        assert!(report.memory_served_after_degrade);
        assert_eq!(report.replay_frames, 1);
        assert_eq!(report.replay_records, 1);
        assert!(report.durable_value_survived);
        assert!(report.refused_value_absent);
        assert!(report.memory_value_absent_after_restart);
        assert!(report.watermark_at_degrade > 0);
    }

    #[test]
    fn public_manifest_rename_fail_recovers_with_full_log_replay() {
        let report = super::crash_matrix::public_manifest_rename_fail_recovery_report(0xD266);

        assert_eq!(report.checkpoint_records, 1);
        assert!(!report.manifest_present_after_recovery);
        assert_eq!(report.replay_frames, 3);
        assert_eq!(report.replay_records, 2);
        assert!(report.active_offset_bytes > 0);
    }

    #[test]
    fn public_manifest_dir_fsync_fail_recovers_with_full_log_replay() {
        let report = super::crash_matrix::public_manifest_dir_fsync_fail_recovery_report(0xD267);

        assert_eq!(report.checkpoint_records, 1);
        assert!(!report.manifest_present_after_recovery);
        assert_eq!(report.replay_frames, 3);
        assert_eq!(report.replay_records, 2);
        assert!(report.active_offset_bytes > 0);
    }

    #[test]
    fn public_live_checkpoint_wait_dir_fsync_fail_stops_before_reply() {
        let report = super::crash_matrix::public_live_checkpoint_wait_dir_fsync_fail_report(0xD290);

        assert!(report.fail_stopped);
        assert!(report.panic_message.contains("checkpoint directory"));
        assert!(report.panic_message.contains("errno 5"));
        assert_eq!(report.reply_bytes_before_fail_stop, 0);
        assert!(report.watermark_before_fail_stop > 0);
        assert!(!report.manifest_present_after_recovery);
        assert_eq!(report.replay_frames, 3);
        assert_eq!(report.replay_records, 2);
        assert!(report.before_value_survived);
        assert!(report.tail_value_survived);
        assert!(report.active_offset_bytes > 0);
    }

    #[test]
    fn public_manifest_replacement_dir_fsync_fail_preserves_old_manifest() {
        let report =
            super::crash_matrix::public_manifest_replacement_dir_fsync_fail_recovery_report(0xD268);

        assert_eq!(report.old_checkpoint_records, 1);
        assert_eq!(report.new_checkpoint_records, 1);
        assert!(report.loaded_old_manifest);
        assert_eq!(report.replay_frames, 4);
        assert_eq!(report.replay_records, 2);
        assert!(report.active_offset_bytes > 0);
    }

    #[test]
    fn public_checkpoint_write_enospc_preserves_old_manifest() {
        let report = super::crash_matrix::public_checkpoint_write_enospc_recovery_report(0xD275);

        assert_eq!(report.old_checkpoint_records, 1);
        assert!(report.checkpoint_publish_failed_enospc);
        assert!(report.loaded_old_manifest);
        assert_eq!(report.replay_frames, 3);
        assert_eq!(report.replay_records, 1);
        assert!(report.active_offset_bytes > 0);
    }

    #[test]
    fn public_manifest_replacement_rename_fail_preserves_old_manifest() {
        let report =
            super::crash_matrix::public_manifest_replacement_rename_fail_recovery_report(0xD269);

        assert_eq!(report.old_checkpoint_records, 1);
        assert_eq!(report.new_checkpoint_records, 1);
        assert!(report.loaded_old_manifest);
        assert_eq!(report.replay_frames, 4);
        assert_eq!(report.replay_records, 2);
        assert!(report.active_offset_bytes > 0);
    }

    #[test]
    fn public_durable_namespace_seed_sweep_recovers_deterministic_digests() {
        let config = crate::durability::DurabilitySweepConfig {
            seed: 0xD255_0000,
            seeds: PUBLIC_DIGEST_SWEEP_SEEDS,
            writes_per_seed: PUBLIC_DIGEST_SWEEP_OPS as u64,
            key_space: PUBLIC_DIGEST_SWEEP_KEYS,
            ..crate::durability::DurabilitySweepConfig::ci(0xD255_0000)
        };
        let first = super::crash_matrix::run_public_durable_recovery_sweep(&config);
        let second = super::crash_matrix::run_public_durable_recovery_sweep(&config);

        assert_eq!(second.manifest, first.manifest);
        assert_eq!(second.manifest_hash, first.manifest_hash);
        assert_eq!(first.manifest.len(), PUBLIC_DIGEST_SWEEP_SEEDS as usize * 48);
        assert_ne!(first.manifest_hash, 0);
    }

    #[test]
    fn public_everysec_seed_sweep_covers_loss_window_and_timer_fsync() {
        let config = crate::durability::DurabilitySweepConfig {
            seed: 0xE5EC_0000,
            seeds: PUBLIC_EVERYSEC_LOSS_SEARCH_SEEDS,
            writes_per_seed: 1,
            key_space: 1,
            ..crate::durability::DurabilitySweepConfig::ci(0xE5EC_0000)
        };
        let first = super::crash_matrix::run_public_everysec_recovery_sweep(&config);
        let second = super::crash_matrix::run_public_everysec_recovery_sweep(&config);

        assert!(first.ok());
        assert_eq!(second.manifest, first.manifest);
        assert_eq!(second.manifest_hash, first.manifest_hash);
        assert_eq!(first.pre_timer_loss_cases + first.pre_timer_survival_cases, config.seeds);
        assert_eq!(first.post_timer_survival_cases, config.seeds);
        assert_eq!(first.manifest.len(), PUBLIC_EVERYSEC_LOSS_SEARCH_SEEDS as usize * 58);
        assert_ne!(first.manifest_hash, 0);
    }

    #[test]
    fn public_everysec_workload_sweep_recovers_valid_prefixes() {
        let config = crate::durability::DurabilitySweepConfig {
            seed: 0xE5EC_8500,
            seeds: PUBLIC_EVERYSEC_WORKLOAD_SWEEP_SEEDS,
            writes_per_seed: PUBLIC_DIGEST_SWEEP_OPS as u64,
            key_space: PUBLIC_DIGEST_SWEEP_KEYS,
            ..crate::durability::DurabilitySweepConfig::ci(0xE5EC_8500)
        };
        let first = super::crash_matrix::run_public_everysec_workload_sweep(&config);
        let second = super::crash_matrix::run_public_everysec_workload_sweep(&config);

        assert!(first.ok());
        assert_eq!(second.manifest, first.manifest);
        assert_eq!(second.manifest_hash, first.manifest_hash);
        assert_eq!(
            first.loss_window_truncated_cases + first.loss_window_full_survival_cases,
            config.seeds
        );
        assert_eq!(first.full_flush_survival_cases, config.seeds);
        assert_eq!(first.manifest.len(), PUBLIC_EVERYSEC_WORKLOAD_SWEEP_SEEDS as usize * 104);
        assert_ne!(first.manifest_hash, 0);
    }

    #[test]
    fn m2_crash_matrix_cli_runner_manifest_is_deterministic() {
        let first = super::crash_matrix::run_m2_crash_matrix_rows();
        assert!(first.ok(), "violations: {:?}", first.violations);
        assert_eq!(first.rows, 20);
        assert_ne!(first.manifest_hash, 0);

        let second = super::crash_matrix::run_m2_crash_matrix_rows();
        assert!(second.ok(), "violations: {:?}", second.violations);
        assert_eq!(second.manifest, first.manifest);
        assert_eq!(second.manifest_hash, first.manifest_hash);
    }

    #[test]
    fn m2_crash_matrix_runner_executes_public_ci_rows() {
        let expected = BTreeSet::from([
            "public_always_single_write_power_cut".to_string(),
            "public_always_batched_pipeline_power_cut".to_string(),
            "public_everysec_single_write_contract".to_string(),
            "public_always_single_write_fsync_err_fail_stop".to_string(),
            "public_fsync_err_after_prior_frame_recovers_previous_watermark".to_string(),
            "public_log_append_write_fault_fail_stop".to_string(),
            "public_power_cut_after_seal_recovers_rotated_segment".to_string(),
            "public_power_cut_after_non_exact_seal_recovers_truncated_segment".to_string(),
            "public_torn_final_frame_recovers_stable_prefix".to_string(),
            "public_active_tail_later_magic_truncates_prefix".to_string(),
            "public_manifest_checkpoint_tail_power_cut".to_string(),
            "public_manifest_rename_fail_full_log_recovery".to_string(),
            "public_manifest_dir_fsync_fail_full_log_recovery".to_string(),
            "public_live_checkpoint_wait_dir_fsync_fail_no_reply".to_string(),
            "public_checkpoint_write_enospc_preserves_old_manifest".to_string(),
            "public_manifest_replacement_dir_fsync_fail_preserves_old_manifest".to_string(),
            "public_manifest_replacement_rename_fail_preserves_old_manifest".to_string(),
            "public_always_recovered_state_sweep".to_string(),
            "public_everysec_loss_window_sweep".to_string(),
            "public_everysec_workload_sweep".to_string(),
        ]);
        let mut ran = BTreeSet::new();

        for row in matrix_runner_rows() {
            let id = matrix_string(&row, "id");
            assert_eq!(matrix_string(&row, "status"), "ci-green");
            match id {
                "public_always_single_write_power_cut" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "torn_frame");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "single_write");
                    let report = public_always_recovery_report(matrix_seed(&row, "seed"));
                    assert!(report.value_survived);
                    assert!(report.watermark_at_cut > 0);
                    assert_eq!(report.replay_frames, 1);
                    assert_eq!(report.replay_records, 1);
                }
                "public_always_batched_pipeline_power_cut" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "torn_frame");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "batched_pipeline");
                    let report =
                        super::crash_matrix::public_always_batched_pipeline_recovery_report(
                            matrix_seed(&row, "seed"),
                        );
                    assert_eq!(report.survived_keys, 2);
                    assert!(report.watermark_at_cut > 0);
                    assert_eq!(report.replay_frames, 1);
                    assert_eq!(report.replay_records, 2);
                }
                "public_everysec_single_write_contract" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "torn_frame");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "everysec");
                    assert_eq!(matrix_string(&row, "workload"), "single_write");
                    let synced = public_everysec_recovery_report(matrix_seed(&row, "seed"), true);
                    assert!(synced.value_survived);
                    assert!(synced.watermark_at_cut > 0);
                    assert_eq!(synced.replay_frames, 1);
                    assert_eq!(synced.replay_records, 1);

                    let base = matrix_seed(&row, "loss_seed_base");
                    let count = matrix_seed(&row, "loss_seed_count");
                    let mut lost = None;
                    for offset in 0..count {
                        let report = public_everysec_recovery_report(base ^ offset, false);
                        assert_eq!(report.watermark_at_cut, 0);
                        if !report.value_survived {
                            lost = Some(report);
                            break;
                        }
                    }
                    let lost = lost.expect("runner row must realize everysec loss window");
                    assert_eq!(lost.replay_frames, 0);
                    assert_eq!(lost.replay_records, 0);
                }
                "public_always_recovered_state_sweep" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "torn_frame");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "public_recovered_state_sweep");
                    let config = test_sweep_config_from_row(&row);
                    let report = super::crash_matrix::run_public_durable_recovery_sweep(&config);
                    assert_eq!(report.manifest_hash, matrix_seed(&row, "expected_hash"));
                    assert_eq!(report.manifest.len(), report.seeds as usize * 48);
                }
                "public_everysec_loss_window_sweep" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "torn_frame");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "everysec");
                    assert_eq!(matrix_string(&row, "workload"), "public_recovered_state_sweep");
                    let config = test_sweep_config_from_row(&row);
                    let report = super::crash_matrix::run_public_everysec_recovery_sweep(&config);
                    assert!(report.ok());
                    assert_eq!(report.manifest_hash, matrix_seed(&row, "expected_hash"));
                    assert_eq!(report.manifest.len(), report.seeds as usize * 58);
                }
                "public_everysec_workload_sweep" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "torn_frame");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "everysec");
                    assert_eq!(matrix_string(&row, "workload"), "public_everysec_workload_sweep");
                    let config = test_sweep_config_from_row(&row);
                    let report = super::crash_matrix::run_public_everysec_workload_sweep(&config);
                    assert!(report.ok());
                    assert_eq!(report.manifest_hash, matrix_seed(&row, "expected_hash"));
                    assert_eq!(report.manifest.len(), report.seeds as usize * 104);
                }
                "public_always_single_write_fsync_err_fail_stop" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "fsync_err");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "single_write");
                    let seed = matrix_seed(&row, "seed");
                    let base = matrix_seed(&row, "loss_seed_base");
                    let count = matrix_seed(&row, "loss_seed_count");
                    let mut recovered_pre_batch = None;
                    for offset in 0..count {
                        let report =
                            super::crash_matrix::public_fsync_err_report(seed, base ^ offset);
                        assert!(report.fail_stopped);
                        assert!(report.panic_message.contains("fdatasync"));
                        assert!(report.panic_message.contains("errno 5"));
                        assert_eq!(report.reply_bytes_before_fail_stop, 0);
                        assert_eq!(report.watermark_before_fail_stop, 0);
                        if !report.value_survived
                            && report.replay_frames == 0
                            && report.replay_records == 0
                        {
                            recovered_pre_batch = Some(report);
                            break;
                        }
                    }
                    let report = recovered_pre_batch
                        .expect("fsync_err row must realize a pre-batch recovery seed");
                    assert_ne!(report.power_cut_seed, 0);
                }
                "public_fsync_err_after_prior_frame_recovers_previous_watermark" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "fsync_err");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "restart_watermark");
                    let seed = matrix_seed(&row, "seed");
                    let base = matrix_seed(&row, "loss_seed_base");
                    let count = matrix_seed(&row, "loss_seed_count");
                    let mut recovered_prefix = None;
                    for offset in 0..count {
                        let report = super::crash_matrix::public_fsync_err_restart_watermark_report(
                            seed,
                            base ^ offset,
                        );
                        assert!(report.fail_stopped);
                        assert!(report.panic_message.contains("fdatasync"));
                        assert!(report.panic_message.contains("errno 5"));
                        assert_eq!(report.reply_bytes_before_fail_stop, 0);
                        assert!(report.previous_watermark > 0);
                        assert_eq!(report.watermark_before_fail_stop, report.previous_watermark);
                        if report.stable_value_survived && report.failed_value_absent {
                            recovered_prefix = Some(report);
                            break;
                        }
                    }
                    let report =
                        recovered_prefix.expect("restart-watermark row must recover prefix only");
                    assert_eq!(report.replay_frames, 1);
                    assert_eq!(report.replay_records, 1);
                    assert_eq!(u64::from(report.active_offset_bytes), report.previous_watermark);
                }
                "public_log_append_write_fault_fail_stop" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "log_append_short_write");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "single_write");
                    assert_eq!(
                        matrix_string(&row, "oracle_scope"),
                        "terminal_file_write_error_under_current_backend_contract"
                    );
                    let report = super::crash_matrix::public_log_append_write_fault_report(
                        matrix_seed(&row, "seed"),
                        matrix_seed(&row, "power_cut_seed"),
                    );
                    assert!(report.fail_stopped);
                    assert!(report.panic_message.contains("file write"));
                    assert!(report.panic_message.contains("errno 5"));
                    assert_eq!(report.reply_bytes_before_fail_stop, 0);
                    assert_eq!(report.watermark_before_fail_stop, 0);
                    assert_eq!(report.replay_frames, 0);
                    assert_eq!(report.replay_records, 0);
                    assert!(!report.value_survived);
                }
                "public_power_cut_after_seal_recovers_rotated_segment" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "power_cut_after_seal");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "segment_recovery");
                    let report = super::crash_matrix::public_power_cut_after_seal_recovery_report(
                        matrix_seed(&row, "seed"),
                    );
                    assert_eq!(report.replay_frames, 3);
                    assert_eq!(report.replay_records, 3);
                    assert_eq!(report.active_segment, 1);
                    assert!(report.active_offset_bytes > 0);
                    assert_eq!(report.sealed_segment_len_bytes, 256);
                    assert_eq!(report.survived_keys, 3);
                }
                "public_power_cut_after_non_exact_seal_recovers_truncated_segment" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "power_cut_after_seal");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "segment_recovery");
                    let report =
                        super::crash_matrix::public_power_cut_after_non_exact_seal_recovery_report(
                            matrix_seed(&row, "seed"),
                        );
                    assert_eq!(report.replay_frames, 3);
                    assert_eq!(report.replay_records, 3);
                    assert_eq!(report.active_segment, 1);
                    assert!(report.active_offset_bytes > 0);
                    assert_eq!(report.sealed_segment_len_bytes, 208);
                    assert_eq!(report.survived_keys, 3);
                }
                "public_torn_final_frame_recovers_stable_prefix" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "torn_frame");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "segment_recovery");
                    assert_eq!(matrix_string(&row, "path"), "torn_final_frame");
                    let report = super::crash_matrix::public_torn_final_frame_recovery_report(
                        matrix_seed(&row, "seed"),
                    );
                    assert!(report.torn_tail_truncated);
                    assert!(report.stable_value_survived);
                    assert!(report.corrupt_value_absent);
                    assert_eq!(report.replay_frames, 1);
                    assert_eq!(report.replay_records, 1);
                    assert_eq!(report.active_offset_bytes, report.stable_frame_end_bytes);
                }
                "public_active_tail_later_magic_truncates_prefix" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "torn_frame");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "segment_recovery");
                    assert_eq!(matrix_string(&row, "path"), "active_tail_later_magic");
                    let report =
                        super::crash_matrix::public_active_tail_later_magic_recovery_report(
                            matrix_seed(&row, "seed"),
                        );
                    assert!(report.active_tail_truncated);
                    assert!(report.stable_value_survived);
                    assert!(report.corrupt_value_absent);
                    assert!(report.later_value_absent);
                    assert_eq!(report.replay_frames, 1);
                    assert_eq!(report.replay_records, 1);
                    assert_eq!(report.active_offset_bytes, report.stable_frame_end_bytes);
                    assert!(report.later_frame_offset_bytes > report.corrupt_offset_bytes);
                }
                "public_manifest_checkpoint_tail_power_cut" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "power_cut_after_manifest");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "checkpoint_tail");
                    let report =
                        super::crash_matrix::public_manifest_checkpoint_tail_recovery_report(
                            matrix_seed(&row, "seed"),
                        );
                    assert_eq!(report.checkpoint_records, 1);
                    assert_eq!(report.replay_frames, 2);
                    assert_eq!(report.replay_records, 1);
                    assert!(report.active_offset_bytes > 0);
                }
                "public_manifest_rename_fail_full_log_recovery" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "manifest_rename_fail");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "checkpoint_tail");
                    let report = super::crash_matrix::public_manifest_rename_fail_recovery_report(
                        matrix_seed(&row, "seed"),
                    );
                    assert_eq!(report.checkpoint_records, 1);
                    assert!(!report.manifest_present_after_recovery);
                    assert_eq!(report.replay_frames, 3);
                    assert_eq!(report.replay_records, 2);
                    assert!(report.active_offset_bytes > 0);
                }
                "public_manifest_dir_fsync_fail_full_log_recovery" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "dir_fsync_fail");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "checkpoint_tail");
                    let report =
                        super::crash_matrix::public_manifest_dir_fsync_fail_recovery_report(
                            matrix_seed(&row, "seed"),
                        );
                    assert_eq!(report.checkpoint_records, 1);
                    assert!(!report.manifest_present_after_recovery);
                    assert_eq!(report.replay_frames, 3);
                    assert_eq!(report.replay_records, 2);
                    assert!(report.active_offset_bytes > 0);
                }
                "public_live_checkpoint_wait_dir_fsync_fail_no_reply" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "dir_fsync_fail");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "live_checkpoint_command");
                    let report =
                        super::crash_matrix::public_live_checkpoint_wait_dir_fsync_fail_report(
                            matrix_seed(&row, "seed"),
                        );
                    assert!(report.fail_stopped);
                    assert!(report.panic_message.contains("checkpoint directory"));
                    assert!(report.panic_message.contains("errno 5"));
                    assert_eq!(report.reply_bytes_before_fail_stop, 0);
                    assert!(report.watermark_before_fail_stop > 0);
                    assert!(!report.manifest_present_after_recovery);
                    assert_eq!(report.replay_frames, 3);
                    assert_eq!(report.replay_records, 2);
                    assert!(report.before_value_survived);
                    assert!(report.tail_value_survived);
                    assert!(report.active_offset_bytes > 0);
                }
                "public_checkpoint_write_enospc_preserves_old_manifest" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "checkpoint_write_enospc");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "manifest_replacement");
                    let report =
                        super::crash_matrix::public_checkpoint_write_enospc_recovery_report(
                            matrix_seed(&row, "seed"),
                        );
                    assert_eq!(report.old_checkpoint_records, 1);
                    assert!(report.checkpoint_publish_failed_enospc);
                    assert!(report.loaded_old_manifest);
                    assert_eq!(report.replay_frames, 3);
                    assert_eq!(report.replay_records, 1);
                    assert!(report.active_offset_bytes > 0);
                }
                "public_manifest_replacement_dir_fsync_fail_preserves_old_manifest" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "dir_fsync_fail");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "manifest_replacement");
                    let report = super::crash_matrix::public_manifest_replacement_dir_fsync_fail_recovery_report(
                        matrix_seed(&row, "seed"),
                    );
                    assert_eq!(report.old_checkpoint_records, 1);
                    assert_eq!(report.new_checkpoint_records, 1);
                    assert!(report.loaded_old_manifest);
                    assert_eq!(report.replay_frames, 4);
                    assert_eq!(report.replay_records, 2);
                    assert!(report.active_offset_bytes > 0);
                }
                "public_manifest_replacement_rename_fail_preserves_old_manifest" => {
                    assert_eq!(matrix_string(&row, "fault_point"), "manifest_rename_fail");
                    assert_eq!(matrix_string(&row, "fsync_policy"), "always");
                    assert_eq!(matrix_string(&row, "workload"), "manifest_replacement");
                    let report =
                        super::crash_matrix::public_manifest_replacement_rename_fail_recovery_report(
                            matrix_seed(&row, "seed"),
                        );
                    assert_eq!(report.old_checkpoint_records, 1);
                    assert_eq!(report.new_checkpoint_records, 1);
                    assert!(report.loaded_old_manifest);
                    assert_eq!(report.replay_frames, 4);
                    assert_eq!(report.replay_records, 2);
                    assert!(report.active_offset_bytes > 0);
                }
                other => panic!("unhandled M2 crash-matrix runner row {other}"),
            }
            assert!(ran.insert(id.to_string()), "duplicate runner row {id}");
        }

        assert_eq!(ran, expected);
    }

    #[test]
    fn public_everysec_namespace_loss_window_can_lose_acked_write_after_power_cut() {
        let mut lost = None;
        for offset in 0..PUBLIC_EVERYSEC_LOSS_SEARCH_SEEDS {
            let report = public_everysec_recovery_report(0xE5EC_0000 ^ offset, false);
            assert_eq!(report.watermark_at_cut, 0);
            if !report.value_survived {
                lost = Some(report);
                break;
            }
        }

        let report = lost.expect("bounded seed search must realize everysec loss window");
        assert_ne!(report.seed, 0);
        assert_eq!(report.replay_frames, 0);
        assert_eq!(report.replay_records, 0);
    }

    #[test]
    fn public_everysec_namespace_write_survives_after_timer_fsync() {
        let report = public_everysec_recovery_report(0xE5EC_0046, true);
        assert_eq!(report.seed, 0xE5EC_0046);
        assert!(report.watermark_at_cut > 0);
        assert!(report.value_survived);
        assert_eq!(report.replay_frames, 1);
        assert_eq!(report.replay_records, 1);
    }

    #[test]
    fn file_create_dir_fault_fires_before_state_mutation() {
        let net = CellNet::new(0, 6, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 256);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        net.borrow_mut().fail_next_file_op(SimFileOpKind::CreateDir, libc::EIO);

        driver.push(IoOp::FileCreateDir {
            dir: root_fd,
            name: "cell-0".to_string(),
            mode: 0o755,
            token: token(2),
        });
        assert!(matches!(
            reap_one(&mut driver, &mut pool),
            CompletionResult::Error { errno: libc::EIO, buf: None }
        ));
        assert!(!net.borrow().paths.contains_key("./cell-0"));

        driver.push(IoOp::FileCreateDir {
            dir: root_fd,
            name: "cell-0".to_string(),
            mode: 0o755,
            token: token(3),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));
        assert!(net.borrow().paths.contains_key("./cell-0"));
    }

    #[test]
    fn file_ops_create_preallocate_sync_rename_unlink() {
        let net = CellNet::new(0, 7, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 256);

        driver.push(IoOp::FileOpen {
            dir: libc::AT_FDCWD,
            name: "seg-000000.ilog".to_string(),
            mode: FileOpenMode::ReadWriteCreate,
            token: token(1),
        });
        let fd = match reap_one(&mut driver, &mut pool) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        };

        driver.push(IoOp::FilePreallocate { fd, len_bytes: 4096, token: token(2) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        driver.push(IoOp::FileSync { fd, mode: FileSyncMode::DataOnly, token: token(3) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        driver.push(IoOp::FileRename {
            old_dir: libc::AT_FDCWD,
            old_name: "seg-000000.ilog".to_string(),
            new_dir: libc::AT_FDCWD,
            new_name: "seg-000001.ilog".to_string(),
            token: token(4),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        driver.push(IoOp::FileUnlink {
            dir: libc::AT_FDCWD,
            name: "seg-000001.ilog".to_string(),
            token: token(5),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        driver.push(IoOp::FileClose { fd, token: token(6) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileClosed));
    }

    #[test]
    fn readwrite_open_requires_an_existing_file() {
        let net = CellNet::new(0, 8, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 256);
        let _fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");

        driver.push(IoOp::FileOpen {
            dir: libc::AT_FDCWD,
            name: "seg-000000.ilog".to_string(),
            mode: FileOpenMode::ReadWrite,
            token: token(2),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileOpened { .. }));

        driver.push(IoOp::FileOpen {
            dir: libc::AT_FDCWD,
            name: "seg-000001.ilog".to_string(),
            mode: FileOpenMode::ReadWrite,
            token: token(3),
        });
        assert!(matches!(
            reap_one(&mut driver, &mut pool),
            CompletionResult::Error { errno: libc::ENOENT, buf: None }
        ));
        assert!(!net.borrow().paths.contains_key("./seg-000001.ilog"));
    }

    #[test]
    fn readwrite_create_truncate_replaces_existing_bytes_exactly_after_sync() {
        let net = CellNet::new(0, 10, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 256);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        let fd = open_file(&mut driver, &mut pool, "META.tmp");
        sync_fd(&mut driver, &mut pool, root_fd, FileSyncMode::Full, 2);
        write_bytes(&mut driver, &mut pool, fd, 0, b"long-catalog-image", 3);
        sync_fd(&mut driver, &mut pool, fd, FileSyncMode::DataOnly, 4);

        driver.push(IoOp::FileOpen {
            dir: libc::AT_FDCWD,
            name: "META.tmp".to_string(),
            mode: FileOpenMode::ReadWriteCreateTruncate,
            token: token(5),
        });
        let fd = match reap_one(&mut driver, &mut pool) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        };
        write_bytes(&mut driver, &mut pool, fd, 0, b"short", 6);
        sync_fd(&mut driver, &mut pool, fd, FileSyncMode::DataOnly, 7);

        net.borrow_mut().power_cut(8);

        let fd = match open_existing_file(&mut driver, &mut pool, "META.tmp", 9) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        };
        assert_eq!(read_bytes(&mut driver, &mut pool, fd, 64, 10), b"short");
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn file_truncate_changes_stable_length_only_after_file_sync() {
        let net = CellNet::new(0, 12, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 256);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        let fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");
        sync_fd(&mut driver, &mut pool, root_fd, FileSyncMode::Full, 2);
        write_bytes(&mut driver, &mut pool, fd, 0, b"abcdefgh", 3);
        sync_fd(&mut driver, &mut pool, fd, FileSyncMode::DataOnly, 4);

        driver.push(IoOp::FileTruncate { fd, len_bytes: 5, token: token(5) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));
        net.borrow_mut().power_cut(6);
        let fd = match open_existing_file(&mut driver, &mut pool, "seg-000000.ilog", 7) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        };
        assert_eq!(read_bytes(&mut driver, &mut pool, fd, 64, 8), b"abcdefgh");

        driver.push(IoOp::FileTruncate { fd, len_bytes: 5, token: token(9) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));
        sync_fd(&mut driver, &mut pool, fd, FileSyncMode::DataOnly, 10);
        net.borrow_mut().power_cut(11);
        let fd = match open_existing_file(&mut driver, &mut pool, "seg-000000.ilog", 12) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        };
        assert_eq!(read_bytes(&mut driver, &mut pool, fd, 64, 13), b"abcde");
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn open_fd_survives_path_rename() {
        let net = CellNet::new(0, 9, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 256);

        driver.push(IoOp::FileOpen {
            dir: libc::AT_FDCWD,
            name: "MANIFEST.tmp".to_string(),
            mode: FileOpenMode::ReadWriteCreate,
            token: token(1),
        });
        let fd = match reap_one(&mut driver, &mut pool) {
            CompletionResult::FileOpened { fd } => fd,
            other => panic!("unexpected completion {other:?}"),
        };
        driver.push(IoOp::FileRename {
            old_dir: libc::AT_FDCWD,
            old_name: "MANIFEST.tmp".to_string(),
            new_dir: libc::AT_FDCWD,
            new_name: "MANIFEST".to_string(),
            token: token(2),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        driver.push(IoOp::FilePreallocate { fd, len_bytes: 128, token: token(3) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));
    }

    #[test]
    fn file_faults_fire_before_state_mutation() {
        let net = CellNet::new(0, 11, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 256);
        let fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");
        net.borrow_mut().fail_next_file_op(SimFileOpKind::Preallocate, libc::ENOSPC);

        driver.push(IoOp::FilePreallocate { fd, len_bytes: 4096, token: token(2) });
        assert!(matches!(
            reap_one(&mut driver, &mut pool),
            CompletionResult::Error { errno: libc::ENOSPC, buf: None }
        ));

        let inode = net.borrow().fds[&fd];
        assert_eq!(net.borrow().nodes[&inode].len_bytes, 0);

        driver.push(IoOp::FilePreallocate { fd, len_bytes: 4096, token: token(3) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));
        assert_eq!(net.borrow().nodes[&inode].len_bytes, 4096);
    }

    #[test]
    fn file_sync_mode_fault_waits_for_matching_sync_mode() {
        let net = CellNet::new(0, 12, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 256);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        let fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");
        net.borrow_mut().fail_next_file_sync(FileSyncMode::Full, libc::EIO);

        driver.push(IoOp::FileSync { fd, mode: FileSyncMode::DataOnly, token: token(2) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        driver.push(IoOp::FileSync { fd: root_fd, mode: FileSyncMode::Full, token: token(3) });
        assert!(matches!(
            reap_one(&mut driver, &mut pool),
            CompletionResult::Error { errno: libc::EIO, buf: None }
        ));

        driver.push(IoOp::FileSync { fd: root_fd, mode: FileSyncMode::Full, token: token(4) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));
    }

    #[test]
    fn file_read_at_reads_offset_and_eof_byte_exact() {
        let net = CellNet::new(0, 13, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 16);
        let fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");
        let inode = net.borrow().fds[&fd];
        net.borrow_mut().nodes.get_mut(&inode).unwrap().bytes = b"abcdef".to_vec();

        let buf = pool.try_lease(LeaseKind::Recv).unwrap();
        driver.push(IoOp::FileReadAt { fd, offset_bytes: 2, buf, len: 3, token: token(2) });
        match reap_one(&mut driver, &mut pool) {
            CompletionResult::FileRead { buf: got, len } => {
                assert_eq!(got, buf);
                assert_eq!(len, 3);
                assert_eq!(&pool.bytes(got)[..len as usize], b"cde");
                pool.release(got);
            }
            other => panic!("unexpected completion {other:?}"),
        }

        let buf = pool.try_lease(LeaseKind::Recv).unwrap();
        driver.push(IoOp::FileReadAt { fd, offset_bytes: 99, buf, len: 3, token: token(3) });
        match reap_one(&mut driver, &mut pool) {
            CompletionResult::FileRead { buf: got, len } => {
                assert_eq!(got, buf);
                assert_eq!(len, 0);
                pool.release(got);
            }
            other => panic!("unexpected completion {other:?}"),
        }
    }

    #[test]
    fn file_read_fault_returns_the_leased_buffer() {
        let net = CellNet::new(0, 17, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 16);
        let fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");
        let buf = pool.try_lease(LeaseKind::Recv).unwrap();
        net.borrow_mut().fail_next_file_op(SimFileOpKind::Read, libc::EIO);

        driver.push(IoOp::FileReadAt { fd, offset_bytes: 0, buf, len: 8, token: token(2) });
        match reap_one(&mut driver, &mut pool) {
            CompletionResult::Error { errno: libc::EIO, buf: Some(got) } => {
                assert_eq!(got, buf);
                pool.release(got);
            }
            other => panic!("unexpected completion {other:?}"),
        }

        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn file_write_at_writes_offset_and_returns_the_leased_buffer() {
        let net = CellNet::new(0, 19, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 16);
        let fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");

        driver.push(IoOp::FilePreallocate { fd, len_bytes: 8, token: token(2) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        let buf = pool.try_lease(LeaseKind::Send).unwrap();
        pool.bytes_mut(buf)[..4].copy_from_slice(b"wxyz");
        driver.push(IoOp::FileWriteAt { fd, offset_bytes: 2, buf, len: 4, token: token(3) });
        match reap_one(&mut driver, &mut pool) {
            CompletionResult::FileWritten { buf: got } => {
                assert_eq!(got, buf);
                pool.release(got);
            }
            other => panic!("unexpected completion {other:?}"),
        }

        let inode = net.borrow().fds[&fd];
        assert_eq!(&net.borrow().nodes[&inode].bytes[..8], b"\0\0wxyz\0\0");
        assert!(!net.borrow().nodes[&inode].synced);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn file_write_fault_returns_buffer_without_state_mutation() {
        let net = CellNet::new(0, 23, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 16);
        let fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");
        driver.push(IoOp::FilePreallocate { fd, len_bytes: 8, token: token(2) });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));

        let inode = net.borrow().fds[&fd];
        let before = net.borrow().nodes[&inode].bytes.clone();
        let buf = pool.try_lease(LeaseKind::Send).unwrap();
        pool.bytes_mut(buf)[..4].copy_from_slice(b"fail");
        net.borrow_mut().fail_next_file_op(SimFileOpKind::Write, libc::EIO);

        driver.push(IoOp::FileWriteAt { fd, offset_bytes: 2, buf, len: 4, token: token(3) });
        match reap_one(&mut driver, &mut pool) {
            CompletionResult::Error { errno: libc::EIO, buf: Some(got) } => {
                assert_eq!(got, buf);
                pool.release(got);
            }
            other => panic!("unexpected completion {other:?}"),
        }

        assert_eq!(net.borrow().nodes[&inode].bytes, before);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn power_cut_keeps_synced_file_bytes_for_all_seeds() {
        for seed in 0..32 {
            let net = CellNet::new(0, seed, Plant::None);
            let mut driver = SimDriver::new(Rc::clone(&net));
            let mut pool = BufferPool::new(4, 16);
            let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
            let fd = open_file(&mut driver, &mut pool, "seg-000000.ilog");
            sync_fd(&mut driver, &mut pool, root_fd, FileSyncMode::Full, 2);
            write_bytes(&mut driver, &mut pool, fd, 0, b"sync", 3);
            sync_fd(&mut driver, &mut pool, fd, FileSyncMode::DataOnly, 4);

            net.borrow_mut().power_cut(seed);

            let fd = match open_existing_file(&mut driver, &mut pool, "seg-000000.ilog", 5) {
                CompletionResult::FileOpened { fd } => fd,
                other => panic!("unexpected completion {other:?}"),
            };
            assert_eq!(read_bytes(&mut driver, &mut pool, fd, 4, 6), b"sync");
        }
    }

    #[test]
    fn power_cut_applies_seeded_loss_tears_and_order_to_unsynced_writes() {
        let mut seen = BTreeSet::new();
        for seed in 0..64 {
            seen.insert(read_existing_sector_image(seed));
        }

        let base = vec![b'A'; SIM_DISK_SECTOR_BYTES * 2];
        let torn = [vec![b'B'; SIM_DISK_SECTOR_BYTES], vec![b'A'; SIM_DISK_SECTOR_BYTES]].concat();
        let full = vec![b'B'; SIM_DISK_SECTOR_BYTES * 2];
        let valid = [base.clone(), torn.clone(), full.clone()];

        assert!(seen.contains(base.as_slice()));
        assert!(seen.contains(torn.as_slice()));
        assert!(seen.contains(full.as_slice()));
        assert!(seen.iter().all(|image| valid.contains(image)));
        assert_eq!(read_existing_sector_image(0xBEEF), read_existing_sector_image(0xBEEF));
    }

    #[test]
    fn power_cut_same_seed_yields_byte_identical_disk_image() {
        let first = scripted_power_cut_disk_image(0xD15C_E018);
        let second = scripted_power_cut_disk_image(0xD15C_E018);

        assert!(!first.is_empty());
        assert_eq!(first, second);
    }

    #[test]
    fn surviving_write_len_is_sector_granular() {
        let mut seen = BTreeSet::new();
        for seed in 0..256 {
            let mut rng = SplitMix64::new(seed);
            seen.insert(surviving_write_len(SIM_DISK_SECTOR_BYTES * 2, &mut rng));
        }

        assert_eq!(seen, BTreeSet::from([0, SIM_DISK_SECTOR_BYTES, SIM_DISK_SECTOR_BYTES * 2]));
    }

    #[test]
    fn power_cut_reverts_unsynced_path_metadata_to_last_dir_fsync() {
        let net = CellNet::new(0, 37, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 16);
        let _root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        let _fd = open_file(&mut driver, &mut pool, "temp.ilog");

        net.borrow_mut().power_cut(1);

        assert!(matches!(
            open_existing_file(&mut driver, &mut pool, "temp.ilog", 2),
            CompletionResult::Error { errno: libc::ENOENT, buf: None }
        ));

        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 3);
        let _fd = open_file(&mut driver, &mut pool, "temp.ilog");
        sync_fd(&mut driver, &mut pool, root_fd, FileSyncMode::Full, 4);
        net.borrow_mut().power_cut(2);

        assert!(matches!(
            open_existing_file(&mut driver, &mut pool, "temp.ilog", 5),
            CompletionResult::FileOpened { .. }
        ));
    }

    #[test]
    fn power_cut_requires_dir_fsync_for_rename_durability() {
        let net = CellNet::new(0, 41, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 16);
        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 1);
        let _fd = open_file(&mut driver, &mut pool, "old.manifest");
        sync_fd(&mut driver, &mut pool, root_fd, FileSyncMode::Full, 2);

        driver.push(IoOp::FileRename {
            old_dir: root_fd,
            old_name: "old.manifest".to_string(),
            new_dir: root_fd,
            new_name: "MANIFEST".to_string(),
            token: token(3),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));
        net.borrow_mut().power_cut(3);

        assert!(matches!(
            open_existing_file(&mut driver, &mut pool, "old.manifest", 4),
            CompletionResult::FileOpened { .. }
        ));
        assert!(matches!(
            open_existing_file(&mut driver, &mut pool, "MANIFEST", 5),
            CompletionResult::Error { errno: libc::ENOENT, buf: None }
        ));

        let root_fd = open_dir(&mut driver, &mut pool, libc::AT_FDCWD, ".", 6);
        driver.push(IoOp::FileRename {
            old_dir: root_fd,
            old_name: "old.manifest".to_string(),
            new_dir: root_fd,
            new_name: "MANIFEST".to_string(),
            token: token(7),
        });
        assert!(matches!(reap_one(&mut driver, &mut pool), CompletionResult::FileDone));
        sync_fd(&mut driver, &mut pool, root_fd, FileSyncMode::Full, 8);
        net.borrow_mut().power_cut(4);

        assert!(matches!(
            open_existing_file(&mut driver, &mut pool, "old.manifest", 9),
            CompletionResult::Error { errno: libc::ENOENT, buf: None }
        ));
        assert!(matches!(
            open_existing_file(&mut driver, &mut pool, "MANIFEST", 10),
            CompletionResult::FileOpened { .. }
        ));
    }
}
