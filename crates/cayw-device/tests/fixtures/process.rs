use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::thread;
use std::time::Duration;

fn mark(directory: &Path, name: &str) {
    fs::write(directory.join(name), process::id().to_string()).unwrap();
}

fn sleep_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

fn descendant(directory: &Path, mode: &str, inherit: bool) -> process::Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.arg(mode).arg(directory).stdin(Stdio::null());
    if !inherit {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    command.spawn().unwrap()
}

// Orphan modes intentionally leave descendants for the runner to stop. Waiting
// here would remove the very process-lifecycle behavior these fixtures verify.
#[allow(clippy::zombie_processes)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("inspect") => {
            println!("arg={}", args[2]);
            println!("cwd={}", std::env::current_dir().unwrap().display());
            println!("explicit={}", std::env::var("CAYW_EXPLICIT").unwrap());
            println!(
                "path={}",
                std::env::var("PATH").unwrap_or("<absent>".into())
            );
            println!("stdin={}", io::read_to_string(io::stdin()).unwrap().len());
            eprintln!("fixture stderr");
        }
        Some("interactive-echo") | Some("interactive-budget") => {
            use std::io::BufRead;
            mark(Path::new(&args[2]), "root.pid");
            io::stdout().write_all(b"ready\n").unwrap();
            io::stdout().flush().unwrap();
            for line in io::stdin().lock().split(b'\n') {
                let line = line.unwrap();
                if line == b"quit" {
                    break;
                }
                io::stdout().write_all(&line).unwrap();
                io::stdout().write_all(b"\n").unwrap();
                io::stdout().flush().unwrap();
                if args[1] == "interactive-budget" {
                    io::stderr().write_all(b"e\n").unwrap();
                    io::stderr().flush().unwrap();
                }
            }
            mark(Path::new(&args[2]), "stdin-eof.pid");
        }
        Some("interactive-await-quit") => {
            use std::io::BufRead;
            mark(Path::new(&args[2]), "root.pid");
            io::stdout().write_all(b"ready\n").unwrap();
            io::stdout().flush().unwrap();
            for line in io::stdin().lock().split(b'\n') {
                if line.unwrap() == b"quit" {
                    break;
                }
            }
        }
        Some("interactive-read-prefix") => {
            use std::io::Read;
            let directory = Path::new(&args[2]);
            mark(directory, "root.pid");
            let mut byte = [0];
            io::stdin().read_exact(&mut byte).unwrap();
            mark(directory, "prefix.pid");
            sleep_forever();
        }
        #[cfg(unix)]
        Some("interactive-close-stdin") => {
            // SAFETY: the isolated fixture intentionally closes its own stdin.
            unsafe { libc::close(0) };
            mark(Path::new(&args[2]), "stdin-closed.pid");
            sleep_forever();
        }
        Some("interactive-inspect") => {
            println!("arg={}", args[3]);
            println!("marker={}", fs::read_to_string("marker").unwrap());
            println!("explicit={}", std::env::var("CAYW_EXPLICIT").unwrap());
            println!(
                "path={}",
                std::env::var("PATH").unwrap_or("<absent>".into())
            );
        }
        Some("interactive-gated-output") => {
            let directory = Path::new(&args[2]);
            mark(directory, "root.pid");
            while !directory.join("emit").exists() {
                thread::sleep(Duration::from_millis(5));
            }
            io::stdout().write_all(&[b'x'; 8192]).unwrap();
            io::stdout().flush().unwrap();
            mark(directory, "emitted.pid");
            sleep_forever();
        }
        Some("interactive-stderr") => {
            let directory = Path::new(&args[2]);
            mark(directory, "root.pid");
            io::stdout().write_all(&[b'o'; 8192]).unwrap();
            io::stdout().flush().unwrap();
            mark(directory, "stdout-sent.pid");
            while !directory.join("flood").exists() {
                thread::sleep(Duration::from_millis(5));
            }
            loop {
                io::stderr().write_all(&[b'e'; 8192]).unwrap();
            }
        }
        Some("exit") => process::exit(7),
        Some("both") => {
            let stderr = thread::spawn(|| {
                io::stderr().write_all(&vec![b'e'; 128 * 1024]).unwrap();
            });
            io::stdout().write_all(&vec![b'o'; 128 * 1024]).unwrap();
            stderr.join().unwrap();
        }
        Some("flood") => {
            let stderr = thread::spawn(|| {
                loop {
                    if io::stderr().write_all(&[b'e'; 8192]).is_err() {
                        break;
                    }
                }
            });
            loop {
                if io::stdout().write_all(&[b'o'; 8192]).is_err() {
                    break;
                }
            }
            stderr.join().unwrap();
        }
        #[cfg(target_os = "linux")]
        Some("non-utf8-name") => {
            // Linux comm is an arbitrary byte string, including spaces and ')'.
            let name = b"odd ) (\xff) S 1\0";
            // SAFETY: PR_SET_NAME reads at most 16 bytes from this NUL-terminated
            // name. The unused variadic arguments are supplied as unsigned long.
            assert_eq!(
                unsafe {
                    libc::prctl(
                        libc::PR_SET_NAME,
                        name.as_ptr(),
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                    )
                },
                0
            );
            mark(Path::new(&args[2]), "named.pid");
            sleep_forever();
        }
        Some("sleep") | Some("leaf") => {
            let directory = Path::new(&args[2]);
            mark(
                directory,
                if args[1] == "leaf" {
                    "leaf.pid"
                } else {
                    "root.pid"
                },
            );
            sleep_forever();
        }
        #[cfg(unix)]
        Some("escaped") => {
            // SAFETY: this fixture is a child, not its current process-group leader.
            assert_ne!(unsafe { libc::setsid() }, -1);
            mark(Path::new(&args[2]), "escaped.pid");
            sleep_forever();
        }
        #[cfg(unix)]
        Some("escape") => {
            let directory = Path::new(&args[2]);
            mark(directory, "root.pid");
            drop(descendant(directory, "escaped", true));
            while !directory.join("escaped.pid").exists() {
                thread::sleep(Duration::from_millis(5));
            }
        }
        Some("middle") => {
            let directory = Path::new(&args[2]);
            mark(directory, "middle.pid");
            let mut leaf = descendant(directory, "leaf", true);
            leaf.wait().unwrap();
        }
        Some("tree") | Some("tree-no-pipes") | Some("orphan") | Some("orphan-no-pipes") => {
            let directory = Path::new(&args[2]);
            mark(directory, "root.pid");
            let inherit = !matches!(args[1].as_str(), "orphan-no-pipes" | "tree-no-pipes");
            let mut middle = descendant(directory, "middle", inherit);
            while !directory.join("leaf.pid").exists() {
                thread::sleep(Duration::from_millis(5));
            }
            if matches!(args[1].as_str(), "tree" | "tree-no-pipes") {
                middle.wait().unwrap();
            }
        }
        _ => panic!("unknown fixture mode"),
    }
}
