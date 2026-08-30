# Windows client

The Windows client mounts a quicKFS export as an ordinary Windows drive using
[WinFsp](https://winfsp.dev/). It speaks the same protocol as the macOS client
against the same Linux server, and shares the transport, authentication, trust,
reconnect, caching and filesystem-adapter code with it — the only Windows-specific
part is the translation between WinFsp's callbacks and that shared adapter.

```text
Explorer / applications
        |
     WinFsp (kernel driver + user-mode DLL)
        |
quickfs-filesystem-winfsp     paths, NTSTATUS, handles, security descriptors
        |
quickfs-fs-adapter            shared with macFUSE: inodes, caches, write buffer
        |
quickfs-client-core           transport, authentication, trust, reconnect, cache
        |
     quicKFS server
```

## Requirements

| Requirement | Needed for | Notes |
| --- | --- | --- |
| Windows 10 1809 or newer, x64 or arm64 | running | The client is built for the MSVC targets. |
| [WinFsp](https://winfsp.dev/rel/) 2.0 or newer | mounting | Installs the kernel driver and `winfsp-x64.dll`. Requires administrator rights to install; running a mount afterwards does not. |
| WinFsp "Developer" feature, or an unpacked SDK | building | Supplies `inc/` and `lib/`; see [Building](#building). |
| Rust 1.88 or newer with the MSVC toolchain | building | Same minimum as the rest of the workspace. |
| Visual Studio Build Tools (MSVC linker) | building | Required by the `x86_64-pc-windows-msvc` target. |

WinFsp is installed once per machine. Choose **Core** for running the client and
add **Developer** if the same machine also builds it.

`quickfs.exe` delay-loads `winfsp-x64.dll`, so it starts and prints an actionable
message on a machine where WinFsp is missing, instead of failing to launch with a
loader error. `quickfs pair`, `quickfs forget` and `quickfs trust` work without
WinFsp installed; only `quickfs mount` requires it.

## Licensing

quicKFS is Apache-2.0. WinFsp itself is separately licensed software the user
installs — GPLv3, with a commercial licence available from its author — and this
client links against its DLL at run time rather than bundling it, exactly as the
macOS client relies on a separately installed macFUSE.

`vendor/winfsp_wrs_sys` is a vendored copy of the MIT-licensed `winfsp_wrs_sys`
crate, with one change to its build script (see below); the safe wrapper
`winfsp_wrs` is used unmodified from crates.io and is also MIT. No GPL-licensed
source is compiled into quicKFS.

Anyone redistributing built binaries should confirm this arrangement against
WinFsp's current licence terms rather than relying on this paragraph.

## Building

```powershell
cargo build --release -p quickfs-windows-cli
```

The binary is `target\release\quickfs.exe`.

The build links against WinFsp's import library. It is found in this order:

1. `WINFSP_LIB_DIR` — the directory that directly contains `winfsp-x64.lib`.
2. `WINFSP_INSTALL_DIR` — a WinFsp SDK root; `lib\` beneath it is used.
3. `HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir` — a machine-wide installation
   that included the Developer feature.

The environment variables exist so an unprivileged developer machine or a CI
runner can build against an SDK unpacked without a machine-wide install:

```powershell
msiexec /a winfsp-2.0.23075.msi /qn TARGETDIR=C:\winfsp-sdk
$env:WINFSP_INSTALL_DIR = "C:\winfsp-sdk\DYNAMIC"
cargo build --release -p quickfs-windows-cli
```

A binary built this way still needs a real WinFsp installation to mount
anything: the import library only satisfies the linker, and the driver comes
from the installer.

## Establishing trust

Trust setup is identical to macOS and uses the same on-disk trust database, so a
pin established by either client is understood by the other.

```powershell
# The administrator issues a one-time pairing ID and code on the server.
quickfs pair 192.0.2.10:4433 --server-name files.example.net --pairing-id 3f2b...

# Or import a fingerprint distributed centrally, with no interactive pairing.
quickfs trust import 192.0.2.10:4433 --server-name files.example.net --sha256 A1:B2:...
quickfs trust import 192.0.2.10:4433 --server-name files.example.net --certificate server.pem

quickfs trust list
quickfs forget 192.0.2.10:4433 --server-name files.example.net
```

Enterprise PKI works the same way and needs no pin at all:

```powershell
quickfs mount 192.0.2.10:4433 Q: --username alice --ca-cert C:\pki\enterprise-root.pem
quickfs mount 192.0.2.10:4433 Q: --username alice --trust-system-roots
```

The client state directory (`--state-dir`, default `.quickfs-client`) holds the
trust database and, by default, the offline cache. On Windows it is created with
an owner-only protected DACL and validated on every read; a directory another
account could reach is refused rather than used. A pinned identity is never
silently replaced — re-pairing a server that already has a different fingerprint
requires an explicit `quickfs forget` first.

## Mounting

```powershell
quickfs mount 192.0.2.10:4433 Q: --server-name files.example.net --username alice
```

The mountpoint may be:

| Form | Meaning |
| --- | --- |
| `Q:`, `Q`, `Q:\` | That drive letter. |
| `*` | The next free drive letter, counting down from `Z:`. |
| `C:\mnt\quickfs` | An existing, empty directory on an NTFS volume. |

The command stays in the foreground for the life of the mount. The volume is
visible only to the logon session that created it, which is why an elevated
prompt does not see a mount made from an ordinary one.

Useful options (`quickfs mount --help` lists them all):

| Option | Default | Purpose |
| --- | --- | --- |
| `--volume-name` | `quicKFS` | Volume label shown in Explorer, at most 32 characters. |
| `--cache-dir` | `--state-dir` | Where the persistent offline read cache lives. |
| `--cache-max-bytes` | 20 GiB | Cached payload ceiling. |
| `--read-ahead-max-bytes` | 64 MiB | In-flight speculative read-ahead; `0` disables it. |
| `--transport-timeout-ms` | 60000 | Longest idle wait for one transport phase. |
| `--callback-timeout-ms` | 120000 | End-to-end budget for one filesystem operation. |
| `--reconnect-attempts` | 3 | Attempts before an operation fails. |

## Unmounting

Either:

```powershell
quickfs unmount Q:
```

or press **Control+C** in the window running the mount. Both take the volume
down the same way: open server handles are closed and the transport is shut
down before the process exits.

`quickfs unmount` reaches the mounting process through a named event in the
session-local namespace. It therefore only works from the same logon session
that created the mount, and reports that rather than failing obscurely when no
such mount exists.

Ejecting the drive from Explorer does not unmount a WinFsp volume; use one of
the two routes above.

## How POSIX metadata is presented

POSIX permissions and Windows ACLs are different models, and the client does not
pretend otherwise. What it does is defined and narrow:

| quicKFS state | Windows presentation | Written back? |
| --- | --- | --- |
| directory | `FILE_ATTRIBUTE_DIRECTORY` | n/a |
| owner write bit clear | `FILE_ATTRIBUTE_READONLY` | yes, through the mode |
| leading `.` in the name | `FILE_ATTRIBUTE_HIDDEN` | no |
| owner read/write/execute bits | the mount owner's ACE in the security descriptor | no |
| group and other mode bits | not shown | no |
| uid and gid | not shown | no |
| modification and access times | `LastWriteTime`, `LastAccessTime` | yes |
| creation time | `CreationTime` | no |

Every node is presented as owned by the account that started the mount, with an
ACE granting only that account, and rights derived from the file's *owner* mode
bits. No ACE is created for `Everyone`, and the POSIX group and other bits are
not approximated onto any Windows trustee — doing so would widen access on a
shared machine beyond what the user asked for. This mirrors the macOS client,
where every node reports the mounting user's uid.

Real authorisation still happens on the server, for the authenticated quicKFS
account. The Windows-side descriptor is a second, more restrictive gate and can
never grant more than the server allows.

Clearing the read-only attribute restores the owner's write bit only; setting it
clears all three. Attributes quicKFS cannot store — hidden, system, archive,
temporary — are accepted and discarded rather than refused, because Windows sets
them constantly and failing would break ordinary file copies.

## Windows-specific limitations

These are deliberate and tested, not accidental:

* **Names that Win32 cannot round-trip are refused at creation** with
  `STATUS_OBJECT_NAME_INVALID`: names containing `< > : " | ? *` or a control
  character, names ending in a dot or a space, and the DOS device names (`CON`,
  `NUL`, `COM1`…, with or without an extension). Files already carrying such
  names — created from Linux — stay visible, readable, renamable and deletable
  from Windows. Only creating a new one is refused, so the client never makes a
  file the user cannot afterwards reach through Explorer.
* **Names that are not valid UTF-8 are omitted from directory listings.** The
  Windows namespace is UTF-16 and has no spelling for them. They remain
  accessible from a POSIX client.
* **Lookup is case-insensitive; stored spelling is preserved.** A directory
  holding two entries that differ only by case is ambiguous in the Windows
  namespace, and a lookup that could match either is refused rather than
  resolved arbitrarily.
* **Symbolic links appear as ordinary files.** Reparse points are not
  implemented, so a link's contents cannot be read through the mount. Its entry
  is still listed, renamable and deletable.
* **Extended attributes, alternate data streams, hard links and object IDs are
  not exposed.** macOS resource forks stored as xattrs are not visible here.
* **Security descriptors cannot be set.** `SetSecurity` is not implemented,
  because a descriptor Windows supplied could not be mapped onto a POSIX mode
  without inventing meaning. Tools that copy ACLs (`robocopy /copyall`,
  `icacls`) report that the volume does not support the operation.
* **Byte-range locks are advisory on the server and are not plumbed through the
  WinFsp bridge.** Windows mandatory locking is not emulated.
* **One mount per mountpoint per session.** A second `quickfs mount Q:` is
  refused immediately, before any network work or password prompt.

## Troubleshooting

**`WinFsp is required to use quicKFS on Windows`**
The WinFsp runtime is missing or incomplete. Install it from
<https://winfsp.dev/>. The check looks for both the installation registry key
and the architecture's DLL, so it also catches a failed or partial uninstall.

**`WinFsp refused the mount with NTSTATUS 0xC0000035`**
The mountpoint is already in use — by another quicKFS mount, another WinFsp
filesystem, or a real drive. Pick another letter or use `*`.

**`WinFsp refused the mount with NTSTATUS 0xC0000034`**
WinFsp's user-mode library is present but its kernel driver is not registered.
This happens after an SDK-only extraction, or when the driver service was
removed. Reinstall WinFsp with the Core feature; `sc query WinFsp` should report
a registered service.

**`no quicKFS mount is active on Q: in this session`**
The mount belongs to a different logon session — commonly, it was started from
an elevated prompt and `unmount` was run from an ordinary one, or the reverse.
Stop it from the session that created it, or press Control+C in its window.

**`private client state must be reachable only by the client account`**
The state directory or the trust database carries permissions another account
could use. This is refused rather than repaired silently, because anything that
can write the trust database can redirect the mount to a server of its
choosing. Move the state directory somewhere only your account can reach, or
delete it and pair again.

**`the requested operation could not be completed because of a device error`
during a copy**
A mutation may or may not have reached the server before the connection failed.
The client reports this rather than a clean failure precisely so an application
does not retry a write that already happened. Check the destination before
retrying.

**Explorer reports "the network name is no longer available"**
The link dropped and reconnect attempts were exhausted. Reads that the offline
cache can satisfy keep working; writes do not. The mount recovers on its own
when the server is reachable again.

**A mount is slow to browse**
Check `--cache-block-kib` and `--read-ahead-max-bytes` against the link's
bandwidth-delay product. Directory listings and metadata are cached for one
second on the Windows side and thirty seconds inside the shared adapter, so a
second pass over the same folder should be immediate; if it is not, the
directory is being invalidated by concurrent mutation.

## Testing

`cargo test -p quickfs-filesystem-winfsp` runs the bridge's behavioural suite
against an in-memory server: it drives the same `FileSystemInterface` callbacks
WinFsp calls, in the same order, and needs no driver, no mount and no
administrator rights.

Behaviour that only exists once a volume is mounted — the FSD's own caching,
Explorer's call sequences, third-party applications — is covered by the manual
plan in [Windows integration tests](windows-integration-tests.md).
