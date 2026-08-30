// SPDX-License-Identifier: Apache-2.0
//! Behavioural tests for the WinFsp bridge.
//!
//! These drive [`WindowsFilesystem`] through the same `FileSystemInterface`
//! methods WinFsp calls, in the same order and with the same arguments WinFsp
//! uses, against a real in-memory server. No driver, no mount and no
//! administrator rights are needed, so they run in ordinary CI and on a
//! developer machine.
//!
//! What they cannot cover is the part that only exists once a volume is
//! mounted: the FSD's own caching and locking, Explorer's exact call sequences,
//! and the behaviour of third-party applications. Those are covered by the
//! manual plan in `docs/windows-integration-tests.md`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::{
    filesystem::WindowsFilesystem,
    handle::Handle,
    status,
    testing::{MockFilesystem, adapter_for},
};
use quickfs_protocol::NodeId;
use std::sync::Arc;
use widestring::U16CString;
use winfsp_wrs::{
    CleanupFlags, CreateFileInfo, CreateOptions, DirInfo, FileAccessRights, FileAttributes,
    FileInfo, FileSystemInterface, SecurityDescriptor, WriteMode,
};

/// A filesystem plus the server behind it.
struct Harness {
    filesystem: WindowsFilesystem,
    server: Arc<MockFilesystem>,
}

impl Harness {
    fn with(server: Arc<MockFilesystem>) -> Self {
        let adapter = adapter_for(Arc::clone(&server));
        let capabilities = adapter.probe_capabilities().unwrap();
        let filesystem = WindowsFilesystem::new(
            adapter,
            capabilities,
            "quickfs-test",
            "S-1-5-21-0-0-0-1000".into(),
        )
        .unwrap();
        Self { filesystem, server }
    }

    fn empty() -> Self {
        Self::with(MockFilesystem::empty())
    }

    fn tree() -> Self {
        Self::with(MockFilesystem::tree())
    }

    /// A read/write open, the way `CreateFile` with `GENERIC_READ |
    /// GENERIC_WRITE` reaches a file system.
    fn read_write() -> FileAccessRights {
        FileAccessRights::FILE_READ_DATA | FileAccessRights::FILE_WRITE_DATA
    }

    fn create_file(&self, path: &str) -> Handle {
        self.create(path, CreateOptions(0), Self::read_write()).0
    }

    fn create_directory(&self, path: &str) -> Handle {
        self.create(path, CreateOptions::FILE_DIRECTORY_FILE, Self::read_write())
            .0
    }

    fn create(
        &self,
        path: &str,
        options: CreateOptions,
        granted_access: FileAccessRights,
    ) -> (Handle, FileInfo) {
        self.try_create(path, options, granted_access)
            .unwrap_or_else(|status| panic!("create {path} failed with {status:#010x}"))
    }

    fn try_create(
        &self,
        path: &str,
        options: CreateOptions,
        granted_access: FileAccessRights,
    ) -> Result<(Handle, FileInfo), status::NtStatus> {
        self.filesystem.create(
            &wide(path),
            CreateFileInfo {
                create_options: options,
                granted_access,
                file_attributes: FileAttributes(0),
                allocation_size: 0,
            },
            empty_descriptor(),
        )
    }

    fn open(&self, path: &str, granted_access: FileAccessRights) -> Handle {
        self.try_open(path, CreateOptions(0), granted_access)
            .unwrap_or_else(|status| panic!("open {path} failed with {status:#010x}"))
            .0
    }

    fn try_open(
        &self,
        path: &str,
        options: CreateOptions,
        granted_access: FileAccessRights,
    ) -> Result<(Handle, FileInfo), status::NtStatus> {
        self.filesystem.open(&wide(path), options, granted_access)
    }

    /// Write a whole buffer the way a `WriteFile` at an offset does, then close.
    fn write_file(&self, path: &str, data: &[u8]) {
        let handle = self.create_file(path);
        self.filesystem
            .write(handle.clone(), data, WriteMode::Normal { offset: 0 })
            .unwrap();
        self.close(handle);
    }

    /// Read a whole file through repeated `read` callbacks, as the cache
    /// manager does.
    fn read_file(&self, path: &str, chunk: usize) -> Vec<u8> {
        let handle = self.open(path, FileAccessRights::FILE_READ_DATA);
        let mut result = Vec::new();
        let mut offset = 0u64;
        let mut buffer = vec![0u8; chunk];
        loop {
            match self.filesystem.read(handle.clone(), &mut buffer, offset) {
                Ok(0) => break,
                Ok(read) => {
                    result.extend_from_slice(&buffer[..read]);
                    offset += read as u64;
                }
                Err(status) if status == status::STATUS_END_OF_FILE => break,
                Err(status) => panic!("read failed with {status:#010x}"),
            }
        }
        self.close(handle);
        result
    }

