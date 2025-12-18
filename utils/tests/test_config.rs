use utils::app_config::*;

#[test]
fn fetch_config() {
    let config_contents = include_str!("resources/test_config.toml");
    AppConfig::init(Some(config_contents)).unwrap();

    let config = AppConfig::fetch().unwrap();
    assert_eq!(config.group_secret, "test");
    assert_eq!(config.signaling_server, "ws://localhost:8000");
    assert_eq!(config.routing_server, "localhost:6379");
    assert_eq!(config.topics.len(), 1);
    assert_eq!(config.topics[0].name, "/test_topic");
    assert_eq!(config.topics[0].role, "publisher");
}

#[test]
fn validate_config() {
    let config_contents = include_str!("resources/test_config.toml");
    AppConfig::init(Some(config_contents)).unwrap();

    let config = AppConfig::fetch().unwrap();
    assert!(config.validate().is_ok());
}

#[test]
fn validate_config_bad_role() {
    let bad_config = r#"
group_secret = "test"
signaling_server = "ws://localhost:8000"
routing_server = "localhost:6379"

[[topics]]
name = "/test"
type = "std_msgs/msg/String"
role = "invalid"
"#;
    AppConfig::init(Some(bad_config)).unwrap();
    let config = AppConfig::fetch().unwrap();
    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err()[0].contains("role must be"));
}
