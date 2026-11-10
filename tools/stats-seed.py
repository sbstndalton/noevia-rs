#!/usr/bin/env python3
"""Statistics replay fixture: retain the shared publication-lock inode in both copies.

This is test setup/validation only. Never remove a runtime lock or normalize
product configuration. See server-projects/src/lock.rs for the inode contract.
"""
import argparse
import os
from pathlib import Path
import stat

LOCK = "embedding-config.json.lock"
CONFIG = "embedding-config.json"


def check_lock(root):
    fd = os.open(root / LOCK, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_size != 0
                or stat.S_IMODE(info.st_mode) != 0o600 or info.st_nlink != 1
                or info.st_uid != os.getuid()):
            raise ValueError("embedding publication lock must be an empty private regular file")
    finally:
        os.close(fd)


def initialize(seed):
    # Exclusive creation: fixture setup must happen once, before either copy.
    # Do not truncate, chmod, unlink or otherwise repair an existing artifact.
    fd = os.open(seed / LOCK, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        os.fchmod(fd, 0o600)
    finally:
        os.close(fd)
    check_lock(seed)


def config_bytes(root):
    try:
        fd = os.open(root / CONFIG, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise ValueError("embedding configuration must remain a regular file")
    with os.fdopen(fd, "rb") as file:
        return file.read()


def verify(seed, work):
    check_lock(seed)
    check_lock(work)
    if config_bytes(seed) != config_bytes(work):
        raise ValueError("replay changed embedding configuration presence or bytes")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("initialize", "verify"))
    parser.add_argument("seed", type=Path)
    parser.add_argument("work", type=Path, nargs="?")
    args = parser.parse_args()
    if args.operation == "initialize":
        if args.work is not None:
            parser.error("initialize takes only the common seed directory")
        initialize(args.seed)
    else:
        if args.work is None:
            parser.error("verify needs the seed and completed work directory")
        verify(args.seed, args.work)


if __name__ == "__main__":
    main()
