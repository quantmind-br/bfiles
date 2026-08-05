# bfiles

Scan a directory tree at maximum speed and list files at or above a size
threshold. Written in Rust on top of `getdents64` and `statx` ([rustix]), with
per-directory work-stealing across all CPU cores ([rayon]).

[rustix]: https://crates.io/crates/rustix
[rayon]: https://crates.io/crates/rayon

## Usage

```console
$ bfiles -s 1gb ~
$ bfiles -s 500mb -o big.csv /data
```

- `-s, --size SIZE` — minimum file size. Supports `b`, `kb`, `mb`, `gb`, `tb`
  suffixes (1024-based) or a raw byte count: `1gb`, `2.5tb`, `500mb`, `1048576`.
- `-o, --output FILE` — also write the results to a CSV file.
  Columns: `path,size_bytes` (path is quoted if it contains special characters).
- `-j, --threads N` — number of scanning threads. Defaults to one per logical
  CPU, which is optimal on local SSD/NVMe. Raise it when the tree lives behind
  high-latency storage (network mounts, spinning disks), where threads spend
  their time waiting rather than computing.
- `PATH` — root directory to scan (default: current directory). A regular file
  may be given and is checked directly.

Results are sorted by size (largest first). The table shows the path and the
human-readable size.

## Behavior

- Hidden files and directories are included.
- Symbolic links are not followed (avoids cycles and double counting); a
  symlinked file is skipped.
- Directories that cannot be read (permission denied) are skipped and counted;
  the count is reported after the table.
- File sizes come from `statx` — only the size is read, never file contents.
- Mount points are crossed, so pseudo-filesystems (`/proc`, `/sys`) and network
  mounts under the root are scanned too.

## Performance notes

The scan is bound by kernel time, not by user-space work: with a warm page
cache it saturates every core issuing `openat`/`getdents64`/`statx`. Each
directory is opened once and read with a per-thread `getdents64` buffer; file
sizes come from a `statx` relative to that directory's descriptor, so the
kernel resolves a single path component instead of re-walking the whole path
per file. A path string is only allocated for directories and for files that
pass the size filter.

## Build and install

```console
$ make            # cargo build --release
$ make install    # installs to ~/.local/bin/bfiles
```

`make install` honors `PREFIX` (defaults to `$(HOME)/.local`), so
`make install PREFIX=/usr` installs to `/usr/bin`. `make uninstall` removes the
binary.
