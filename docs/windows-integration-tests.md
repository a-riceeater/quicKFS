# Windows integration test plan

The automated suite (`cargo test -p quickfs-filesystem-winfsp`) drives the
WinFsp callbacks directly against an in-memory server, so it covers the
translation layer without a driver. What it cannot reach is everything that only
exists once a volume is actually mounted: the FSD's caching and locking, the
exact call sequences Explorer and third-party applications produce, and the
behaviour of a real network link. This plan covers that.

## Environment

| Item | Value |
| --- | --- |
| Client | Windows 10 1809 or newer, x64, WinFsp 2.0+ (Core) installed |
| Server | The Linux `quickfs-server-daemon` with a writable export |
| Link | At least one run on a LAN and one over a link with 50 ms or more of added latency |
| Mount | `quickfs mount <server> Q: --username <user>` unless a case says otherwise |

Set up once:

```powershell
quickfs pair <server> --server-name <name> --pairing-id <id>
quickfs mount <server> Q: --server-name <name> --username <user>
```

Record, for every case: pass or fail, the exact error text if any, and whether
the mount survived. A case that leaves the mount unusable is a failure even if
its own assertion passed.

## 1. Basic file lifecycle

| # | Steps | Expected |
| --- | --- | --- |
| 1.1 | `New-Item Q:\hello.txt -Value "hello"`, `Get-Content Q:\hello.txt` | Content matches exactly. |
| 1.2 | `Set-Content Q:\hello.txt "replaced"`, read back | Old content is fully gone, no trailing bytes. |
| 1.3 | `Add-Content Q:\hello.txt "more"`, read back | Appended at the end. |
| 1.4 | `Remove-Item Q:\hello.txt`, `Test-Path Q:\hello.txt` | `False`, and the file is gone on the server. |
| 1.5 | `New-Item Q:\empty.dat -ItemType File`, read, delete | Zero length throughout; no special-case failure. |
| 1.6 | Verify each of the above on the server with a POSIX client | Byte-identical; no stray temporary files. |

## 2. Large files

| # | Steps | Expected |
| --- | --- | --- |
| 2.1 | Copy a 1 GiB file to `Q:` in Explorer | Completes; `Get-FileHash` matches the source. |
| 2.2 | Copy the same file back from `Q:` | Hash matches. |
| 2.3 | Copy a 5 GiB file (crosses 32-bit offsets) | Completes; hash matches. |
| 2.4 | While 2.1 runs, watch memory in Task Manager | Client memory stays bounded; no growth proportional to file size. |
| 2.5 | `fsutil file createnew Q:\sparse.bin 1073741824` | Succeeds or reports a clear unsupported status; the mount stays usable. |

## 3. Directories

| # | Steps | Expected |
| --- | --- | --- |
| 3.1 | Create `Q:\a\b\c\d\e` with `New-Item -ItemType Directory -Force` | All levels created. |
| 3.2 | `Get-ChildItem Q:\a -Recurse` | Every level enumerated once, no duplicates or omissions. |
| 3.3 | `Remove-Item Q:\a\b\c` while it holds a file | Fails with "directory is not empty". |
| 3.4 | Empty it, then remove | Succeeds. |
| 3.5 | `Remove-Item Q:\a -Recurse` | Whole tree removed. |
| 3.6 | Create a directory with 10 000 entries, open it in Explorer | Fully listed; enumeration pages without repeating or dropping entries. |
| 3.7 | Sort that view by name, size and date in Explorer | No missing rows; no error dialogs. |

## 4. Names

