#!/usr/bin/env python3
"""Exercise a mounted filesystem with a Premiere-like POSIX media workload."""

from __future__ import annotations

import argparse
import array
import concurrent.futures
import ctypes
import errno
import fcntl
import mmap
import os
import plistlib
import random
import select
import shutil
import signal
import socket
import stat
import sys
import tempfile
import termios
from collections.abc import Callable
from pathlib import Path


MIB = 1024 * 1024
WORKSPACE_PREFIX = ".quickfs-media-smoke-"
MEDIA_SIZE = 24 * MIB + 12_347
LARGE_IO_SIZE = 9 * MIB + 32_771
PATTERN_TILE = bytes(((index * 73 + 41) ^ (index >> 8)) & 0xFF for index in range(64 * 1024))


class SmokeFailure(RuntimeError):
    """A workload invariant was not satisfied."""


class DarwinAttrList(ctypes.Structure):
    _fields_ = [
        ("bitmapcount", ctypes.c_ushort),
        ("reserved", ctypes.c_ushort),
        ("commonattr", ctypes.c_uint32),
        ("volattr", ctypes.c_uint32),
        ("dirattr", ctypes.c_uint32),
        ("fileattr", ctypes.c_uint32),
        ("forkattr", ctypes.c_uint32),
    ]


class DarwinTimespec(ctypes.Structure):
    _pack_ = 4
    _fields_ = [("seconds", ctypes.c_long), ("nanoseconds", ctypes.c_long)]


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SmokeFailure(message)


def pattern_at(offset: int, length: int) -> bytes:
    """Return deterministic bytes based only on their absolute file offsets."""
    require(offset >= 0 and length >= 0, "pattern range must be nonnegative")
    if length == 0:
        return b""
    start = offset % len(PATTERN_TILE)
    repeats = (start + length + len(PATTERN_TILE) - 1) // len(PATTERN_TILE)
    return (PATTERN_TILE * repeats)[start : start + length]


def pwrite_all(file_descriptor: int, data: bytes, offset: int) -> int:
    """Complete a positioned write, retrying legal short writes."""
    view = memoryview(data)
    written = 0
    while written < len(view):
        amount = os.pwrite(file_descriptor, view[written:], offset + written)
        if amount <= 0:
            raise SmokeFailure(
                f"pwrite made no progress at offset {offset + written}"
            )
        written += amount
    return written


def pread_exact(file_descriptor: int, length: int, offset: int) -> bytes:
    """Read up to length bytes at offset, retrying legal short reads."""
    result = bytearray()
    while len(result) < length:
        block = os.pread(file_descriptor, length - len(result), offset + len(result))
        if not block:
            break
        result.extend(block)
    return bytes(result)


def write_all(file_descriptor: int, data: bytes) -> None:
    view = memoryview(data)
    written = 0
    while written < len(view):
        amount = os.write(file_descriptor, view[written:])
        if amount <= 0:
            raise SmokeFailure("write made no progress")
        written += amount


def sync_file(file_descriptor: int) -> None:
    if hasattr(os, "fdatasync"):
        os.fdatasync(file_descriptor)
    os.fsync(file_descriptor)


def _raise_errno(operation: str) -> None:
    error = ctypes.get_errno()
    raise OSError(error, f"{operation}: {os.strerror(error)}")


def set_xattr(path: Path, name: bytes, value: bytes) -> None:
    if hasattr(os, "setxattr"):
        os.setxattr(path, name, value)
        return
    require(sys.platform == "darwin", "extended-attribute API is unavailable")
    libc = ctypes.CDLL(None, use_errno=True)
    payload = ctypes.create_string_buffer(value) if value else None
    result = libc.setxattr(
        ctypes.c_char_p(os.fsencode(path)),
        ctypes.c_char_p(name),
        ctypes.cast(payload, ctypes.c_void_p) if payload is not None else None,
        ctypes.c_size_t(len(value)),
        ctypes.c_uint32(0),
        ctypes.c_int(0),
    )
    if result != 0:
        _raise_errno("setxattr")


