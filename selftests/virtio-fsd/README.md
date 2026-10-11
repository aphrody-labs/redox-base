# virtio-fsd-selftest

Guest tests for `/scheme/virtio-fs.<tag>`.

The driver serves each virtio-fs share (PCI function) at `/scheme/virtio-fs.<share_tag>`, where `<share_tag>` is the share's tag as given to the hypervisor (e.g. QEMU's `tag=` option). The first argument is that tag. Tests then run against `/scheme/virtio-fs.<share_tag>`. The driver also serves debug nodes (e.g. `lru_evicted`) from a separate `/scheme/virtio-fs-debug.<share_tag>`, which the eviction test reads. With no tag, the program prints help and exits. `scripts/redoxer-run.sh` passes `virtiofs`, matching its `tag=` device option.

| Group | Covers |
| --- | --- |
| functional | open/create, mkdir, unlink, rename, link, symlink, I/O, locks, readdir, setattr, fstatvfs, `O_STAT` / `fpath` |
| edge | errno paths, handle kinds, `.` / `..` unlink |
| concurrency | overlapping large reads/writes, concurrent appends, read/write races, DMA-pool storms |
| perf | sequential and concurrent I/O, concurrency gain |

Missing `fixtures/` is a SKIP not a FAIL.

## Host

Run this on the host against the virtio-fs share to generate fixtures:

```bash
./scripts/gen-fixtures.sh /path/to/host/share
```

Structure of fixtures:

```text
fixtures
├── cp
│   ├── src_600.txt
│   └── src.txt
├── deletable
│   ├── a
│   │   └── foo
│   ├── b
│   │   └── bar
│   ├── once_dir
│   ├── once_file.txt
│   └── once_symlink -> ../links/target.txt
├── empty
├── files
│   └── small.txt
├── known.txt
├── links
│   ├── chain_a -> chain_b
│   ├── chain_b -> target.txt
│   ├── dangling -> no_such_name
│   ├── loop_a -> loop_b
│   ├── loop_b -> loop_a
│   ├── target.txt
│   ├── to_dir -> ../nested
│   ├── to_file -> target.txt
│   ├── to_file_dot -> ./target.txt
│   ├── to_nested -> ../files/small.txt
│   └── with spaces -> target.txt
├── many
│   ├── file_000
│  ...
│   └── file_199
└── nested
    └── a
        └── b
            └── c
                └── leaf.txt

15 directories, 219 files
```

## Guest

```text
virtio-fsd-selftest
virtio-fsd-selftest virtiofs
virtio-fsd-selftest virtiofs --list
virtio-fsd-selftest virtiofs --skip-perf
virtio-fsd-selftest virtiofs --perf-only --filter perf_concurrency_gain --threads 8 iterations 128
virtio-fsd-selftest virtiofs --filter io_golden
```

### Exit

- `0` ok (skips allowed)
- `1` failures
- `2` test root is missing

## Live system (`make qemu`)

On a booted Redox with the `virtio-fsd-selftest` package installed (add `virtio-fsd-selftest = {}` to the `[packages]` of the config you build):

1. Generate the fixtures in the host directory you share. This only touches `<dir>/fixtures` (it is deleted and recreated):

```bash
./scripts/gen-fixtures.sh "$HOME/shared"
```

2. Start Redox with that directory shared. The tag is the part before `=`, and a bare directory gets the tag `host`:

```bash
make qemu share="$HOME/shared"                         # tag "host"
make qemu share="one=$HOME/shared two=$HOME/shared1"   # tags "one" and "two"
```

3. In the guest, pass the tag:

```text
virtio-fsd-selftest host <FLAGS>
```

Without `fixtures/` the fixture-based tests are a SKIP, not a FAIL.

## redoxer

```bash
make test-virtio-fs
```

Starts host virtiofsd, generates fixtures, overlays the installed sysroot, and
runs `virtio-fsd-selftest virtiofs --skip-perf`. Overriding flags is possible with e.g. `SELFTEST_ARGS='--filter io_golden'`. The share tag stays `virtiofs`.

It tries using `virtiofsd` from `VIRTIOFSD`, `PATH`, or `/usr/libexec`. Otherwise downloads the static x86_64 build from [virtio-fs/virtiofsd](https://gitlab.com/virtio-fs/virtiofsd) (CI artifact on `main`) into `~/.cache/virtiofsd-redoxer`. `VIRTIOFSD_REFRESH=1` re-fetches.