| # | Steps | Expected |
| --- | --- | --- |
| 4.1 | Create `Q:\café.txt`, `Q:\日本語.txt`, `Q:\emoji-🎉.txt` | All created, listed and reopenable by the same spelling. |
| 4.2 | Create a 255-character name | Succeeds. A 256-character name is refused. |
| 4.3 | `New-Item "Q:\bad:name.txt"` | Refused; the mount stays usable. |
| 4.4 | `New-Item "Q:\CON"` (through `\\?\Q:\CON`) | Refused as a reserved name. |
| 4.5 | `New-Item "Q:\trailing."` (through `\\?\`) | Refused. |
| 4.6 | On the server, create `bad:name.txt`, `CON` and `trailing.`; list `Q:` | All three appear, can be read, renamed and deleted from Windows. |
| 4.7 | On the server, create a file whose name is not valid UTF-8; list `Q:` | It is omitted from the listing; the rest of the directory is unaffected. |
| 4.8 | `Get-Content Q:\CAFÉ.TXT` after creating `café.txt` | Same file; the listing still shows the original spelling. |
| 4.9 | On the server, create `Readme.txt` and `README.TXT` in one directory; open either from Windows | Refused rather than resolved arbitrarily; the directory still lists both. |

## 5. Rename and move

| # | Steps | Expected |
| --- | --- | --- |
| 5.1 | `Rename-Item Q:\a.txt b.txt` | Renamed; content intact. |
| 5.2 | `Move-Item Q:\b.txt Q:\dir\` | Moved; content intact. |
| 5.3 | `Move-Item Q:\b.txt Q:\dir\` where the destination exists, without `-Force` | Refused. |
| 5.4 | Same with `-Force` | Replaces the destination. |
| 5.5 | Rename a file that is open for reading in another window | Succeeds; the open handle keeps reading the same data. |
| 5.6 | Rename a directory holding a subtree | Whole subtree moves; paths beneath it resolve. |
| 5.7 | Rename `x.txt` to `X.TXT` | Spelling changes on the server. |

## 6. Handles and sharing

| # | Steps | Expected |
| --- | --- | --- |
| 6.1 | Open the same file in Notepad and in `Get-Content` at once | Both read it. |
| 6.2 | Open a file in Word, leave it open for ten minutes, save | Save succeeds; no handle timeout. |
| 6.3 | Delete a file that another process holds open | Windows defers the delete to the last close; the entry disappears then. |
| 6.4 | `[System.IO.File]::Open("Q:\x", 'Open', 'Read', 'None')`, then open again from another window | The second open is refused with a sharing violation. |
| 6.5 | Kill a process mid-write (`Stop-Process -Force`) | The mount stays usable; no handle is left open on the server (check the server's session handle count). |
| 6.6 | Run 6.5 fifty times in a loop | Server handle count returns to its baseline every time. |

## 7. Concurrency and stress

| # | Steps | Expected |
| --- | --- | --- |
| 7.1 | Eight PowerShell jobs each writing and reading a distinct 100 MiB file | All hashes match; no errors. |
| 7.2 | Eight jobs reading the *same* file concurrently | All reads correct. |
| 7.3 | One job appending while another reads the same file | No torn reads; no client crash. |
| 7.4 | Create, rename and delete 5 000 small files in a loop for ten minutes | No leak in client memory or server handles; no errors. |
| 7.5 | Run [`winfsp-tests`](https://github.com/winfsp/winfsp) against the mount, excluding the cases for features documented as unsupported (streams, reparse points, EAs, `SetSecurity`) | No failures outside the excluded set. |
| 7.6 | `robocopy /MIR` a 10 000-file tree onto `Q:` and back off | Both directions complete; trees compare equal. |

## 8. Network failure and reconnect

| # | Steps | Expected |
| --- | --- | --- |
| 8.1 | Start a large read; block the server port with a firewall rule mid-transfer | The read fails with a device-not-connected style error, not "file not found". |
| 8.2 | Remove the rule; retry the read | Succeeds without remounting. |
| 8.3 | Start a large write; drop the link mid-transfer | The write fails; the destination on the server is either the old file or a partial file, never a silently truncated "successful" copy. |
| 8.4 | Repeat 8.3 and inspect the client's exit status and message | An ambiguous mutation is reported as an I/O error, never as success. |
| 8.5 | Stop the server daemon; browse the mount | Reads the cache can satisfy still work; others report the disconnect. Explorer offers to retry. |
| 8.6 | Restart the daemon; browse again | Recovers without remounting. |
| 8.7 | Suspend and resume the client machine with the mount idle | Recovers on first access. |

## 9. Mount lifecycle

| # | Steps | Expected |
| --- | --- | --- |
| 9.1 | Mount and unmount 25 times in a loop | Every cycle succeeds; the drive letter is released each time. |
| 9.2 | `quickfs mount ... Q:` twice | The second is refused immediately, before any password prompt. |
| 9.3 | `quickfs unmount Q:` from another window | The mount exits cleanly; the letter disappears. |
| 9.4 | Control+C in the mount window | Same. |
| 9.5 | `quickfs unmount Q:` when nothing is mounted | Reports that no mount is active, exit status non-zero. |
| 9.6 | Mount on `*` | Takes the highest free letter. |
| 9.7 | Mount on an existing empty directory | The volume appears there. |
| 9.8 | Kill the mount process with `Stop-Process -Force` | The letter is released by WinFsp; a fresh mount on it succeeds. |

## 10. Explorer and shell

| # | Steps | Expected |
| --- | --- | --- |
| 10.1 | Open `Q:` in Explorer; check the label, type and free space | Label matches `--volume-name`; free space matches the server's `df`. |
| 10.2 | Right-click a file, Properties | Size, "size on disk", and timestamps are plausible. |
| 10.3 | Tick and untick "Read-only" in Properties | The server's mode gains and loses the owner write bit; other bits are untouched. |
| 10.4 | Copy a folder from `C:` to `Q:` with the progress dialog | Completes; no error dialogs; the copy compares equal. |
| 10.5 | Drag a file within `Q:` | Moves rather than copying. |
| 10.6 | Enable "Show hidden files"; check dotfiles | `.gitignore` and similar are hidden by default and appear when hidden files are shown. |
| 10.7 | Search within `Q:` from the Explorer search box | Returns results; does not hang the shell. |
| 10.8 | View a folder of images in Large Icons | Thumbnails render; browsing stays responsive. |

## 11. Applications

| # | Application | Steps | Expected |
| --- | --- | --- | --- |
| 11.1 | Notepad | Open, edit, save a text file on `Q:` | Saves; content correct on the server. |
| 11.2 | Word or Excel | Open, edit, save; check the lock and temporary files it creates | Saves; its temporary files are created and cleaned up. |
| 11.3 | Visual Studio Code | Open a folder on `Q:`, edit and save | Works; the file watcher does not spin the CPU. |
| 11.4 | Git for Windows | `git clone` into `Q:`, then `git status` | Clean tree; case-insensitive lookup does not confuse it. |
| 11.5 | 7-Zip | Extract a large archive onto `Q:` | Completes; contents match. |
| 11.6 | Windows Media Player or VLC | Play a large video from `Q:`, seek several times | Plays without stalling; seeks are prompt. |
| 11.7 | Windows Defender | Let a scheduled scan run over `Q:` | Completes; the mount stays usable. |

## 12. Parity with the macOS client

Run each of these on both clients against the same server and export, and
compare the outcomes:

| # | Case |
| --- | --- |
| 12.1 | Create, write, read back and delete a file. |
| 12.2 | Rename over an existing file, with and without replacement. |
| 12.3 | Remove a non-empty directory. |
| 12.4 | Truncate a file shorter, then longer; check the extension reads back as zeroes. |
| 12.5 | Look a file up by a differently-cased spelling. |
| 12.6 | Read a file while another client writes it. |
| 12.7 | Behaviour when the server export is read-only. |
| 12.8 | Behaviour when the link drops mid-write. |

Differences are expected only where the operating systems differ, and each one
should already be listed under "Windows-specific limitations" in
[the Windows client guide](windows.md). A difference that is *not* listed there
is a bug in either the client or the documentation.