def get_xattr(path: Path, name: bytes) -> bytes:
    if hasattr(os, "getxattr"):
        return os.getxattr(path, name)
    require(sys.platform == "darwin", "extended-attribute API is unavailable")
    libc = ctypes.CDLL(None, use_errno=True)
    path_bytes = os.fsencode(path)
    size = libc.getxattr(
        ctypes.c_char_p(path_bytes),
        ctypes.c_char_p(name),
        None,
        ctypes.c_size_t(0),
        ctypes.c_uint32(0),
        ctypes.c_int(0),
    )
    if size < 0:
        _raise_errno("getxattr(size)")
    if size == 0:
        return b""
    value = ctypes.create_string_buffer(size)
    received = libc.getxattr(
        ctypes.c_char_p(path_bytes),
        ctypes.c_char_p(name),
        ctypes.cast(value, ctypes.c_void_p),
        ctypes.c_size_t(size),
        ctypes.c_uint32(0),
        ctypes.c_int(0),
    )
    if received < 0:
        _raise_errno("getxattr")
    return value.raw[:received]


def list_xattrs(path: Path) -> set[bytes]:
    if hasattr(os, "listxattr"):
        return {os.fsencode(name) for name in os.listxattr(path)}
    require(sys.platform == "darwin", "extended-attribute API is unavailable")
    libc = ctypes.CDLL(None, use_errno=True)
    path_bytes = os.fsencode(path)
    size = libc.listxattr(ctypes.c_char_p(path_bytes), None, ctypes.c_size_t(0), ctypes.c_int(0))
    if size < 0:
        _raise_errno("listxattr(size)")
    if size == 0:
        return set()
    value = ctypes.create_string_buffer(size)
    received = libc.listxattr(
        ctypes.c_char_p(path_bytes),
        ctypes.cast(value, ctypes.c_void_p),
        ctypes.c_size_t(size),
        ctypes.c_int(0),
    )
    if received < 0:
        _raise_errno("listxattr")
    return {name for name in value.raw[:received].split(b"\0") if name}


def remove_xattr(path: Path, name: bytes) -> None:
    if hasattr(os, "removexattr"):
        os.removexattr(path, name)
        return
    require(sys.platform == "darwin", "extended-attribute API is unavailable")
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.removexattr(
        ctypes.c_char_p(os.fsencode(path)), ctypes.c_char_p(name), ctypes.c_int(0)
    ) != 0:
        _raise_errno("removexattr")


def exchange_data(left: Path, right: Path) -> None:
    require(sys.platform == "darwin", "exchangedata(2) is a macOS-only test")
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.exchangedata(
        ctypes.c_char_p(os.fsencode(left)),
        ctypes.c_char_p(os.fsencode(right)),
        ctypes.c_uint(0),
    ) != 0:
        _raise_errno("exchangedata")


def set_backup_time(path: Path, seconds: int, nanoseconds: int) -> None:
    require(sys.platform == "darwin", "backup-time reporting is a macOS-only test")
    attributes = DarwinAttrList(5, 0, 0x00002000, 0, 0, 0, 0)
    timestamp = DarwinTimespec(seconds, nanoseconds)
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.setattrlist(
        ctypes.c_char_p(os.fsencode(path)),
        ctypes.byref(attributes),
        ctypes.byref(timestamp),
        ctypes.sizeof(timestamp),
        ctypes.c_ulong(0),
    ) != 0:
        _raise_errno("setattrlist(ATTR_CMN_BKUPTIME)")


def get_backup_time(path: Path) -> tuple[int, int]:
    require(sys.platform == "darwin", "backup-time reporting is a macOS-only test")
    attributes = DarwinAttrList(5, 0, 0x00002000, 0, 0, 0, 0)
    result = ctypes.create_string_buffer(4 + ctypes.sizeof(DarwinTimespec))
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.getattrlist(
        ctypes.c_char_p(os.fsencode(path)),
        ctypes.byref(attributes),
        ctypes.byref(result),
        ctypes.sizeof(result),
        ctypes.c_ulong(0),
    ) != 0:
        _raise_errno("getattrlist(ATTR_CMN_BKUPTIME)")
    timestamp = DarwinTimespec.from_buffer_copy(result.raw[4:])
    return timestamp.seconds, timestamp.nanoseconds


