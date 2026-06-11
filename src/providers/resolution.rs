use rusqlite::Connection;

use crate::db::agent_groups::AgentGroup;
use crate::db::endpoints::{self, Endpoint};
use crate::protocol::ids::EndpointName;

/// Everything the native provider needs to call an OpenAI-compatible API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInference {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolutionError {
    #[error("group {group:?} has no endpoint configured and no CLAW_DEFAULT_ENDPOINT is set")]
    NoEndpoint { group: String },
    #[error("endpoint {0:?} does not exist")]
    UnknownEndpoint(EndpointName),
    #[error("group {group:?} has no model configured and no CLAW_DEFAULT_MODEL is set")]
    NoModel { group: String },
    #[error("endpoint {endpoint:?} names api_key_env {var:?} but the variable is not set")]
    MissingKeyEnv { endpoint: EndpointName, var: String },
}

/// Selection chain (§8.7): group row → instance default → error.
#[must_use]
pub fn endpoint_name_for(
    group: &AgentGroup,
    default_endpoint: Option<&str>,
) -> Option<EndpointName> {
    group
        .endpoint
        .clone()
        .or_else(|| default_endpoint.map(EndpointName::new))
}

#[must_use]
pub fn model_for(group: &AgentGroup, default_model: Option<&str>) -> Option<String> {
    group
        .model
        .clone()
        .or_else(|| default_model.map(str::to_owned))
}

/// Stored key wins; an `api_key_env` reference must actually resolve; keyless is valid
/// (local inference servers usually take no auth).
pub fn api_key_for(
    endpoint: &Endpoint,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, ResolutionError> {
    if let Some(key) = &endpoint.api_key {
        return Ok(Some(key.clone()));
    }
    match &endpoint.api_key_env {
        None => Ok(None),
        Some(var) => env(var)
            .map(Some)
            .ok_or_else(|| ResolutionError::MissingKeyEnv {
                endpoint: endpoint.name.clone(),
                var: var.clone(),
            }),
    }
}

pub fn resolve_inference(
    conn: &Connection,
    group: &AgentGroup,
    default_endpoint: Option<&str>,
    default_model: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedInference, ResolutionError> {
    let endpoint_name =
        endpoint_name_for(group, default_endpoint).ok_or_else(|| ResolutionError::NoEndpoint {
            group: group.name.clone(),
        })?;
    let endpoint = endpoints::get(conn, &endpoint_name)
        .ok()
        .flatten()
        .ok_or(ResolutionError::UnknownEndpoint(endpoint_name))?;
    let model = model_for(group, default_model).ok_or_else(|| ResolutionError::NoModel {
        group: group.name.clone(),
    })?;
    let api_key = api_key_for(&endpoint, env)?;
    Ok(ResolvedInference {
        base_url: endpoint.base_url,
        api_key,
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CentralDb, agent_groups};

    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |var| {
            pairs
                .iter()
                .find(|(key, _)| *key == var)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    struct Fixture {
        db: CentralDb,
        group: AgentGroup,
    }

    fn fixture() -> Fixture {
        let db = CentralDb::open_in_memory().expect("open");
        let group = db
            .with(|conn| {
                endpoints::create(
                    conn,
                    &EndpointName::new("local"),
                    "http://localhost:8000/v1",
                )?;
                agent_groups::create(conn, "Chat", "chat")
            })
            .expect("fixture");
        Fixture { db, group }
    }

    #[test]
    fn group_settings_win_over_instance_defaults() {
        let fix = fixture();
        let mut group = fix.group.clone();
        group.endpoint = Some(EndpointName::new("local"));
        group.model = Some("gemma4-moe".to_owned());

        let resolved = fix
            .db
            .with(|conn| {
                Ok(resolve_inference(
                    conn,
                    &group,
                    Some("other-default"),
                    Some("default-model"),
                    |_| None,
                ))
            })
            .expect("db")
            .expect("resolve");
        assert_eq!(resolved.base_url, "http://localhost:8000/v1");
        assert_eq!(resolved.model, "gemma4-moe");
        assert_eq!(resolved.api_key, None);
    }

    #[test]
    fn instance_defaults_fill_unset_group_fields() {
        let fix = fixture();
        let resolved = fix
            .db
            .with(|conn| {
                Ok(resolve_inference(
                    conn,
                    &fix.group,
                    Some("local"),
                    Some("default-model"),
                    |_| None,
                ))
            })
            .expect("db")
            .expect("resolve");
        assert_eq!(resolved.model, "default-model");
    }

    #[test]
    fn missing_links_in_the_chain_are_distinct_errors() {
        let fix = fixture();
        let no_endpoint = fix
            .db
            .with(|conn| {
                Ok(resolve_inference(conn, &fix.group, None, Some("m"), |_| {
                    None
                }))
            })
            .expect("db");
        assert_eq!(
            no_endpoint,
            Err(ResolutionError::NoEndpoint {
                group: "Chat".to_owned()
            })
        );

        let unknown = fix
            .db
            .with(|conn| {
                Ok(resolve_inference(
                    conn,
                    &fix.group,
                    Some("nope"),
                    Some("m"),
                    |_| None,
                ))
            })
            .expect("db");
        assert_eq!(
            unknown,
            Err(ResolutionError::UnknownEndpoint(EndpointName::new("nope")))
        );

        let no_model = fix
            .db
            .with(|conn| {
                Ok(resolve_inference(
                    conn,
                    &fix.group,
                    Some("local"),
                    None,
                    |_| None,
                ))
            })
            .expect("db");
        assert_eq!(
            no_model,
            Err(ResolutionError::NoModel {
                group: "Chat".to_owned()
            })
        );
    }

    #[test]
    fn api_key_resolution_prefers_stored_then_env_then_keyless() {
        let stored = Endpoint {
            name: EndpointName::new("a"),
            base_url: String::new(),
            api_key: Some("sk-stored".to_owned()),
            api_key_env: Some("IGNORED".to_owned()),
            notes: None,
            created_at: String::new(),
        };
        assert_eq!(
            api_key_for(&stored, env_from(&[])).expect("ok"),
            Some("sk-stored".to_owned())
        );

        let env_backed = Endpoint {
            api_key: None,
            api_key_env: Some("OPENROUTER_API_KEY".to_owned()),
            ..stored.clone()
        };
        assert_eq!(
            api_key_for(&env_backed, env_from(&[("OPENROUTER_API_KEY", "sk-env")])).expect("ok"),
            Some("sk-env".to_owned())
        );
        assert_eq!(
            api_key_for(&env_backed, env_from(&[])),
            Err(ResolutionError::MissingKeyEnv {
                endpoint: EndpointName::new("a"),
                var: "OPENROUTER_API_KEY".to_owned()
            })
        );

        let keyless = Endpoint {
            api_key: None,
            api_key_env: None,
            ..stored
        };
        assert_eq!(api_key_for(&keyless, env_from(&[])).expect("ok"), None);
    }
}
