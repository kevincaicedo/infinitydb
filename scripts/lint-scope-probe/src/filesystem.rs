//! ADR-0144 D5: each plant must report its exact resolved API path.
//! No function is executed; even destructive calls are compiler inputs only.
#![allow(deprecated, unused_variables)]

pub struct HoldsFile {
    pub value: std::fs::File, // PLANT clippy::disallowed_types std::fs::File
}

pub struct HoldsOpenOptions {
    pub value: std::fs::OpenOptions, // PLANT clippy::disallowed_types std::fs::OpenOptions
}

pub struct HoldsReadDir {
    pub value: std::fs::ReadDir, // PLANT clippy::disallowed_types std::fs::ReadDir
}

pub struct HoldsDirEntry {
    pub value: std::fs::DirEntry, // PLANT clippy::disallowed_types std::fs::DirEntry
}

pub struct HoldsDirBuilder {
    pub value: std::fs::DirBuilder, // PLANT clippy::disallowed_types std::fs::DirBuilder
}

// Type plants are above; methods below must be judged independently.
#[allow(clippy::disallowed_types)]
pub mod methods {
    use std::path::Path;

    pub fn read(p: &Path) {
        let _ = std::fs::read(p); // PLANT clippy::disallowed_methods std::fs::read
    }

    pub fn read_to_string(p: &Path) {
        let _ = std::fs::read_to_string(p); // PLANT clippy::disallowed_methods std::fs::read_to_string
    }

    pub fn read_dir(p: &Path) {
        let _ = std::fs::read_dir(p); // PLANT clippy::disallowed_methods std::fs::read_dir
    }

    pub fn metadata(p: &Path) {
        let _ = std::fs::metadata(p); // PLANT clippy::disallowed_methods std::fs::metadata
    }

    pub fn symlink_metadata(p: &Path) {
        let _ = std::fs::symlink_metadata(p); // PLANT clippy::disallowed_methods std::fs::symlink_metadata
    }

    pub fn remove_file(p: &Path) {
        let _ = std::fs::remove_file(p); // PLANT clippy::disallowed_methods std::fs::remove_file
    }

    pub fn remove_dir(p: &Path) {
        let _ = std::fs::remove_dir(p); // PLANT clippy::disallowed_methods std::fs::remove_dir
    }

    pub fn remove_dir_all(p: &Path) {
        let _ = std::fs::remove_dir_all(p); // PLANT clippy::disallowed_methods std::fs::remove_dir_all
    }

    pub fn create_dir(p: &Path) {
        let _ = std::fs::create_dir(p); // PLANT clippy::disallowed_methods std::fs::create_dir
    }

    pub fn create_dir_all(p: &Path) {
        let _ = std::fs::create_dir_all(p); // PLANT clippy::disallowed_methods std::fs::create_dir_all
    }

    pub fn read_link(p: &Path) {
        let _ = std::fs::read_link(p); // PLANT clippy::disallowed_methods std::fs::read_link
    }

    pub fn canonicalize(p: &Path) {
        let _ = std::fs::canonicalize(p); // PLANT clippy::disallowed_methods std::fs::canonicalize
    }

    pub fn exists(p: &Path) {
        let _ = std::fs::exists(p); // PLANT clippy::disallowed_methods std::fs::exists
    }

    pub fn write(p: &Path) {
        let _ = std::fs::write(p, b"probe"); // PLANT clippy::disallowed_methods std::fs::write
    }

    pub fn rename(p: &Path) {
        let _ = std::fs::rename(p, p); // PLANT clippy::disallowed_methods std::fs::rename
    }

    pub fn copy(p: &Path) {
        let _ = std::fs::copy(p, p); // PLANT clippy::disallowed_methods std::fs::copy
    }

    pub fn hard_link(p: &Path) {
        let _ = std::fs::hard_link(p, p); // PLANT clippy::disallowed_methods std::fs::hard_link
    }

    pub fn soft_link(p: &Path) {
        let _ = std::fs::soft_link(p, p); // PLANT clippy::disallowed_methods std::fs::soft_link
    }