def set_volume_name(mountpoint: Path, name: str) -> None:
    require(sys.platform == "darwin", "volume renaming is a macOS-only test")
    encoded = name.encode("utf-8") + b"\0"
    # ATTR_VOL_NAME is variable width. Darwin expects an attrreference_t,
    # followed by the name and one spare byte for its strict bounds check.
    payload = ctypes.create_string_buffer(8 + len(encoded) + 1)
    ctypes.c_int32.from_buffer(payload).value = 8
    ctypes.c_uint32.from_buffer(payload, 4).value = len(encoded)
    payload[8 : 8 + len(encoded)] = encoded
    attributes = DarwinAttrList(5, 0, 0, 0x80002000, 0, 0, 0)
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.setattrlist(
        ctypes.c_char_p(os.fsencode(mountpoint)),
        ctypes.byref(attributes),
        ctypes.byref(payload),
        ctypes.sizeof(payload),
        ctypes.c_ulong(0),
    ) != 0:
        _raise_errno("setattrlist(ATTR_VOL_NAME)")


class Progress:
    def __init__(self) -> None:
        self.current = "startup"

    def run(self, label: str, action: Callable[[], None]) -> None:
        self.current = label
        print(f"[....] {label}", flush=True)
        action()
        print(f"[ ok ] {label}", flush=True)


