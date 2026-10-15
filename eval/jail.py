#!/usr/bin/env python3
"""Run a command in a Landlock jail: the shell arm's sandbox. Linux only, no privileges needed.

    python3 eval/jail.py --read DATA --write WORK --exec DUCKDB -- sh -c 'duckdb -c "..."'

A model with a shell on your machine is a model that can delete your files. So the command may
read and execute the system's programs and libraries, execute the one extra binary it is given,
read beneath each --read path, do anything beneath each --write path, and nothing else: no other
file, and no TCP connection or listening port (Landlock ABI 4). UDP and Unix sockets are not
Landlock's to refuse at ABI 4. Another process's /proc entries — the harness's environment, with
its API key — are out of reach too: they need ptrace access, which a Landlock domain has only
over itself. And a command cannot start more than a few hundred threads or write a file over
256 MiB. The jail is applied to this process and inherited by everything it starts, then this
process becomes the command.

Refuses to run where Landlock cannot enforce all of that, rather than running unconfined.
"""
import ctypes
import os
import resource
import struct
import sys

SYS_CREATE, SYS_ADD, SYS_RESTRICT = 444, 445, 446
RULE_PATH_BENEATH, RULE_NET_PORT = 1, 2
PR_SET_NO_NEW_PRIVS = 38

# Filesystem rights by ABI: V1 has 13, V2 adds REFER, V3 TRUNCATE. V4 adds the network.
EXECUTE, WRITE_FILE, READ_FILE, READ_DIR = 1 << 0, 1 << 1, 1 << 2, 1 << 3
FS_V3 = (1 << 15) - 1
READ = EXECUTE | READ_FILE | READ_DIR
FILE_RIGHTS = EXECUTE | WRITE_FILE | READ_FILE | (1 << 14)  # a file rule takes file rights only
NET_BIND_TCP, NET_CONNECT_TCP = 1 << 0, 1 << 1

# DuckDB will not start without reading /proc and /sys (measured). Landlock still keeps the
# jailed process from reading another process's /proc entries, such as the environment of the
# harness that holds the API key: those need ptrace access, which a domain has only over itself.
SYSTEM = ["/usr", "/bin", "/lib", "/lib64", "/etc", "/proc", "/sys"]


def die(msg):
    sys.stderr.write(f"jail: {msg}\n")
    sys.exit(126)


def threads_of(pid):
    try:
        return len(os.listdir(f"/proc/{pid}/task"))
    except OSError:
        return 0


def owned_by_me(pid):
    try:
        return os.stat(f"/proc/{pid}").st_uid == os.getuid()
    except OSError:
        return False


def main(argv):
    if "--" not in argv:
        die(__doc__.strip().splitlines()[2].strip())
    sep = argv.index("--")
    opts, cmd = argv[:sep], argv[sep + 1:]
    rules = [(p, READ) for p in SYSTEM if os.path.exists(p)]
    it = iter(opts)
    for a in it:
        path = os.path.realpath(next(it))
        if a == "--read":
            rules.append((path, READ))
        elif a == "--write":
            rules.append((path, FS_V3))
        elif a == "--exec":
            rules.append((path, EXECUTE | READ_FILE))
        else:
            die(f"unknown option {a}")
    # /dev/null is written to by half the shell idioms there are.
    rules.append(("/dev/null", READ_FILE | WRITE_FILE))

    libc = ctypes.CDLL(None, use_errno=True)
    abi = libc.syscall(ctypes.c_long(SYS_CREATE), None, ctypes.c_size_t(0), ctypes.c_uint32(1))
    if abi < 4:
        die(f"Landlock ABI {abi} here; the jail needs 4 (Linux 6.7+) to also forbid TCP")
    attr = ctypes.create_string_buffer(struct.pack("=QQ", FS_V3, NET_BIND_TCP | NET_CONNECT_TCP))
    fd = libc.syscall(ctypes.c_long(SYS_CREATE), attr, ctypes.c_size_t(16), ctypes.c_uint32(0))
    if fd < 0:
        die(f"could not create a ruleset: {os.strerror(ctypes.get_errno())}")
    for path, rights in rules:
        p = os.open(path, os.O_PATH | os.O_CLOEXEC)
        if not os.path.isdir(path):
            rights &= FILE_RIGHTS
        rule = ctypes.create_string_buffer(struct.pack("=Qi", rights, p))
        if libc.syscall(ctypes.c_long(SYS_ADD), ctypes.c_int(fd), ctypes.c_int(RULE_PATH_BENEATH),
                        rule, ctypes.c_uint32(0)) != 0:
            die(f"could not add {path}: {os.strerror(ctypes.get_errno())}")
        os.close(p)
    # Bounds on what one command can do to the machine besides its time: no fork bomb — the
    # kernel counts every thread this user runs, so the bound starts from what already runs, with
    # room for DuckDB's own threads — and no file that fills the disk.
    mine = sum(threads_of(e) for e in os.listdir("/proc") if e.isdigit() and owned_by_me(e))
    resource.setrlimit(resource.RLIMIT_NPROC, (mine + 256, mine + 256))
    resource.setrlimit(resource.RLIMIT_FSIZE, (256 << 20, 256 << 20))
    if libc.prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0:
        die("could not set no_new_privs")
    if libc.syscall(ctypes.c_long(SYS_RESTRICT), ctypes.c_int(fd), ctypes.c_uint32(0)) != 0:
        die(f"could not restrict: {os.strerror(ctypes.get_errno())}")
    os.close(fd)
    os.execvp(cmd[0], cmd)


if __name__ == "__main__":
    main(sys.argv[1:])