    /// Close a handle the way WinFsp does: cleanup, then close.
    fn close(&self, handle: Handle) {
        self.filesystem
            .cleanup(handle.clone(), None, CleanupFlags(0));
        self.filesystem.close(handle);
    }

    /// Close a handle that has been marked for deletion.
    fn close_deleting(&self, handle: Handle) {
        self.filesystem
            .cleanup(handle.clone(), None, CleanupFlags::DELETE);
        self.filesystem.close(handle);
    }

    /// Every name a directory enumeration yields, paging through the buffer the
    /// way WinFsp does when a listing does not fit in one call.
    fn list(&self, path: &str, page: usize) -> Vec<String> {
        let handle = self.open(path, FileAccessRights::FILE_LIST_DIRECTORY);
        let mut names: Vec<String> = Vec::new();
        let mut marker: Option<U16CString> = None;
        loop {
            let before = names.len();
            let mut room = page;
            self.filesystem
                .read_directory(handle.clone(), marker.as_deref(), |info: DirInfo| {
                    if room == 0 {
                        return false;
                    }
                    room -= 1;
                    names.push(dir_info_name(&info));
                    true
                })
                .unwrap();
            if names.len() == before {
                break;
            }
            marker = names.last().map(|name| U16CString::from_str(name).unwrap());
        }
        self.close(handle);
        names
    }
}

fn wide(value: &str) -> U16CString {
    U16CString::from_str(value).unwrap()
}

/// A descriptor for the `SecurityDescriptor` argument of `create`. The bridge
/// ignores it — quicKFS derives descriptors from POSIX modes — so any valid one
/// will do.
fn empty_descriptor() -> SecurityDescriptor {
    SecurityDescriptor::from_wstr(&wide("D:P")).unwrap()
}

/// Recover the file name from a `DirInfo`, which stores the length in its
/// `size` field rather than terminating the name.
fn dir_info_name(info: &DirInfo) -> String {
    let header = std::mem::size_of::<winfsp_wrs_sys::FSP_FSCTL_DIR_INFO>();
    let units = (usize::from(info.size).saturating_sub(header)) / std::mem::size_of::<u16>();
    String::from_utf16_lossy(&info.file_name[..units.min(info.file_name.len())])
}

/// Wait for a condition the bridge satisfies asynchronously.
///
/// Closing a handle that only ever read is deliberately not on the caller's
/// critical path — the adapter closes the remote descriptor in the background
/// rather than charging one round trip to every metadata peek — so a test that
/// asserts the descriptor is gone has to allow for that.
fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("{what} did not happen within the deadline");
}

fn node(server: &MockFilesystem, components: &[&str]) -> NodeId {
    server
        .lookup_path(components)
        .unwrap_or_else(|| panic!("{components:?} should exist on the server"))
}

#[test]
fn a_created_file_reaches_the_server_and_reads_back_byte_for_byte() {
    let harness = Harness::empty();
    harness.write_file("\\hello.txt", b"hello quickfs");

    assert_eq!(
        harness
            .server
            .contents(node(&harness.server, &["hello.txt"])),
        Some(b"hello quickfs".to_vec()),
        "the bytes must be on the server, not only in a client buffer"
    );
    assert_eq!(harness.read_file("\\hello.txt", 4_096), b"hello quickfs");
}

#[test]
fn an_empty_file_is_created_read_and_deleted_without_special_casing() {
    let harness = Harness::empty();
    harness.write_file("\\empty.dat", b"");

    assert_eq!(
        harness
            .server
            .contents(node(&harness.server, &["empty.dat"])),
        Some(Vec::new())
    );
    assert_eq!(harness.read_file("\\empty.dat", 4_096), Vec::<u8>::new());

    let handle = harness.open("\\empty.dat", Harness::read_write());
    harness.close_deleting(handle);
    assert!(harness.server.lookup_path(&["empty.dat"]).is_none());
}

