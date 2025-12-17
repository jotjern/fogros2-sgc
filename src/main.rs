#[cfg(not(debug_assertions))]
use human_panic::setup_panic;

#[cfg(debug_assertions)]
extern crate better_panic;

use std::env;
use std::fs;
use utils::app_config::AppConfig;
use utils::error::Result;

fn main() -> Result<()> {
    #[cfg(not(debug_assertions))]
    {
        setup_panic!();
    }

    #[cfg(debug_assertions)]
    {
        better_panic::Settings::debug()
            .most_recent_first(false)
            .lineno_suffix(true)
            .verbosity(better_panic::Verbosity::Full)
            .install();
    }

    ::std::env::set_var("RUST_LOG", "debug");
    env_logger::init();

    let include_path = match env::var_os("SGC_CONFIG") {
        Some(config_file) => {
            format!(
                "{}{}",
                "./src/resources/",
                config_file.into_string().unwrap()
            )
        }
        None => "./src/resources/automatic.toml".to_owned(),
    };
    println!("Using config file : {}", include_path);
    let config_contents = fs::read_to_string(include_path).expect("config file not found!");

    AppConfig::init(Some(&config_contents))?;

    cli::cli_match()?;

    Ok(())
}
