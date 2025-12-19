//! CLI command implementations.

use crate::db::{get_redis_url, test_redis_connection};
use crate::topic_manager::ros_topic_discovery;
use std::fs;
use std::io::Write;
use std::path::Path;
use utils::app_config::AppConfig;
use utils::error::Result;

/// Generate a new group secret.
pub fn init(name: &str) -> Result<()> {
    let secret_dir = format!("./secrets/{}", name);
    let secret_path = format!("{}/secret.key", secret_dir);

    // Check if already exists
    if Path::new(&secret_path).exists() {
        eprintln!(
            "Error: Group secret '{}' already exists at {}",
            name, secret_path
        );
        eprintln!("To regenerate, first delete: rm -rf {}", secret_dir);
        std::process::exit(1);
    }

    // Create directory
    fs::create_dir_all(&secret_dir).map_err(|e| {
        utils::error::Error::new(&format!("Failed to create directory {}: {}", secret_dir, e))
    })?;

    // Generate 32 bytes of random data
    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret).map_err(|e| {
        utils::error::Error::new(&format!("Failed to generate random secret: {}", e))
    })?;

    // Write secret file
    let mut file = fs::File::create(&secret_path).map_err(|e| {
        utils::error::Error::new(&format!("Failed to create {}: {}", secret_path, e))
    })?;
    file.write_all(&secret)
        .map_err(|e| utils::error::Error::new(&format!("Failed to write secret: {}", e)))?;

    println!("✓ Created group secret: {}", secret_path);
    println!();
    println!("Next steps:");
    println!("  1. Copy ./secrets/{} to all robots in your fleet", name);
    println!("  2. Set group_secret = \"{}\" in your config file", name);
    println!("  3. Run: sgc check");

    Ok(())
}

/// Validate config and test connectivity.
pub fn check() -> Result<()> {
    println!("Checking configuration...\n");
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // Load config
    let config = match AppConfig::fetch() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ Failed to load config: {}", e);
            std::process::exit(1);
        }
    };

    // Validate config structure
    if let Err(validation_errors) = config.validate() {
        for e in validation_errors {
            errors.push(e);
        }
    } else {
        println!("✓ Config file valid");
    }

    // Check group secret exists
    let secret_path = config.secret_path();
    if secret_path.exists() {
        match fs::metadata(&secret_path) {
            Ok(meta) if meta.len() >= 16 => {
                println!("✓ Group secret found: {}", secret_path.display());
            }
            Ok(meta) => {
                warnings.push(format!(
                    "Group secret is only {} bytes (recommended: 32)",
                    meta.len()
                ));
            }
            Err(e) => {
                errors.push(format!("Cannot read {}: {}", secret_path.display(), e));
            }
        }
    } else {
        errors.push(format!(
            "Group secret not found: {}\n  Run: sgc init {}",
            secret_path.display(),
            config.group_secret
        ));
    }

    // Test signaling server connectivity
    print!("  Testing signaling server... ");
    std::io::stdout().flush().ok();
    match test_signaling_server(&config.signaling_server) {
        Ok(()) => println!("✓ Signaling server reachable: {}", config.signaling_server),
        Err(e) => {
            println!("✗");
            errors.push(format!(
                "Cannot connect to signaling server {}: {}",
                config.signaling_server, e
            ));
        }
    }

    // Test Redis connectivity
    print!("  Testing routing server... ");
    std::io::stdout().flush().ok();
    let redis_url = get_redis_url();
    match test_redis_connection(&redis_url) {
        Ok(()) => println!("✓ Routing server reachable: {}", config.routing_server),
        Err(e) => {
            println!("✗");
            errors.push(format!(
                "Cannot connect to routing server {}: {}",
                config.routing_server, e
            ));
        }
    }

    // Show topics
    if !config.topics.is_empty() {
        println!("\nConfigured topics:");
        for topic in &config.topics {
            println!("  {} ({}) - {}", topic.name, topic.topic_type, topic.role);
        }
    }

    // Print warnings
    for w in &warnings {
        println!("\n⚠ Warning: {}", w);
    }

    // Print errors and exit
    if !errors.is_empty() {
        println!("\n✗ {} error(s) found:", errors.len());
        for e in &errors {
            println!("  - {}", e);
        }
        std::process::exit(1);
    }

    println!("\n✓ All checks passed. Ready to run: sgc router");
    Ok(())
}

/// Test WebSocket connection to signaling server.
fn test_signaling_server(url: &str) -> std::result::Result<(), String> {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    // Parse ws:// or wss:// URL to get host:port
    let url = url.trim_start_matches("ws://").trim_start_matches("wss://");
    let host_port = url.split('/').next().unwrap_or(url);

    // Add default port if missing
    let host_port = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{}:8000", host_port)
    };

    // Resolve hostname to socket address
    let addr = host_port
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed: {}", e))?
        .next()
        .ok_or_else(|| "No addresses found".to_string())?;

    TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| e.to_string())?;

    Ok(())
}

/// Start the SGC router.
#[tokio::main]
async fn router_async_loop() {
    let config = match AppConfig::fetch() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config: {}", e);
            std::process::exit(1);
        }
    };

    // Validate config
    if let Err(errors) = config.validate() {
        eprintln!("Configuration errors:");
        for e in errors {
            eprintln!("  - {}", e);
        }
        eprintln!("\nRun 'sgc check' for more details.");
        std::process::exit(1);
    }

    // Check secret exists
    let secret_path = config.secret_path();
    if !secret_path.exists() {
        eprintln!("Error: Group secret not found: {}", secret_path.display());
        eprintln!("Run: sgc init {}", config.group_secret);
        std::process::exit(1);
    }

    info!("Starting SGC router...");
    info!("  Group: {}", config.group_secret);
    info!("  Signaling: {}", config.signaling_server);
    info!("  Routing: {}", config.routing_server);
    info!("  Topics: {}", config.topics.len());

    ros_topic_discovery().await;
}

pub fn router() -> Result<()> {
    router_async_loop();
    Ok(())
}

/// Print current configuration.
pub fn config() -> Result<()> {
    let config = AppConfig::fetch()?;
    println!("Current configuration:");
    println!("  group_secret: {}", config.group_secret);
    println!("  signaling_server: {}", config.signaling_server);
    println!("  routing_server: {}", config.routing_server);
    println!("  topics:");
    for topic in &config.topics {
        println!(
            "    - {} ({}) [{}]",
            topic.name, topic.topic_type, topic.role
        );
    }
    Ok(())
}