#[test]
fn a_file_larger_than_one_transfer_is_written_and_read_in_pieces() {
    // Four megabytes crosses the mock's one-megabyte per-request limit several
    // times, so the adapter has to split both directions and reassemble them.
    let harness = Harness::empty();
    let payload: Vec<u8> = (0..4 * 1024 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();

    let handle = harness.create_file("\\large.bin");
    let mut offset = 0u64;
    for chunk in payload.chunks(64 * 1024) {
        let (written, _) = harness
            .filesystem
            .write(handle.clone(), chunk, WriteMode::Normal { offset })
            .unwrap();
        assert_eq!(written, chunk.len());
        offset += chunk.len() as u64;
    }
    harness.close(handle);

    assert_eq!(
        harness
            .server
            .contents(node(&harness.server, &["large.bin"])),
        Some(payload.clone())
    );
    assert_eq!(harness.read_file("\\large.bin", 128 * 1024), payload);
}

#[test]
fn nested_directories_are_created_enumerated_and_removed_in_order() {
    let harness = Harness::empty();
    for path in ["\\a", "\\a\\b", "\\a\\b\\c"] {
        let handle = harness.create_directory(path);
        harness.close(handle);
    }
    harness.write_file("\\a\\b\\c\\leaf.txt", b"leaf");

    assert_eq!(
        harness.read_file("\\a\\b\\c\\leaf.txt", 4_096),
        b"leaf",
        "a file three levels down must be reachable by path"
    );
    assert_eq!(harness.list("\\a\\b\\c", 64), vec![".", "..", "leaf.txt"]);

    // A non-empty directory cannot be removed, and Windows expects to be told
    // so when the delete flag is set rather than at cleanup.
    let handle = harness.open("\\a\\b\\c", Harness::read_write());
    assert_eq!(
        harness
            .filesystem
            .set_delete(handle.clone(), &wide("\\a\\b\\c"), true),
        Err(status::STATUS_DIRECTORY_NOT_EMPTY)
    );
    harness.close(handle);

    // Emptied, it can.
    let leaf = harness.open("\\a\\b\\c\\leaf.txt", Harness::read_write());
    harness.close_deleting(leaf);
    let handle = harness.open("\\a\\b\\c", Harness::read_write());
    harness
        .filesystem
        .set_delete(handle.clone(), &wide("\\a\\b\\c"), true)
        .unwrap();
    harness.close_deleting(handle);
    assert!(harness.server.lookup_path(&["a", "b", "c"]).is_none());
    assert!(harness.server.lookup_path(&["a", "b"]).is_some());
}

#[test]
fn unicode_names_survive_creation_enumeration_and_reopening() {
    let harness = Harness::empty();
    // A combining-accent Latin name, a CJK name, and a name outside the basic
    // multilingual plane, which costs two UTF-16 units per character.
    let names = ["café.txt", "日本語のファイル.md", "emoji-🎉-name.bin"];
    for name in names {
        harness.write_file(&format!("\\{name}"), name.as_bytes());
    }

    let listed = harness.list("\\", 64);
    for name in names {
        assert!(
            listed.contains(&name.to_string()),
            "{name} should enumerate"
        );
        assert_eq!(
            harness.read_file(&format!("\\{name}"), 4_096),
            name.as_bytes(),
            "{name} should reopen by the same spelling"
        );
    }
}

#[test]
fn lookup_is_case_insensitive_while_the_stored_spelling_is_preserved() {
    let harness = Harness::empty();
    harness.write_file("\\ReadMe.TXT", b"contents");

    for spelling in ["\\readme.txt", "\\README.TXT", "\\ReadMe.TXT"] {
        assert_eq!(
            harness.read_file(spelling, 4_096),
            b"contents",
            "{spelling} should reach the same file"
        );
    }
    assert!(
        harness.list("\\", 64).contains(&"ReadMe.TXT".to_string()),
        "enumeration must report the spelling the server stores"
    );
}

#[test]
fn a_rename_moves_the_entry_and_leaves_the_open_handle_usable() {
    let harness = Harness::empty();
    let handle = harness.create_directory("\\destination");
    harness.close(handle);
    harness.write_file("\\source.txt", b"payload");

    let handle = harness.open("\\source.txt", Harness::read_write());
    harness
        .filesystem
        .rename(
            handle.clone(),
            &wide("\\source.txt"),
            &wide("\\destination\\moved.txt"),
            false,
        )
        .unwrap();

    // The handle names an inode, not a path, so it still reads after the move.
    let mut buffer = [0u8; 16];
    let read = harness
        .filesystem
        .read(handle.clone(), &mut buffer, 0)
        .unwrap();
    assert_eq!(&buffer[..read], b"payload");
    harness.close(handle);

    assert!(harness.server.lookup_path(&["source.txt"]).is_none());
    assert_eq!(
        harness
            .server
            .contents(node(&harness.server, &["destination", "moved.txt"])),
        Some(b"payload".to_vec())
    );
}

#[test]
fn renaming_over_an_existing_file_obeys_the_replace_flag() {
    let harness = Harness::empty();
    harness.write_file("\\from.txt", b"new");
    harness.write_file("\\onto.txt", b"old");

    let handle = harness.open("\\from.txt", Harness::read_write());
    assert_eq!(
        harness.filesystem.rename(
            handle.clone(),
            &wide("\\from.txt"),
            &wide("\\onto.txt"),
            false,
        ),
        Err(status::STATUS_OBJECT_NAME_COLLISION),
        "without the replace flag an existing destination must be refused"
    );
    harness
        .filesystem
        .rename(
            handle.clone(),
            &wide("\\from.txt"),
            &wide("\\onto.txt"),
            true,
        )
        .unwrap();
    harness.close(handle);

    assert!(harness.server.lookup_path(&["from.txt"]).is_none());
    assert_eq!(
        harness
            .server
            .contents(node(&harness.server, &["onto.txt"])),
        Some(b"new".to_vec())
    );
}

#[test]
fn a_delete_after_a_rename_removes_the_new_name() {
    // The delete happens at cleanup, by which time the handle's idea of where
    // the file lives must have followed the rename.
    let harness = Harness::empty();
    harness.write_file("\\original.txt", b"x");

    let handle = harness.open("\\original.txt", Harness::read_write());
    harness
        .filesystem
        .rename(
            handle.clone(),
            &wide("\\original.txt"),
            &wide("\\renamed.txt"),
            false,
        )
        .unwrap();
    harness.close_deleting(handle);

    assert!(harness.server.lookup_path(&["renamed.txt"]).is_none());
    assert!(harness.server.lookup_path(&["original.txt"]).is_none());
}

#[test]
fn every_handle_this_bridge_opens_is_closed_again() {
    // A leaked handle is a leaked descriptor on the server, and an interrupted
    // operation is the case most likely to leak one.
    let harness = Harness::tree();
    assert_eq!(harness.server.open_handles(), 0);

    let handle = harness.open("\\docs\\notes.txt", Harness::read_write());
    let mut buffer = [0u8; 4];
    harness
        .filesystem
        .read(handle.clone(), &mut buffer, 0)
        .unwrap();
    assert_eq!(
        harness.server.open_handles(),
        1,
        "the read opened one handle"
    );
    harness.close(handle);
    eventually("the read handle closed", || {
        harness.server.open_handles() == 0
    });

    // A write that fails part way through still closes.
    let handle = harness.create_file("\\fresh.bin");
    let _ = harness
        .filesystem
        .write(handle.clone(), b"partial", WriteMode::Normal { offset: 0 });
    harness.close(handle);
    eventually(
        "a handle released even when its operations did not all succeed",
        || harness.server.open_handles() == 0,
    );
}

#[test]
fn opening_a_file_for_metadata_alone_costs_no_server_handle() {
    // Explorer opens every file in a folder purely to read its attributes.
    // Doing that eagerly would be one round trip per file.
    let harness = Harness::tree();
    let before = harness.server.traffic();

    let handle = harness.open("\\docs\\notes.txt", FileAccessRights::FILE_READ_ATTRIBUTES);
    harness.filesystem.get_file_info(handle.clone()).unwrap();
    let during = harness.server.traffic();
    assert_eq!(
        during.opens, before.opens,
        "an attribute-only open must not open the remote file"
    );
    harness.close(handle);
}

#[test]
fn a_file_created_after_a_directory_was_listed_is_immediately_visible() {
    // Explorer lists a folder and then opens what the user creates in it. The
    // listing the enumeration cached predates the creation, so a lookup that
    // trusted it would report the brand-new file as missing.
    let harness = Harness::empty();
    harness.write_file("\\existing.txt", b"x");
    assert_eq!(harness.list("\\", 64), vec!["existing.txt"]);

    harness.write_file("\\created-after.txt", b"new");
    assert_eq!(
        harness.read_file("\\created-after.txt", 4_096),
        b"new",
        "a file created after the listing must be reachable at once"
    );
    let listed = harness.list("\\", 64);
    assert!(listed.contains(&"created-after.txt".to_string()));
    assert!(listed.contains(&"existing.txt".to_string()));
}

#[test]
fn a_file_deleted_after_a_directory_was_listed_is_immediately_gone() {
    let harness = Harness::empty();
    harness.write_file("\\doomed.txt", b"x");
    harness.write_file("\\survivor.txt", b"y");
    assert_eq!(harness.list("\\", 64).len(), 2);

    let handle = harness.open("\\doomed.txt", Harness::read_write());
    harness.close_deleting(handle);

    assert_eq!(
        harness
            .try_open(
                "\\doomed.txt",
                CreateOptions(0),
                FileAccessRights::FILE_READ_DATA
            )
            .err(),
        Some(status::STATUS_OBJECT_NAME_NOT_FOUND),
        "a deleted file must not survive in a cached listing"
    );
    assert_eq!(harness.list("\\", 64), vec!["survivor.txt"]);
}

#[test]
fn a_directory_listing_is_paged_without_repeating_or_dropping_entries() {
    // WinFsp calls back repeatedly with the last name as a marker whenever the
    // buffer fills. Losing an entry hides a file; repeating one makes Explorer
    // show duplicates.
    let harness = Harness::empty();
    let expected: Vec<String> = (0..37).map(|index| format!("file{index:03}.txt")).collect();
    for name in &expected {
        harness.write_file(&format!("\\{name}"), name.as_bytes());
    }

    let all_at_once = harness.list("\\", 1_000);
    let one_at_a_time = harness.list("\\", 1);
    let in_fives = harness.list("\\", 5);

    assert_eq!(all_at_once, one_at_a_time);
    assert_eq!(all_at_once, in_fives);
    let mut names = all_at_once.clone();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), all_at_once.len(), "no entry may repeat");
    for name in &expected {
        assert!(all_at_once.contains(name), "{name} should be listed");
    }
}

