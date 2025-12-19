use clap::{AppSettings, IntoApp, Parser, Subcommand};
use clap_complete::{
    generate,
    shells::{Bash, Fish, Zsh},
};
use core::commands;
use std::path::PathBuf;
use utils::app_config::AppConfig;
use utils::error::Result;

#[derive(Parser, Debug)]
#[clap(
    name = "sgc",
    author,
    about = "FogROS2-SGC: Secure Global Connectivity for ROS2",
    version
)]
#[clap(setting = AppSettings::SubcommandRequired)]
#[clap(global_setting(AppSettings::DeriveDisplayOrder))]
pub struct Cli {
    /// Path to config file (default: uses SGC_CONFIG env var)
    #[clap(short, long, parse(from_os_str), value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the SGC router
    #[clap(name = "router")]
    Router,

    /// Initialize a new group secret
    #[clap(name = "init")]
    Init {
        /// Name for the group secret (creates ./secrets/<name>/secret.key)
        name: String,
    },

    /// Validate config and test connectivity
    #[clap(name = "check")]
    Check,

    /// Show current configuration
    #[clap(name = "config")]
    Config,

    /// Generate shell completion scripts
    #[clap(name = "completion")]
    Completion {
        #[clap(subcommand)]
        subcommand: CompletionSubcommand,
    },
}

#[derive(Subcommand, PartialEq, Debug)]
enum CompletionSubcommand {
    #[clap(about = "Generate bash completions")]
    Bash,
    #[clap(about = "Generate zsh completions")]
    Zsh,
    #[clap(about = "Generate fish completions")]
    Fish,
}

pub fn cli_match() -> Result<()> {
    let cli = Cli::parse();

    // For init command, we don't need config loaded
    if let Commands::Init { name } = &cli.command {
        return commands::init(name);
    }

    // Merge any additional config file
    AppConfig::merge_config(cli.config.as_deref())?;

    let app = Cli::into_app();
    AppConfig::merge_args(app)?;

    match &cli.command {
        Commands::Router => commands::router()?,
        Commands::Check => commands::check()?,
        Commands::Config => commands::config()?,
        Commands::Init { .. } => unreachable!(),
        Commands::Completion { subcommand } => {
            let mut app = Cli::into_app();
            match subcommand {
                CompletionSubcommand::Bash => {
                    generate(Bash, &mut app, "sgc", &mut std::io::stdout());
                }
                CompletionSubcommand::Zsh => {
                    generate(Zsh, &mut app, "sgc", &mut std::io::stdout());
                }
                CompletionSubcommand::Fish => {
                    generate(Fish, &mut app, "sgc", &mut std::io::stdout());
                }
            }
        }
    }

    Ok(())
}
