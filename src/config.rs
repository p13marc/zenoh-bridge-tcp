//! Zenoh configuration setup for the bridge.

use anyhow::Result;
use std::path::Path;
use zenoh::config::Config;

/// Create a Zenoh config from a JSON5 configuration file
pub fn create_zenoh_config_from_file<P: AsRef<Path>>(path: P) -> Result<Config> {
    Config::from_file(path.as_ref())
        .map_err(|e| anyhow::anyhow!("Failed to load Zenoh config from file: {}", e))
}

/// Create a Zenoh config from a JSON5 string
#[allow(dead_code)] // Public API for potential library use
pub fn create_zenoh_config_from_json5(json5: &str) -> Result<Config> {
    Config::from_json5(json5)
        .map_err(|e| anyhow::anyhow!("Failed to parse Zenoh config from JSON5: {}", e))
}

/// Create and configure a Zenoh session based on mode and endpoints
pub fn create_zenoh_config(
    mode: &str,
    connect: Option<&String>,
    listen: Option<&String>,
) -> Result<Config> {
    let mut config = Config::default();

    // Set mode
    match mode {
        "peer" => {
            config
                .insert_json5("mode", "\"peer\"")
                .map_err(|e| anyhow::anyhow!("Failed to set mode: {}", e))?;
        }
        "client" => {
            config
                .insert_json5("mode", "\"client\"")
                .map_err(|e| anyhow::anyhow!("Failed to set mode: {}", e))?;
        }
        "router" => {
            config
                .insert_json5("mode", "\"router\"")
                .map_err(|e| anyhow::anyhow!("Failed to set mode: {}", e))?;
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Invalid mode: {}. Must be peer, client, or router",
                mode
            ));
        }
    }

    // Set connect endpoint
    if let Some(endpoint) = connect {
        config
            .insert_json5("connect/endpoints", &format!("[\"{}\"]", endpoint))
            .map_err(|e| anyhow::anyhow!("Failed to set connect endpoint: {}", e))?;
    }

    // Set listen endpoint
    if let Some(endpoint) = listen {
        config
            .insert_json5("listen/endpoints", &format!("[\"{}\"]", endpoint))
            .map_err(|e| anyhow::anyhow!("Failed to set listen endpoint: {}", e))?;
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_mode() {
        let config = create_zenoh_config("peer", None, None);
        assert!(config.is_ok());
    }

    #[test]
    fn test_client_mode() {
        let config = create_zenoh_config("client", None, None);
        assert!(config.is_ok());
    }

    #[test]
    fn test_router_mode() {
        let config = create_zenoh_config("router", None, None);
        assert!(config.is_ok());
    }

    #[test]
    fn test_invalid_mode() {
        let config = create_zenoh_config("invalid", None, None);
        assert!(config.is_err());
    }

    #[test]
    fn test_with_connect() {
        let endpoint = "tcp/localhost:7447".to_string();
        let config = create_zenoh_config("peer", Some(&endpoint), None);
        assert!(config.is_ok());
    }

    #[test]
    fn test_with_listen() {
        let endpoint = "tcp/0.0.0.0:7447".to_string();
        let config = create_zenoh_config("peer", None, Some(&endpoint));
        assert!(config.is_ok());
    }

    #[test]
    fn test_from_json5() {
        let json5 = r#"{ "mode": "peer" }"#;
        let config = create_zenoh_config_from_json5(json5);
        assert!(config.is_ok());
    }

    #[test]
    fn test_from_json5_invalid() {
        let json5 = r#"{ invalid json }"#;
        let config = create_zenoh_config_from_json5(json5);
        assert!(config.is_err());
    }
}