#[test]
fn the_volume_root_lists_no_dot_entries_and_a_subdirectory_lists_both() {
    let harness = Harness::tree();
    let root = harness.list("\\", 64);
    assert!(!root.contains(&".".to_string()));
    assert!(!root.contains(&"..".to_string()));

    let docs = harness.list("\\docs", 64);
    assert_eq!(docs[0], ".");
    assert_eq!(docs[1], "..");
}

#[test]
fn concurrent_readers_and_writers_on_one_export_do_not_interfere() {
    let harness = Arc::new(Harness::empty());
    for index in 0..8 {
        harness.write_file(&format!("\\shared{index}.bin"), &[index as u8; 4_096]);
    }

    std::thread::scope(|scope| {
        for index in 0..8u8 {
            let reader = Arc::clone(&harness);
            scope.spawn(move || {
                let path = format!("\\shared{index}.bin");
                for _ in 0..10 {
                    assert_eq!(reader.read_file(&path, 1_024), vec![index; 4_096]);
                }
            });
            let writer = Arc::clone(&harness);
            scope.spawn(move || {
                let path = format!("\\written{index}.bin");
                writer.write_file(&path, &[index; 2_048]);
                assert_eq!(writer.read_file(&path, 4_096), vec![index; 2_048]);
            });
        }
    });

    eventually("every handle closed", || harness.server.open_handles() == 0);
}

