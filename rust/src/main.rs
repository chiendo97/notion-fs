mod cache;
mod fs;
mod notion;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};

use cache::{Config, NotionCache};
use fs::NotionFS;
use notion::NotionClient;

#[derive(Parser)]
#[command(name = "notion-fs", about = "Notion FUSE filesystem")]
struct Cli {
    /// Directory to mount the filesystem on
    mountpoint: String,

    /// Path to notion.yaml config
    #[arg(long = "config")]
    config_path: Option<String>,

    /// Directory for JSON cache files
    #[arg(long = "cache-dir", default_value = "./cache")]
    cache_dir: String,
}

fn load_config(config_path: Option<&str>) -> Config {
    let paths_to_try: Vec<PathBuf> = if let Some(p) = config_path {
        vec![PathBuf::from(p)]
    } else {
        vec![
            PathBuf::from("/config/notion.yaml"),
            PathBuf::from("./config/notion.yaml"),
        ]
    };

    for p in &paths_to_try {
        if p.exists() {
            let content = std::fs::read_to_string(p).unwrap_or_else(|e| {
                eprintln!("Error reading config {}: {}", p.display(), e);
                std::process::exit(1);
            });
            return serde_yaml::from_str(&content).unwrap_or_else(|e| {
                eprintln!("Error parsing config: {}", e);
                std::process::exit(1);
            });
        }
    }

    if let Some(path) = config_path {
        eprintln!("Error: config not found at {}", path);
    } else {
        eprintln!("Error: no config file found");
    }
    std::process::exit(1);
}

fn main() {
    let cli = Cli::parse();

    let token = std::env::var("NOTION_TOKEN").unwrap_or_else(|_| {
        eprintln!("Error: NOTION_TOKEN environment variable not set");
        std::process::exit(1);
    });

    let config = load_config(cli.config_path.as_deref());

    if config.projects.is_empty() {
        eprintln!("Error: no projects configured");
        std::process::exit(1);
    }

    let client = NotionClient::new(token);
    let cache = NotionCache::new(config, client, Some(PathBuf::from(&cli.cache_dir)));

    let loaded = cache.load_from_disk();
    if loaded > 0 {
        eprintln!("Loaded {} tickets from disk cache", loaded);
    } else {
        eprintln!("No disk cache found, fetching from Notion...");

        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg} {pos} tickets")
                .unwrap(),
        );
        pb.set_message("Fetching from Notion...");
        pb.enable_steady_tick(Duration::from_millis(100));

        let pb_ref = &pb;
        let total = cache.refresh(None, Some(&|tickets_so_far| {
            pb_ref.set_position(tickets_so_far as u64);
        }));

        pb.finish_and_clear();
        eprintln!("Loaded {} tickets", total);
    }

    let fs = NotionFS::new(Arc::new(cache));

    eprintln!("Mounted at {}", cli.mountpoint);

    let mut config = fuser::Config::default();
    config.n_threads = Some(4);
    config.clone_fd = true;

    let session = fuser::spawn_mount2(fs, &cli.mountpoint, &config).unwrap_or_else(|e| {
        eprintln!("Error mounting filesystem: {}", e);
        std::process::exit(1);
    });

    // Wait for Ctrl+C, then unmount cleanly
    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .expect("Error setting Ctrl+C handler");

    rx.recv().ok();
    eprintln!("\nUnmounting...");
    session.umount_and_join().unwrap_or_else(|e| {
        eprintln!("Error unmounting: {}", e);
    });
}
