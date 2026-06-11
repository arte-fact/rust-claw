use std::path::PathBuf;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("invalid value {value:?} for {var}: {reason}")]
    Invalid {
        var: &'static str,
        value: String,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub port: u16,
    pub auth_token: Option<String>,
    pub timezone: String,
    pub default_endpoint: Option<String>,
    pub default_model: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|var| std::env::var(var).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let data_dir =
            lookup("CLAW_DATA_DIR").map_or_else(|| PathBuf::from("/data"), PathBuf::from);
        let port = parse_port(lookup("CLAW_PORT"))?;
        let timezone = lookup("CLAW_TIMEZONE").unwrap_or_else(|| "UTC".to_owned());
        Ok(Self {
            data_dir,
            port,
            auth_token: lookup("CLAW_AUTH_TOKEN"),
            timezone,
            default_endpoint: lookup("CLAW_DEFAULT_ENDPOINT"),
            default_model: lookup("CLAW_DEFAULT_MODEL"),
        })
    }

    pub fn central_db_path(&self) -> PathBuf {
        self.data_dir.join("central.db")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn groups_dir(&self) -> PathBuf {
        self.data_dir.join("groups")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join("claw.sock")
    }
}

fn parse_port(raw: Option<String>) -> Result<u16, ConfigError> {
    match raw {
        None => Ok(8080),
        Some(value) => value
            .parse()
            .map_err(|err: std::num::ParseIntError| ConfigError::Invalid {
                var: "CLAW_PORT",
                value,
                reason: err.to_string(),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |var| {
            pairs
                .iter()
                .find(|(key, _)| *key == var)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn empty_environment_yields_defaults() {
        let config = Config::from_lookup(|_| None).expect("defaults must parse");
        assert_eq!(config.data_dir, PathBuf::from("/data"));
        assert_eq!(config.port, 8080);
        assert_eq!(config.timezone, "UTC");
        assert_eq!(config.auth_token, None);
        assert_eq!(config.default_endpoint, None);
        assert_eq!(config.default_model, None);
    }

    #[test]
    fn environment_overrides_are_read() {
        let pairs = [
            ("CLAW_DATA_DIR", "/srv/claw"),
            ("CLAW_PORT", "9090"),
            ("CLAW_AUTH_TOKEN", "secret"),
            ("CLAW_TIMEZONE", "Europe/Paris"),
            ("CLAW_DEFAULT_ENDPOINT", "openrouter"),
            ("CLAW_DEFAULT_MODEL", "gemma4-moe"),
        ];
        let config = Config::from_lookup(lookup_from(&pairs)).expect("overrides must parse");
        assert_eq!(config.data_dir, PathBuf::from("/srv/claw"));
        assert_eq!(config.port, 9090);
        assert_eq!(config.auth_token.as_deref(), Some("secret"));
        assert_eq!(config.timezone, "Europe/Paris");
        assert_eq!(config.default_endpoint.as_deref(), Some("openrouter"));
        assert_eq!(config.default_model.as_deref(), Some("gemma4-moe"));
    }

    #[test]
    fn invalid_port_is_rejected_with_context() {
        let pairs = [("CLAW_PORT", "not-a-port")];
        let err = Config::from_lookup(lookup_from(&pairs)).expect_err("bad port must fail");
        let ConfigError::Invalid { var, value, .. } = err;
        assert_eq!(var, "CLAW_PORT");
        assert_eq!(value, "not-a-port");
    }

    #[test]
    fn derived_paths_live_under_data_dir() {
        let pairs = [("CLAW_DATA_DIR", "/srv/claw")];
        let config = Config::from_lookup(lookup_from(&pairs)).expect("must parse");
        assert_eq!(
            config.central_db_path(),
            PathBuf::from("/srv/claw/central.db")
        );
        assert_eq!(config.sessions_dir(), PathBuf::from("/srv/claw/sessions"));
        assert_eq!(config.groups_dir(), PathBuf::from("/srv/claw/groups"));
        assert_eq!(config.logs_dir(), PathBuf::from("/srv/claw/logs"));
        assert_eq!(config.socket_path(), PathBuf::from("/srv/claw/claw.sock"));
    }
}