#[test]
fn the_same_file_may_be_open_several_times_at_once() {
    // Windows applications routinely hold a file open in several places; each
    // open gets its own context and its own remote handle, and closing one must
    // not disturb another.
    let harness = Harness::empty();
    harness.write_file("\\shared.txt", b"shared contents");

    let first = harness.open("\\shared.txt", FileAccessRights::FILE_READ_DATA);
    let second = harness.open("\\shared.txt", FileAccessRights::FILE_READ_DATA);
    let mut buffer = [0u8; 6];
    harness
        .filesystem
        .read(first.clone(), &mut buffer, 0)
        .unwrap();
    harness
        .filesystem
        .read(second.clone(), &mut buffer, 0)
        .unwrap();
    assert_eq!(&buffer, b"shared");

    harness.close(first);
    // The surviving handle still works after its sibling closed.
    let read = harness
        .filesystem
        .read(second.clone(), &mut buffer, 7)
        .unwrap();
    assert_eq!(&buffer[..read], b"conten");
    harness.close(second);
    eventually("both handles closed", || harness.server.open_handles() == 0);
}

#[test]
fn truncating_and_extending_a_file_changes_its_size_on_the_server() {
    let harness = Harness::empty();
    harness.write_file("\\resize.bin", &[7u8; 1_000]);

    let handle = harness.open("\\resize.bin", Harness::read_write());
    let info = harness
        .filesystem
        .set_file_size(handle.clone(), 100, false)
        .unwrap();
    assert_eq!(info.file_size(), 100);

    let info = harness
        .filesystem
        .set_file_size(handle.clone(), 4_000, false)
        .unwrap();
    assert_eq!(info.file_size(), 4_000);
    harness.close(handle);

    let contents = harness
        .server
        .contents(node(&harness.server, &["resize.bin"]))
        .unwrap();
    assert_eq!(contents.len(), 4_000);
    assert_eq!(&contents[..100], &[7u8; 100]);
    assert!(
        contents[100..].iter().all(|byte| *byte == 0),
        "extending a file must read back as zeroes"
    );
}

