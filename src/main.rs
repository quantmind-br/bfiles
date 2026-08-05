mod scan;

use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use rayon::slice::ParallelSliceMut;

const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

/// Parse a human size, e.g. "1gb", "500mb", "2.5tb", "1048576" (1024-based units).
fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("size cannot be empty".to_string());
    }
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let value: f64 = num
        .parse()
        .map_err(|_| format!("invalid size `{input}` (expected e.g. 1gb, 500mb, 1048576)"))?;
    if !(0.0..=u64::MAX as f64).contains(&value) {
        return Err(format!("invalid size `{input}`: must be a positive number"));
    }
    let unit = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1u64,
        "k" | "kb" | "kib" => 1u64 << 10,
        "m" | "mb" | "mib" => 1u64 << 20,
        "g" | "gb" | "gib" => 1u64 << 30,
        "t" | "tb" | "tib" => 1u64 << 40,
        other => {
            return Err(format!(
                "unknown size unit `{other}` (use b, kb, mb, gb, tb)"
            ));
        }
    };
    let bytes = value * unit as f64;
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return Err(format!("size `{input}` is too large"));
    }
    Ok(bytes as u64)
}

/// Append a human-readable size to `out`, reusing its buffer.
fn write_human(out: &mut String, bytes: u64) {
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        let _ = write!(out, "{bytes} B");
    } else {
        let _ = write!(out, "{value:.2} {}", UNITS[unit]);
    }
}

fn human_size(bytes: u64) -> String {
    let mut s = String::new();
    write_human(&mut s, bytes);
    s
}

#[derive(Parser)]
#[command(
    name = "bfiles",
    version,
    about = "Scan a directory tree at maximum speed and list files at or above a size threshold"
)]
struct Args {
    /// Minimum file size (1024-based units: b, kb, mb, gb, tb; or raw bytes)
    #[arg(short = 's', long = "size", value_name = "SIZE", value_parser = parse_size)]
    size: u64,

    /// Also write results to a CSV file (columns: path,size_bytes)
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    output: Option<PathBuf>,

    /// Number of scanning threads (default: one per logical CPU)
    #[arg(short = 'j', long = "threads", value_name = "N")]
    threads: Option<usize>,

    /// Root directory to scan (default: current directory)
    #[arg(value_name = "PATH")]
    root: Option<PathBuf>,
}

fn pad(w: &mut impl Write, mut n: usize) -> io::Result<()> {
    const SPACES: [u8; 64] = [b' '; 64];
    while n > 0 {
        let k = n.min(SPACES.len());
        w.write_all(&SPACES[..k])?;
        n -= k;
    }
    Ok(())
}

fn write_csv(path: &Path, results: &[(PathBuf, u64)]) -> io::Result<()> {
    let file = fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    writeln!(w, "path,size_bytes")?;
    for (p, size) in results {
        let s = p.to_string_lossy();
        if s.contains([',', '"', '\n', '\r']) {
            w.write_all(b"\"")?;
            // Escape embedded quotes by doubling them, without a temporary String.
            let mut rest = &*s;
            while let Some(i) = rest.find('"') {
                w.write_all(&rest.as_bytes()[..i])?;
                w.write_all(b"\"\"")?;
                rest = &rest[i + 1..];
            }
            w.write_all(rest.as_bytes())?;
            w.write_all(b"\"")?;
        } else {
            w.write_all(s.as_bytes())?;
        }
        writeln!(w, ",{size}")?;
    }
    w.flush()
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
}

fn print_table(
    results: &[(PathBuf, u64)],
    min_size: u64,
    elapsed: f64,
    errors: usize,
) -> io::Result<()> {
    let stdout = io::stdout();
    let is_tty = stdout.is_terminal();
    let mut w = BufWriter::with_capacity(1 << 20, stdout.lock());

    let total: u64 = results.iter().map(|(_, s)| s).sum();
    writeln!(
        w,
        "Scanned: {} file(s) >= {} ({} total) in {:.2}s",
        results.len(),
        human_size(min_size),
        human_size(total),
        elapsed
    )?;
    if results.is_empty() {
        writeln!(w, "Nothing found.")?;
        return w.flush();
    }

    // Measure column widths without materializing a string per row.
    let mut buf = String::with_capacity(16);
    let mut size_w = 4usize;
    let mut max_path = 0usize;
    for (p, size) in results {
        buf.clear();
        write_human(&mut buf, *size);
        size_w = size_w.max(buf.len());
        max_path = max_path.max(p.to_string_lossy().chars().count());
    }
    let path_w = if is_tty {
        terminal_width()
            .saturating_sub(size_w + 3)
            .min(max_path)
            .max(4)
    } else {
        max_path.max(4)
    };

    writeln!(w)?;
    writeln!(w, "{:<path_w$}  {:>size_w$}", "PATH", "SIZE")?;
    for (p, size) in results {
        let s = p.to_string_lossy();
        let len = s.chars().count();
        if len > path_w {
            // Keep the tail, which carries the file name.
            let skip = len - (path_w - 1);
            let off = s.char_indices().nth(skip).map_or(s.len(), |(i, _)| i);
            w.write_all("…".as_bytes())?;
            w.write_all(s[off..].as_bytes())?;
        } else {
            w.write_all(s.as_bytes())?;
            pad(&mut w, path_w - len)?;
        }
        buf.clear();
        write_human(&mut buf, *size);
        pad(&mut w, 2 + size_w - buf.len())?;
        w.write_all(buf.as_bytes())?;
        w.write_all(b"\n")?;
    }
    if errors > 0 {
        writeln!(
            w,
            "\nWarning: {errors} entrie(s) could not be read (permission denied or removed during the scan)."
        )?;
    }
    w.flush()
}

fn main() -> ExitCode {
    let args = Args::parse();
    if let Some(j) = args.threads
        && rayon::ThreadPoolBuilder::new()
            .num_threads(j)
            .build_global()
            .is_err()
    {
        eprintln!("bfiles: cannot start {j} threads");
        return ExitCode::FAILURE;
    }
    let root = args.root.unwrap_or_else(|| PathBuf::from("."));
    let canonical = match fs::canonicalize(&root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bfiles: cannot access `{}`: {e}", root.display());
            return ExitCode::FAILURE;
        }
    };
    let meta = match fs::metadata(&canonical) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "bfiles: cannot read metadata of `{}`: {e}",
                canonical.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let start = Instant::now();
    let min_size = args.size;
    let mut results: Vec<(PathBuf, u64)> = Vec::new();
    let mut errors = 0usize;

    if meta.is_file() {
        if meta.len() >= min_size {
            results.push((canonical, meta.len()));
        }
    } else if meta.is_dir() {
        (results, errors) = scan::scan(canonical, min_size);
    } else {
        eprintln!(
            "bfiles: `{}` is not a regular file or directory",
            canonical.display()
        );
        return ExitCode::FAILURE;
    }
    let elapsed = start.elapsed().as_secs_f64();

    results.par_sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    if let Some(out) = &args.output {
        if let Err(e) = write_csv(out, &results) {
            eprintln!("bfiles: cannot write `{}`: {e}", out.display());
            return ExitCode::FAILURE;
        }
        println!("CSV written to {}", out.display());
    }

    if let Err(e) = print_table(&results, min_size, elapsed, errors)
        && e.kind() != io::ErrorKind::BrokenPipe
    {
        eprintln!("bfiles: cannot write output: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