    pub fn set_permissions(p: &Path) {
        let _ = std::fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(0o600)); // PLANT clippy::disallowed_methods std::fs::set_permissions
    }

    pub fn path_exists(p: &Path) {
        let _ = p.exists(); // PLANT clippy::disallowed_methods std::path::Path::exists
    }

    pub fn path_try_exists(p: &Path) {
        let _ = p.try_exists(); // PLANT clippy::disallowed_methods std::path::Path::try_exists
    }

    pub fn path_is_dir(p: &Path) {
        let _ = p.is_dir(); // PLANT clippy::disallowed_methods std::path::Path::is_dir
    }

    pub fn path_is_file(p: &Path) {
        let _ = p.is_file(); // PLANT clippy::disallowed_methods std::path::Path::is_file
    }

    pub fn path_metadata(p: &Path) {
        let _ = p.metadata(); // PLANT clippy::disallowed_methods std::path::Path::metadata
    }

    pub fn path_symlink_metadata(p: &Path) {
        let _ = p.symlink_metadata(); // PLANT clippy::disallowed_methods std::path::Path::symlink_metadata
    }

    pub fn path_read_dir(p: &Path) {
        let _ = p.read_dir(); // PLANT clippy::disallowed_methods std::path::Path::read_dir
    }

    pub fn path_read_link(p: &Path) {
        let _ = p.read_link(); // PLANT clippy::disallowed_methods std::path::Path::read_link
    }

    pub fn path_canonicalize(p: &Path) {
        let _ = p.canonicalize(); // PLANT clippy::disallowed_methods std::path::Path::canonicalize
    }

    pub fn path_is_symlink(p: &Path) {
        let _ = p.is_symlink(); // PLANT clippy::disallowed_methods std::path::Path::is_symlink
    }

    pub fn builder_new(p: &Path) {
        let _ = std::fs::DirBuilder::new(); // PLANT clippy::disallowed_methods std::fs::DirBuilder::new
    }

    pub fn chown(p: &Path) {
        let _ = std::os::unix::fs::chown(p, None, None); // PLANT clippy::disallowed_methods std::os::unix::fs::chown
    }

    pub fn lchown(p: &Path) {
        let _ = std::os::unix::fs::lchown(p, None, None); // PLANT clippy::disallowed_methods std::os::unix::fs::lchown
    }

    pub fn symlink(p: &Path) {
        let _ = std::os::unix::fs::symlink(p, p); // PLANT clippy::disallowed_methods std::os::unix::fs::symlink
    }

    pub fn chroot(p: &Path) {
        let _ = std::os::unix::fs::chroot(p); // PLANT clippy::disallowed_methods std::os::unix::fs::chroot
    }

    pub fn builder_create(builder: &std::fs::DirBuilder, p: &Path) {
        let _ = builder.create(p); // PLANT clippy::disallowed_methods std::fs::DirBuilder::create
    }

    pub fn fchown(fd: std::os::fd::BorrowedFd<'_>) {
        let _ = std::os::unix::fs::fchown(fd, None, None); // PLANT clippy::disallowed_methods std::os::unix::fs::fchown
    }

    pub fn renamed_import(p: &Path) {
        use std::fs::read as slurp;
        let _ = slurp(p); // PLANT clippy::disallowed_methods std::fs::read
    }

    pub fn path_ufcs(p: &Path) {
        let _ = <Path>::is_symlink(p); // PLANT clippy::disallowed_methods std::path::Path::is_symlink
    }

    pub fn lexical_path(p: &Path) {
        let _ = p.file_name(); // CONTROL
    }

    pub fn injected_trait_read(reader: &mut impl std::io::Read) {
        let _ = reader.read(&mut [0u8; 16]); // CONTROL
    }

    pub fn boot_read(p: &Path) {
        #[allow(clippy::disallowed_methods, reason = "boot: probe control")]
        let _ = std::fs::read(p); // CONTROL
    }
}