#[test]
fn an_allocation_size_change_does_not_truncate_a_longer_file() {
    // Windows sets allocation size as a hint constantly. Treating it as a
    // length would silently destroy data.
    let harness = Harness::empty();
    harness.write_file("\\hint.bin", &[3u8; 500]);

    let handle = harness.open("\\hint.bin", Harness::read_write());
    let info = harness
        .filesystem
        .set_file_size(handle.clone(), 8_192, true)
        .unwrap();
    assert_eq!(info.file_size(), 500, "the file's length must not change");
    harness.close(handle);
    assert_eq!(
        harness
            .server
            .contents(node(&harness.server, &["hint.bin"]))
            .map(|data| data.len()),
        Some(500)
    );
}

#[test]
fn overwriting_a_file_empties_it_before_the_new_contents_arrive() {
    let harness = Harness::empty();
    harness.write_file("\\document.txt", b"the previous, much longer contents");

    let handle = harness.open("\\document.txt", Harness::read_write());
    harness
        .filesystem
        .overwrite(handle.clone(), FileAttributes(0), false, 0)
        .unwrap();
    harness
        .filesystem
        .write(handle.clone(), b"short", WriteMode::Normal { offset: 0 })
        .unwrap();
    harness.close(handle);

    assert_eq!(
        harness
            .server
            .contents(node(&harness.server, &["document.txt"])),
        Some(b"short".to_vec()),
        "no trailing bytes from the old contents may survive"
    );
}

#[test]
fn appending_writes_land_at_the_end_of_the_file() {
    let harness = Harness::empty();
    harness.write_file("\\log.txt", b"first\n");

    let handle = harness.open("\\log.txt", Harness::read_write());
    harness
        .filesystem
        .write(handle.clone(), b"second\n", WriteMode::WriteToEOF)
        .unwrap();
    harness
        .filesystem
        .write(handle.clone(), b"third\n", WriteMode::WriteToEOF)
        .unwrap();
    harness.close(handle);

    assert_eq!(
        harness.read_file("\\log.txt", 4_096),
        b"first\nsecond\nthird\n"
    );
}

#[test]
fn a_constrained_write_never_extends_the_file() {
    let harness = Harness::empty();
    harness.write_file("\\fixed.bin", &[1u8; 10]);

    let handle = harness.open("\\fixed.bin", Harness::read_write());
    let (written, info) = harness
        .filesystem
        .write(
            handle.clone(),
            &[2u8; 100],
            WriteMode::ConstrainedIO { offset: 5 },
        )
        .unwrap();
    assert_eq!(written, 5, "only the bytes inside the file may be taken");
    assert_eq!(info.file_size(), 10);

    // Entirely past the end: nothing at all.
    let (written, _) = harness
        .filesystem
        .write(
            handle.clone(),
            &[3u8; 4],
            WriteMode::ConstrainedIO { offset: 10 },
        )
        .unwrap();
    assert_eq!(written, 0);
    harness.close(handle);

    let contents = harness
        .server
        .contents(node(&harness.server, &["fixed.bin"]))
        .unwrap();
    assert_eq!(contents, [1, 1, 1, 1, 1, 2, 2, 2, 2, 2]);
}

#[test]
fn a_flush_pushes_buffered_bytes_to_the_server_before_it_returns() {
    // The adapter coalesces small writes. An application that calls
    // FlushFileBuffers is asking for durability and must not be told the data
    // is safe while it is still in a client buffer.
    let harness = Harness::empty();
    let handle = harness.create_file("\\durable.txt");
    harness
        .filesystem
        .write(
            handle.clone(),
            b"needs flushing",
            WriteMode::Normal { offset: 0 },
        )
        .unwrap();

    harness.filesystem.flush(handle.clone()).unwrap();
    assert_eq!(
        harness
            .server
            .contents(node(&harness.server, &["durable.txt"])),
        Some(b"needs flushing".to_vec()),
        "the bytes must be on the server once flush returns"
    );
    harness.close(handle);
}

