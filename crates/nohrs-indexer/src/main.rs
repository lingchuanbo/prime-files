//! `prime-files-indexer.exe`.
//!
//! ```text
//! --service --allow <SID>              run as the service (the SCM starts this)
//! --install --allow <SID> [--report F] install or update it, elevated
//! --uninstall [--report F]             remove it, elevated
//! --query                              as a user: ask the running service, print counts
//! --find <text>                        as a user: paths it returns starting with text
//! --dump <C:>                          as an administrator: read a volume here, print counts
//! --tree <path> [--levels <n>]         as an administrator: what a subtree request would answer
//! --pipe-tree <path> [--levels <n>]    as a user: ask the running service for it
//! ```

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    use nohrs_indexer::{client, install, is_sid, protocol, service};

    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        arguments
            .windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
    };
    let allow = value("--allow").filter(|sid| is_sid(sid));
    // The elevated runs have no console the person sees: what went wrong is
    // written where the app that asked can read it.
    let report = |result: std::io::Result<()>| -> ExitCode {
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                service::log(&error.to_string());
                if let Some(path) = value("--report")
                    && let Err(written) = std::fs::write(&path, error.to_string())
                {
                    eprintln!("{error} (and could not write {path}: {written})");
                }
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        }
    };
    match arguments.first().map(String::as_str) {
        Some("--service") => match allow {
            Some(sid) => report(service::run(sid)),
            None => report(Err(std::io::Error::other("--service needs --allow <SID>"))),
        },
        Some("--install") => match allow {
            Some(sid) => report(install::install(&sid)),
            None => report(Err(std::io::Error::other("--install needs --allow <SID>"))),
        },
        Some("--uninstall") => report(install::uninstall()),
        Some("--query") => {
            struct Count(Vec<(String, u64)>);
            impl protocol::Sink for Count {
                fn volume(&mut self, root: &str) {
                    self.0.push((root.to_string(), 0));
                }
                fn entry(&mut self, _: u32, _: &str, _: bool, _: u64, _: u32) {
                    if let Some(last) = self.0.last_mut() {
                        last.1 += 1;
                    }
                }
            }
            println!("service: {:?}", client::status());
            let started = std::time::Instant::now();
            let mut count = Count(Vec::new());
            let result = client::snapshot(&mut count);
            for (root, names) in &count.0 {
                println!("{root} {names} names");
            }
            println!("{:?}", started.elapsed());
            report(result)
        }
        // As a user: the paths the running service returns that start with
        // the given text (case aside), to see what it lists and what not.
        Some("--find") => {
            struct Find {
                prefix: String,
                paths: Vec<String>,
                found: Vec<String>,
            }
            impl protocol::Sink for Find {
                fn volume(&mut self, _root: &str) {
                    self.paths.clear();
                }
                fn entry(&mut self, parent: u32, name: &str, _: bool, _: u64, _: u32) {
                    let path = match self.paths.get(parent as usize) {
                        Some(parent) => format!("{parent}\\{name}"),
                        None => name.to_string(),
                    };
                    if path.to_lowercase().starts_with(&self.prefix) && self.found.len() < 200 {
                        self.found.push(path.clone());
                    }
                    self.paths.push(path);
                }
            }
            let prefix = value("--find").unwrap_or_default().to_lowercase();
            let mut find = Find {
                prefix,
                paths: Vec::new(),
                found: Vec::new(),
            };
            let result = client::snapshot(&mut find);
            for path in &find.found {
                println!("{path}");
            }
            report(result)
        }
        Some("--dump") => {
            let root = value("--dump").unwrap_or_else(|| "C:".into());
            let started = std::time::Instant::now();
            let result = nohrs_indexer::mft::read_volume(&root).map(|table| {
                let read = started.elapsed();
                let policy = nohrs_indexer::mft::Policy {
                    profiles: Vec::new(),
                    allowed: Vec::new(),
                };
                let volume = nohrs_indexer::mft::tree(&table, &root, &policy);
                println!(
                    "{root}: {} records, {} names, read {read:?}, total {:?}",
                    table.records.len(),
                    volume.len(),
                    started.elapsed()
                );
            });
            report(result)
        }
        // As an administrator: what a subtree request would answer for one
        // folder, read straight from the volume's own table — the size map's
        // data before it goes over the pipe.
        Some("--tree") => {
            let path = value("--tree").unwrap_or_default();
            let levels = value("--levels")
                .and_then(|levels| levels.parse::<u16>().ok())
                .unwrap_or(3);
            let drive = path
                .split(['\\', '/'])
                .next()
                .unwrap_or_default()
                .trim_end_matches(':')
                .to_string();
            if drive.is_empty() || path.is_empty() {
                eprintln!("--tree needs a path on a lettered drive, like \"C:\\Users\"");
                return ExitCode::FAILURE;
            }
            let root = format!("{drive}:");
            let started = std::time::Instant::now();
            let result = nohrs_indexer::mft::read_volume(&root).map(|table| {
                let read = started.elapsed();
                let policy = nohrs_indexer::mft::Policy { profiles: Vec::new(), allowed: Vec::new() };
                let volume = nohrs_indexer::mft::tree(&table, &root, &policy);
                let built = started.elapsed();
                match volume.subtree(&path, levels) {
                    Some(tree) => {
                        println!(
                            "{path}: {} entries, {levels} levels, {} names; table read {read:?}, built {built:?}, answered {:?}",
                            tree.len(),
                            volume.len(),
                            started.elapsed() - built
                        );
                        if let Some(first) = tree.first() {
                            println!("total {} bytes", first.size);
                        }
                        for entry in tree.iter().skip(1).take(30) {
                            println!("{:>12}  {} {}", entry.size, if entry.dir { "D" } else { "F" }, entry.name);
                        }
                    }
                    None => println!("{path}: not in {root}'s table"),
                }
            });
            report(result)
        }
        // As a user, through the running service: what the app's own request
        // answers, and how long the service took to shape it.
        Some("--pipe-tree") => {
            let path = value("--pipe-tree").unwrap_or_default();
            let levels = value("--levels")
                .and_then(|levels| levels.parse::<u16>().ok())
                .unwrap_or(3);
            let hello = client::hello();
            let started = std::time::Instant::now();
            let result = client::subtree(&path, levels).map(|tree| {
                println!(
                    "{path}: {} entries, {levels} levels, answered {:?}, hello {hello:?}",
                    tree.len(),
                    started.elapsed()
                );
                if let Some(first) = tree.first() {
                    println!("total {} bytes", first.size);
                }
                for entry in tree.iter().skip(1).take(30) {
                    println!(
                        "{:>12}  {} {}",
                        entry.size,
                        if entry.dir { "D" } else { "F" },
                        entry.name
                    );
                }
            });
            report(result)
        }
        _ => {
            eprintln!(
                "prime-files-indexer: see the comment at the top of main.rs for the commands"
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("prime-files-indexer runs on Windows only");
}