class Workload:
    def __init__(self, workspace: Path) -> None:
        self.workspace = workspace

    def statvfs(self) -> None:
        information = os.statvfs(self.workspace)
        require(information.f_bsize > 0, "statvfs returned a zero block size")
        require(information.f_frsize > 0, "statvfs returned a zero fragment size")
        require(information.f_namemax >= 64, "statvfs returned an implausible name limit")
        require(information.f_blocks >= 0, "statvfs returned a negative block count")
        require(information.f_bfree >= 0, "statvfs returned a negative free-block count")

    def positioned_concurrent_io(self) -> None:
        media_path = self.workspace / "timeline-media.bin"
        descriptor = os.open(media_path, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        expected = bytearray(MEDIA_SIZE)
        try:
            os.ftruncate(descriptor, MEDIA_SIZE)

            large_offset = 137 * 1024 + 17
            large_data = pattern_at(large_offset, LARGE_IO_SIZE)
            require(
                len(large_data) > 8 * MIB,
                "large positioned I/O request did not exceed 8 MiB",
            )
            require(
                pwrite_all(descriptor, large_data, large_offset) == len(large_data),
                "large pwrite was incomplete",
            )
            expected[large_offset : large_offset + len(large_data)] = large_data

            operations: list[tuple[int, int]] = [
                (4 * MIB + 12_289, 4 * MIB + 137),
                (5 * MIB + 4_093, 2 * MIB + 33),
                (6 * MIB + 777, MIB + 513),
                (5 * MIB + 32_001, 3 * MIB + 91),
                (18 * MIB + 19, 2 * MIB + 4_099),
                (19 * MIB + 2_047, MIB + 8_191),
            ]
            randomizer = random.Random(0x51A7E)
            for _ in range(72):
                length = randomizer.randint(4 * 1024, 768 * 1024)
                offset = randomizer.randint(0, MEDIA_SIZE - length)
                operations.append((offset, length))
            randomizer.shuffle(operations)
            require(
                [offset for offset, _ in operations]
                != sorted(offset for offset, _ in operations),
                "positioned writes were not dispatched out of offset order",
            )

            for offset, length in operations:
                expected[offset : offset + length] = pattern_at(offset, length)

            def perform_write(operation: tuple[int, int]) -> None:
                offset, length = operation
                worker_descriptor = os.open(media_path, os.O_RDWR)
                try:
                    data = pattern_at(offset, length)
                    require(
                        pwrite_all(worker_descriptor, data, offset) == length,
                        f"concurrent pwrite was incomplete at offset {offset}",
                    )
                finally:
                    os.close(worker_descriptor)

            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
                futures = [executor.submit(perform_write, operation) for operation in operations]
                for future in concurrent.futures.as_completed(futures):
                    future.result()

            sync_file(descriptor)
            file_status = os.fstat(descriptor)
            require(file_status.st_size == MEDIA_SIZE, "concurrent writes changed file size")

            large_read = pread_exact(descriptor, LARGE_IO_SIZE, large_offset)
            require(
                large_read == expected[large_offset : large_offset + LARGE_IO_SIZE],
                "single >8 MiB pread returned incorrect data",
            )

            read_operations = list(operations)
            for _ in range(48):
                length = randomizer.randint(1, 640 * 1024)
                offset = randomizer.randint(0, MEDIA_SIZE - length)
                read_operations.append((offset, length))
            randomizer.shuffle(read_operations)

            def perform_read(operation: tuple[int, int]) -> None:
                offset, length = operation
                worker_descriptor = os.open(media_path, os.O_RDONLY)
                try:
                    actual = pread_exact(worker_descriptor, length, offset)
                finally:
                    os.close(worker_descriptor)
                require(
                    actual == expected[offset : offset + length],
                    f"concurrent pread mismatch at offset {offset}, length {length}",
                )

            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
                futures = [executor.submit(perform_read, operation) for operation in read_operations]
                for future in concurrent.futures.as_completed(futures):
                    future.result()

            os.lseek(descriptor, 0, os.SEEK_SET)
            position = 0
            loop_iterations = 0
            while position < MEDIA_SIZE:
                requested = min(MIB + 131_071 + (loop_iterations % 5) * 65_537, MEDIA_SIZE - position)
                block = os.read(descriptor, requested)
                require(block, f"sequential read stopped early at offset {position}")
                require(
                    block == expected[position : position + len(block)],
                    f"sequential read-loop mismatch at offset {position}",
                )
                position += len(block)
                loop_iterations += 1
            require(position > 8 * MIB, "sequential read loop did not span 8 MiB")
            require(os.read(descriptor, 1) == b"", "read past end of file did not return EOF")
        finally:
            os.close(descriptor)

    def sparse_and_truncate(self) -> None:
        sparse_path = self.workspace / "sparse-render-cache.bin"
        descriptor = os.open(sparse_path, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        try:
            distant_offset = 32 * MIB + 12_345
            marker = pattern_at(distant_offset, 64 * 1024 + 17)
            pwrite_all(descriptor, marker, distant_offset)
            require(
                os.fstat(descriptor).st_size == distant_offset + len(marker),
                "sparse pwrite produced the wrong file size",
            )
            require(
                pread_exact(descriptor, 128 * 1024, 8 * MIB + 99) == bytes(128 * 1024),
                "sparse hole did not read as zeroes",
            )
            require(
                pread_exact(descriptor, len(marker), distant_offset) == marker,
                "sparse tail data was not readable",
            )
            if hasattr(os, "SEEK_DATA") and hasattr(os, "SEEK_HOLE"):
                first_hole = os.lseek(descriptor, 0, os.SEEK_HOLE)
                first_data = os.lseek(descriptor, 0, os.SEEK_DATA)
                logical_size = distant_offset + len(marker)
                require(
                    0 <= first_hole <= logical_size,
                    "SEEK_HOLE returned an offset outside the file",
                )
                require(
                    0 <= first_data <= distant_offset,
                    "SEEK_DATA returned an offset after the sparse tail extent",
                )
                # POSIX permits a filesystem to expose a sparse file as one
                # dense data extent. macFUSE's kernel backend can materialize
                # the zero-filled gap before the request reaches QuickFS, in
                # which case DATA=0 and HOLE=EOF are the correct answers.

            shortened_size = 2 * MIB + 37
            os.ftruncate(descriptor, shortened_size)
            require(os.fstat(descriptor).st_size == shortened_size, "truncate-down failed")
            require(
                pread_exact(descriptor, 1, shortened_size) == b"",
                "truncate-down did not establish EOF",
            )

            extended_size = 18 * MIB + 117
            os.ftruncate(descriptor, extended_size)
            require(os.fstat(descriptor).st_size == extended_size, "truncate-up failed")
            require(
                pread_exact(descriptor, 128 * 1024, 9 * MIB + 7) == bytes(128 * 1024),
                "truncate-up range did not read as zeroes",
            )
            end_marker_offset = extended_size - 4_099
            end_marker = pattern_at(end_marker_offset, 4_099)
            pwrite_all(descriptor, end_marker, end_marker_offset)
            sync_file(descriptor)
            require(
                pread_exact(descriptor, len(end_marker), end_marker_offset) == end_marker,
                "post-truncate pwrite was not retained",
            )
        finally:
            os.close(descriptor)

    def mapped_io(self) -> None:
        mapped_path = self.workspace / "mapped-index.bin"
        mapped_size = 4 * MIB
        descriptor = os.open(mapped_path, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        expected = bytearray(mapped_size)
        try:
            os.ftruncate(descriptor, mapped_size)
            edits = [
                (17, 64 * 1024 + 3),
                (MIB - 29, 192 * 1024 + 71),
                (2 * MIB + 4_093, 384 * 1024 + 19),
                (3 * MIB + 777, 512 * 1024 + 31),
            ]
            with mmap.mmap(descriptor, mapped_size, access=mmap.ACCESS_WRITE) as mapping:
                for offset, length in edits:
                    data = pattern_at(offset, length)
                    mapping[offset : offset + length] = data
                    expected[offset : offset + length] = data
                mapping.flush()
                os.fsync(descriptor)
            sync_file(descriptor)
            require(
                pread_exact(descriptor, mapped_size, 0) == expected,
                "mmap flush/fsync data mismatch",
            )
        finally:
            os.close(descriptor)

    def durable_rename_and_namespace(self) -> None:
        package = self.workspace / "render-package"
        os.mkdir(package, 0o700)
        temporary = package / ".preview.mov.partial"
        final = package / "preview.mov"
        payload = pattern_at(0, 768 * 1024 + 113)

        descriptor = os.open(temporary, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        try:
            write_all(descriptor, payload)
            sync_file(descriptor)
        finally:
            os.close(descriptor)

        os.replace(temporary, final)
        require(not temporary.exists(), "atomic rename left the temporary name present")
        require(final.is_file(), "atomic rename did not create the final name")

        directory_flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
        directory_descriptor = os.open(package, directory_flags)
        try:
            os.fsync(directory_descriptor)
        finally:
            os.close(directory_descriptor)

        link = package / "current-preview"
        os.symlink(final.name, link)
        require(os.readlink(link) == final.name, "readlink returned an unexpected target")
        linked_descriptor = os.open(link, os.O_RDONLY)
        try:
            require(
                pread_exact(linked_descriptor, len(payload), 0) == payload,
                "reading through a symlink returned incorrect data",
            )
        finally:
            os.close(linked_descriptor)
        os.unlink(link)

        scratch = package / "scratch"
        victim = scratch / "discard.me"
        os.mkdir(scratch, 0o700)
        victim_descriptor = os.open(victim, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        try:
            write_all(victim_descriptor, b"temporary")
        finally:
            os.close(victim_descriptor)
        os.unlink(victim)
        os.rmdir(scratch)

        os.unlink(final)
        os.rmdir(package)

    def macos_metadata_links_copy_exchange_and_readiness(self) -> None:
        source = self.workspace / "metadata-source.mov"
        destination = self.workspace / "metadata-destination.mov"
        hardlink = self.workspace / "metadata-source-alias.mov"
        copied = self.workspace / "optimized-copy.mov"
        source_payload = pattern_at(0, 2 * MIB + 333)
        destination_payload = b"destination-content"
        source.write_bytes(source_payload)
        destination.write_bytes(destination_payload)

        custom_name = b"com.quickfs.smoke"
        finder_tag_name = b"com.apple.metadata:_kMDItemUserTags"
        quarantine_name = b"com.apple.quarantine"
        resource_name = b"com.apple.ResourceFork"
        finder_tags = plistlib.dumps(["quicKFS\n6"], fmt=plistlib.FMT_BINARY)
        set_xattr(source, custom_name, b"custom-application-metadata")
        set_xattr(source, finder_tag_name, finder_tags)
        set_xattr(source, quarantine_name, b"0081;00000000;quicKFS;smoke-test")
        set_xattr(source, resource_name, b"source-resource-fork")
        set_xattr(destination, resource_name, b"destination-resource-fork")
        require(
            get_xattr(source, custom_name) == b"custom-application-metadata",
            "custom extended attribute did not round-trip",
        )
        require(get_xattr(source, finder_tag_name) == finder_tags, "Finder tag xattr mismatch")
        require(
            get_xattr(source, quarantine_name).endswith(b"smoke-test"),
            "quarantine xattr mismatch",
        )
        names = list_xattrs(source)
        for expected in (custom_name, finder_tag_name, quarantine_name, resource_name):
            require(expected in names, f"listxattr omitted {expected!r}")

        os.link(source, hardlink)
        source_status = os.stat(source)
        link_status = os.stat(hardlink)
        require(source_status.st_ino == link_status.st_ino, "hardlink changed inode identity")
        require(source_status.st_nlink >= 2, "hardlink count was not reported")
        require(hardlink.read_bytes() == source_payload, "hardlink data did not match")

        shutil.copyfile(source, copied)
        require(copied.read_bytes() == source_payload, "optimized/application copy data mismatch")

        descriptor = os.open(source, os.O_RDWR)
        try:
            available = array.array("i", [0])
            fcntl.ioctl(descriptor, termios.FIONREAD, available, True)
            require(available[0] == len(source_payload), "FIONREAD returned the wrong byte count")
            poller = select.poll()
            requested = select.POLLIN | select.POLLOUT
            poller.register(descriptor, requested)
            events = poller.poll(1_000)
            require(events, "poll returned no readiness for a regular media file")
            require(events[0][1] & requested == requested, "poll omitted read/write readiness")
        finally:
            os.close(descriptor)

        try:
            exchange_data(source, destination)
        except OSError as error:
            if error.errno not in (errno.ENOTSUP, errno.EOPNOTSUPP):
                raise
            # macFUSE dropped the exchangedata vnode capability on macOS 11.
            # QuickFS still tests its protocol/server operation and retains the
            # native callback for older macFUSE kernels that expose it.
            print("[skip] this macFUSE/macOS pair does not expose exchangedata(2)", flush=True)
        else:
            require(source.read_bytes() == destination_payload, "exchangedata left content mismatch")
            require(destination.read_bytes() == source_payload, "exchangedata right content mismatch")
            require(
                get_xattr(source, resource_name) == b"destination-resource-fork",
                "exchangedata did not move the destination resource fork",
            )
            require(
                get_xattr(destination, resource_name) == b"source-resource-fork",
                "exchangedata did not move the source resource fork",
            )
            require(
                get_xattr(source, custom_name) == b"custom-application-metadata",
                "exchangedata incorrectly moved ordinary inode metadata",
            )
        remove_xattr(source, custom_name)
        require(custom_name not in list_xattrs(source), "removexattr left the attribute listed")

        os.unlink(hardlink)
        os.unlink(copied)
        os.unlink(source)
        os.unlink(destination)

    def macos_volume_and_backup_metadata(self) -> None:
        backup_path = self.workspace / "backup-metadata.mov"
        backup_path.write_bytes(b"backup-time")
        expected = (1_700_123_456, 789_000_000)
        set_backup_time(backup_path, *expected)
        require(get_backup_time(backup_path) == expected, "backup time did not round-trip")
        os.unlink(backup_path)

        set_volume_name(self.workspace.parent, "quicKFS-smoke")
        set_volume_name(self.workspace.parent, "quicKFS-live")

    def special_nodes_and_non_utf8_names(self) -> None:
        raw_name = os.fsencode(self.workspace) + b"/raw-\xff-name"
        try:
            descriptor = os.open(raw_name, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        except OSError as error:
            if sys.platform != "darwin" or error.errno not in (errno.EILSEQ, errno.EIO):
                raise
            # APFS itself returns EILSEQ for this name, and modern macFUSE's
            # Darwin normalization layer returns EIO before QuickFS receives
            # the lossless bytes. The v5 protocol and Linux export tests still
            # verify arbitrary Unix byte names end to end.
            print("[skip] macOS does not accept this non-UTF-8 pathname", flush=True)
        else:
            try:
                write_all(descriptor, b"lossless-name")
            finally:
                os.close(descriptor)
            require(
                raw_name.split(b"/")[-1] in os.listdir(os.fsencode(self.workspace)),
                "raw filename was changed",
            )
            os.unlink(raw_name)

        fifo = self.workspace / "render-control.fifo"
        unix_socket = self.workspace / "render-control.sock"
        try:
            os.mkfifo(fifo, 0o600)
        except OSError as error:
            if error.errno in (errno.ENOTSUP, errno.EOPNOTSUPP, errno.ENODEV, errno.EPERM):
                print("[skip] backing server does not advertise special-node creation", flush=True)
                return
            raise
        require(stat.S_ISFIFO(os.lstat(fifo).st_mode), "mkfifo did not create a FIFO")
        endpoint = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            endpoint.bind(str(unix_socket))
            require(stat.S_ISSOCK(os.lstat(unix_socket).st_mode), "bind did not create a socket node")
        finally:
            endpoint.close()
            if unix_socket.exists():
                os.unlink(unix_socket)
            os.unlink(fifo)

    @staticmethod
    def _lock_child(
        lock_path: Path,
        inherited_descriptor: int,
        result_pipe: int,
        release_pipe: int,
        start: int,
        length: int,
    ) -> None:
        try:
            os.close(inherited_descriptor)
            descriptor = os.open(lock_path, os.O_RDWR)
            try:
                try:
                    fcntl.lockf(
                        descriptor,
                        fcntl.LOCK_EX | fcntl.LOCK_NB,
                        length,
                        start,
                        os.SEEK_SET,
                    )
                except OSError as error:
                    if error.errno not in (errno.EACCES, errno.EAGAIN):
                        os.write(result_pipe, b"E")
                        os._exit(2)
                    os.write(result_pipe, b"C")
                else:
                    fcntl.lockf(descriptor, fcntl.LOCK_UN, length, start, os.SEEK_SET)
                    os.write(result_pipe, b"U")
                    os._exit(3)

                if os.read(release_pipe, 1) != b"R":
                    os.write(result_pipe, b"E")
                    os._exit(4)
                try:
                    fcntl.lockf(
                        descriptor,
                        fcntl.LOCK_EX | fcntl.LOCK_NB,
                        length,
                        start,
                        os.SEEK_SET,
                    )
                except OSError:
                    os.write(result_pipe, b"E")
                    os._exit(5)
                os.write(result_pipe, b"A")
                fcntl.lockf(descriptor, fcntl.LOCK_UN, length, start, os.SEEK_SET)
            finally:
                os.close(descriptor)
        except BaseException:
            try:
                os.write(result_pipe, b"E")
            except OSError:
                pass
            os._exit(6)
        os._exit(0)

    @staticmethod
    def _read_pipe_byte(pipe: int, timeout: float, description: str) -> bytes:
        readable, _, _ = select.select([pipe], [], [], timeout)
        require(readable, f"timed out waiting for lock child to {description}")
        value = os.read(pipe, 1)
        require(value, f"lock child exited before it could {description}")
        return value

    def cross_process_range_lock(self) -> None:
        lock_path = self.workspace / "range-lock.bin"
        descriptor = os.open(lock_path, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        start = 4_096
        length = 8_192
        result_read, result_write = os.pipe()
        release_read, release_write = os.pipe()
        child = -1
        parent_locked = False
        try:
            os.ftruncate(descriptor, 32 * 1024)
            fcntl.lockf(
                descriptor,
                fcntl.LOCK_EX | fcntl.LOCK_NB,
                length,
                start,
                os.SEEK_SET,
            )
            parent_locked = True
            child = os.fork()
            if child == 0:
                os.close(result_read)
                os.close(release_write)
                self._lock_child(
                    lock_path,
                    descriptor,
                    result_write,
                    release_read,
                    start,
                    length,
                )

            os.close(result_write)
            result_write = -1
            os.close(release_read)
            release_read = -1
            first = self._read_pipe_byte(result_read, 10.0, "report the lock conflict")
            require(first == b"C", f"competing nonblocking lock returned status {first!r}")

            fcntl.lockf(descriptor, fcntl.LOCK_UN, length, start, os.SEEK_SET)
            parent_locked = False
            os.write(release_write, b"R")
            second = self._read_pipe_byte(result_read, 10.0, "acquire the released lock")
            require(second == b"A", f"released byte-range lock returned status {second!r}")

            waited_child, status = os.waitpid(child, 0)
            require(waited_child == child, "waitpid returned an unexpected child")
            child = -1
            require(os.WIFEXITED(status), "lock child did not exit normally")
            require(os.WEXITSTATUS(status) == 0, "lock child exited unsuccessfully")
        finally:
            if parent_locked:
                try:
                    fcntl.lockf(descriptor, fcntl.LOCK_UN, length, start, os.SEEK_SET)
                except OSError:
                    pass
            for pipe in (result_read, result_write, release_read, release_write):
                if pipe >= 0:
                    try:
                        os.close(pipe)
                    except OSError:
                        pass
            if child > 0:
                try:
                    os.kill(child, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                os.waitpid(child, 0)
            os.close(descriptor)


def safe_cleanup(workspace: Path, mountpoint: Path) -> None:
    absolute_workspace = Path(os.path.abspath(workspace))
    require(
        absolute_workspace.parent == mountpoint,
        "refusing cleanup because workspace is not directly below the mountpoint",
    )
    require(
        absolute_workspace.name.startswith(WORKSPACE_PREFIX),
        "refusing cleanup because workspace name lacks the smoke-test prefix",
    )
    status = os.lstat(absolute_workspace)
    require(stat.S_ISDIR(status.st_mode), "refusing cleanup because workspace is not a directory")
    require(not stat.S_ISLNK(status.st_mode), "refusing cleanup because workspace is a symlink")
    shutil.rmtree(absolute_workspace)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run destructive media-application filesystem checks inside a uniquely "
            "created and removed directory beneath MOUNTPOINT."
        )
    )
    parser.add_argument("mountpoint", help="mounted filesystem directory to exercise")
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    for function_name in ("pread", "pwrite", "fork", "statvfs"):
        require(hasattr(os, function_name), f"Python/OS lacks required os.{function_name}")

    mountpoint = Path(os.path.realpath(os.path.expanduser(arguments.mountpoint)))
    require(mountpoint.is_dir(), f"mountpoint is not a directory: {mountpoint}")
    workspace: Path | None = None
    progress = Progress()
    failure: BaseException | None = None

    try:
        workspace = Path(tempfile.mkdtemp(prefix=WORKSPACE_PREFIX, dir=mountpoint))
        os.chmod(workspace, 0o700)
        print(f"workspace: {workspace}", flush=True)
        workload = Workload(workspace)
        progress.run("statvfs capacity reporting", workload.statvfs)
        progress.run(
            "concurrent/random/overlapping positioned I/O and >8 MiB reads",
            workload.positioned_concurrent_io,
        )
        progress.run("sparse writes and truncate-down/truncate-up", workload.sparse_and_truncate)
        progress.run("mmap flush and file fsync", workload.mapped_io)
        progress.run(
            "synced temp file, atomic rename, directory fsync, and namespace ops",
            workload.durable_rename_and_namespace,
        )
        progress.run(
            "xattrs, Finder metadata, resource forks, hardlinks, copy, exchangedata, ioctl, and poll",
            workload.macos_metadata_links_copy_exchange_and_readiness,
        )
        progress.run(
            "macOS volume rename plus backup-time set/reporting",
            workload.macos_volume_and_backup_metadata,
        )
        progress.run(
            "lossless byte names plus FIFO/socket creation when the server supports them",
            workload.special_nodes_and_non_utf8_names,
        )
        progress.run(
            "cross-process nonblocking byte-range lock conflict and release",
            workload.cross_process_range_lock,
        )
    except BaseException as error:
        failure = error
        print(
            f"[FAIL] {progress.current}: {type(error).__name__}: {error}",
            file=sys.stderr,
            flush=True,
        )
    finally:
        if workspace is not None:
            try:
                safe_cleanup(workspace, mountpoint)
                print(f"[ ok ] cleaned workspace {workspace.name}", flush=True)
            except BaseException as cleanup_error:
                print(
                    f"[FAIL] cleanup: {type(cleanup_error).__name__}: {cleanup_error}",
                    file=sys.stderr,
                    flush=True,
                )
                if failure is None:
                    failure = cleanup_error

    if failure is not None:
        return 1
    print(f"PASS media workload smoke test at {mountpoint}", flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SmokeFailure as error:
        print(f"[FAIL] startup: {error}", file=sys.stderr, flush=True)
        raise SystemExit(1) from None