#[test]
fn a_volume_flush_carries_no_file_context_and_still_succeeds() {
    // WinFsp passes a null file context to mean "flush the volume". Handling it
    // is what the crate's one `unsafe` module exists for.
    let harness = Harness::empty();
    let volume = Handle::default_absent();
    assert!(harness.filesystem.flush(volume).is_ok());
}

#[test]
fn a_read_only_export_refuses_every_mutation_with_a_write_protected_volume() {
    let harness = Harness::with(MockFilesystem::read_only());
    harness.server.insert_file(
        quickfs_protocol::ROOT_NODE,
        "readable.txt",
        0o444,
        b"contents".to_vec(),
    );

    assert_eq!(
        harness
            .try_create("\\new.txt", CreateOptions(0), Harness::read_write())
            .err(),
        Some(status::STATUS_MEDIA_WRITE_PROTECTED)
    );

    let handle = harness.open("\\readable.txt", FileAccessRights::FILE_READ_DATA);
    assert_eq!(
        harness
            .filesystem
            .write(handle.clone(), b"nope", WriteMode::Normal { offset: 0 })
            .err(),
        Some(status::STATUS_MEDIA_WRITE_PROTECTED)
    );
    assert_eq!(
        harness
            .filesystem
            .set_file_size(handle.clone(), 0, false)
            .err(),
        Some(status::STATUS_MEDIA_WRITE_PROTECTED)
    );
    assert_eq!(
        harness
            .filesystem
            .set_delete(handle.clone(), &wide("\\readable.txt"), true)
            .err(),
        Some(status::STATUS_MEDIA_WRITE_PROTECTED)
    );
    // Reading still works.
    let mut buffer = [0u8; 8];
    assert_eq!(
        harness
            .filesystem
            .read(handle.clone(), &mut buffer, 0)
            .unwrap(),
        8
    );
    harness.close(handle);
}

#[test]
fn names_windows_cannot_round_trip_are_refused_at_creation_but_not_at_lookup() {
    let harness = Harness::empty();
    for name in ["bad:name.txt", "star*.txt", "trailing.", "CON"] {
        assert_eq!(
            harness
                .try_create(
                    &format!("\\{name}"),
                    CreateOptions(0),
                    Harness::read_write()
                )
                .err(),
            Some(status::STATUS_OBJECT_NAME_INVALID),
            "{name} should be refused at creation"
        );
    }

    // The same names, created on the server as a Linux client would, stay
    // reachable so they can be read and deleted from Windows.
    for name in ["bad:name.txt", "star*.txt", "trailing.", "CON"] {
        harness.server.insert_file(
            quickfs_protocol::ROOT_NODE,
            name,
            0o644,
            name.as_bytes().to_vec(),
        );
    }
    for name in ["bad:name.txt", "star*.txt", "trailing.", "CON"] {
        assert_eq!(
            harness.read_file(&format!("\\{name}"), 4_096),
            name.as_bytes(),
            "{name} should still be readable"
        );
    }
}

#[test]
fn a_path_that_tries_to_escape_the_export_is_refused() {
    let harness = Harness::tree();
    for path in ["\\..\\outside", "\\docs\\..\\..\\etc", "\\.\\docs"] {
        let status = harness
            .try_open(path, CreateOptions(0), FileAccessRights::FILE_READ_DATA)
            .err()
            .unwrap_or_else(|| panic!("{path} should not resolve"));
        assert_eq!(status, status::STATUS_OBJECT_PATH_NOT_FOUND, "{path}");
    }
}

#[test]
fn opening_a_directory_as_a_file_and_the_reverse_are_both_refused() {
    let harness = Harness::tree();
    assert_eq!(
        harness
            .try_open(
                "\\docs",
                CreateOptions::FILE_NON_DIRECTORY_FILE,
                FileAccessRights::FILE_READ_DATA
            )
            .err(),
        Some(status::STATUS_FILE_IS_A_DIRECTORY)
    );
    assert_eq!(
        harness
            .try_open(
                "\\docs\\notes.txt",
                CreateOptions::FILE_DIRECTORY_FILE,
                FileAccessRights::FILE_READ_DATA
            )
            .err(),
        Some(status::STATUS_NOT_A_DIRECTORY)
    );
}

#[test]
fn an_absent_file_reports_absence_rather_than_a_generic_failure() {
    let harness = Harness::tree();
    assert_eq!(
        harness
            .try_open(
                "\\nothing.txt",
                CreateOptions(0),
                FileAccessRights::FILE_READ_DATA
            )
            .err(),
        Some(status::STATUS_OBJECT_NAME_NOT_FOUND)
    );
    assert_eq!(
        harness
            .filesystem
            .get_security_by_name(&wide("\\nothing.txt"), || None)
            .err(),
        Some(status::STATUS_OBJECT_NAME_NOT_FOUND),
        "WinFsp uses this probe to decide between create and open"
    );
}

#[test]
fn the_read_only_attribute_round_trips_through_the_posix_mode() {
    let harness = Harness::empty();
    harness.write_file("\\attributes.txt", b"x");

    let handle = harness.open("\\attributes.txt", Harness::read_write());
    let info = harness
        .filesystem
        .set_basic_info(handle.clone(), FileAttributes::READONLY, 0, 0, 0, 0)
        .unwrap();
    assert!(info.file_attributes().is(FileAttributes::READONLY));

    let (attributes, _, _) = harness
        .filesystem
        .get_security_by_name(&wide("\\attributes.txt"), || None)
        .unwrap();
    assert!(
        attributes.is(FileAttributes::READONLY),
        "the attribute must be visible to a fresh lookup, not only to this handle"
    );

    let info = harness
        .filesystem
        .set_basic_info(handle.clone(), FileAttributes::NORMAL, 0, 0, 0, 0)
        .unwrap();
    assert!(!info.file_attributes().is(FileAttributes::READONLY));
    harness.close(handle);
}

#[test]
fn a_dot_prefixed_name_is_presented_as_hidden() {
    let harness = Harness::empty();
    harness.write_file("\\.gitignore", b"target/");
    harness.write_file("\\visible.txt", b"x");

    let (hidden, _, _) = harness
        .filesystem
        .get_security_by_name(&wide("\\.gitignore"), || None)
        .unwrap();
    let (visible, _, _) = harness
        .filesystem
        .get_security_by_name(&wide("\\visible.txt"), || None)
        .unwrap();
    assert!(hidden.is(FileAttributes::HIDDEN));
    assert!(!visible.is(FileAttributes::HIDDEN));
}

#[test]
fn one_entry_lookup_is_answered_from_the_parents_cached_listing() {
    // Explorer asks for entries one at a time far more often than it
    // enumerates. Each of those must not become a round trip.
    let harness = Harness::tree();
    let directory = harness.open("\\docs", FileAccessRights::FILE_LIST_DIRECTORY);
    harness
        .filesystem
        .read_directory(directory.clone(), None, |_| true)
        .unwrap();

    let before = harness.server.traffic();
    for _ in 0..20 {
        harness
            .filesystem
            .get_dir_info_by_name(directory.clone(), &wide("notes.txt"))
            .unwrap();
    }
    let after = harness.server.traffic();
    assert_eq!(
        after.directory_listings, before.directory_listings,
        "repeated single-entry lookups must not re-list the directory"
    );
    harness.close(directory);
}

#[test]
fn free_space_is_reported_from_the_servers_own_accounting() {
    let harness = Harness::empty();
    let info = harness.filesystem.get_volume_info().unwrap();
    // The mock reports 1 Mi blocks of 4 KiB with half available.
    assert_eq!(info.total_size(), 1024 * 1024 * 4_096);
    assert_eq!(info.free_size(), 512 * 1024 * 4_096);
    assert_eq!(info.volume_label().to_string().unwrap(), "quickfs-test");
}

#[test]
fn repeated_mount_teardown_returns_every_reference_and_handle() {
    // The bridge is torn down and rebuilt against one server, as a repeated
    // mount and unmount does.
    let server = MockFilesystem::tree();
    for _ in 0..5 {
        let harness = Harness::with(Arc::clone(&server));
        let handle = harness.open("\\docs\\notes.txt", FileAccessRights::FILE_READ_DATA);
        let mut buffer = [0u8; 5];
        harness
            .filesystem
            .read(handle.clone(), &mut buffer, 0)
            .unwrap();
        harness.close(handle);
        harness.filesystem.dispatcher_stopped(true);
        assert_eq!(
            harness.filesystem.resolver().retained_directories(),
            0,
            "stopping the dispatcher must return every retained reference"
        );
    }
    eventually("every handle closed", || server.open_handles() == 0);
}
