use rusqlite::{Row, params};

use crate::protocol::content::Routing;
use crate::protocol::ids::AgentGroupId;
use crate::protocol::macros::text_enum;

use super::{SessionDb, SessionStoreError};

text_enum!(DestinationKind {
    Channel => "channel",
    Agent => "agent",
});

#[derive(Debug, Clone, PartialEq)]
pub struct Destination {
    pub name: String,
    pub display_name: Option<String>,
    pub kind: DestinationKind,
    pub channel_type: Option<String>,
    pub platform_id: Option<String>,
    pub agent_group_id: Option<AgentGroupId>,
}

impl SessionDb {
    /// Upserts the single default-reply routing row (id = 1).
    pub fn write_routing(&self, routing: &Routing) -> Result<(), SessionStoreError> {
        self.with(|conn| {
            conn.execute(
                "INSERT INTO session_routing (id, channel_type, platform_id, thread_id)
                 VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET channel_type = excluded.channel_type,
                    platform_id = excluded.platform_id, thread_id = excluded.thread_id",
                params![routing.channel_type, routing.platform_id, routing.thread_id],
            )?;
            Ok(())
        })
    }

    pub fn routing(&self) -> Result<Option<Routing>, SessionStoreError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT channel_type, platform_id, thread_id FROM session_routing WHERE id = 1",
                [],
                |row| {
                    Ok(Routing {
                        channel_type: row.get(0)?,
                        platform_id: row.get(1)?,
                        thread_id: row.get(2)?,
                    })
                },
            )
            .map_or_else(
                |err| match err {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                },
                |routing| Ok(Some(routing)),
            )
        })
    }

    pub fn replace_destinations(
        &self,
        destinations: &[Destination],
    ) -> Result<(), SessionStoreError> {
        self.with(|conn| {
            conn.execute("DELETE FROM destinations", [])?;
            for dest in destinations {
                conn.execute(
                    "INSERT INTO destinations
                       (name, display_name, type, channel_type, platform_id, agent_group_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        dest.name,
                        dest.display_name,
                        dest.kind,
                        dest.channel_type,
                        dest.platform_id,
                        dest.agent_group_id,
                    ],
                )?;
            }
            Ok(())
        })
    }

    pub fn destination(&self, name: &str) -> Result<Option<Destination>, SessionStoreError> {
        self.with(|conn| {
            conn.query_row(
                &format!("{SELECT_DESTINATION} WHERE name = ?1"),
                params![name],
                from_row,
            )
            .map_or_else(
                |err| match err {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                },
                |dest| Ok(Some(dest)),
            )
        })
    }

    pub fn destinations(&self) -> Result<Vec<Destination>, SessionStoreError> {
        self.with(|conn| {
            conn.prepare(&format!("{SELECT_DESTINATION} ORDER BY name"))?
                .query_map([], from_row)?
                .collect()
        })
    }
}

const SELECT_DESTINATION: &str = "SELECT name, display_name, type, channel_type, platform_id,
        agent_group_id FROM destinations";

fn from_row(row: &Row<'_>) -> Result<Destination, rusqlite::Error> {
    Ok(Destination {
        name: row.get(0)?,
        display_name: row.get(1)?,
        kind: row.get(2)?,
        channel_type: row.get(3)?,
        platform_id: row.get(4)?,
        agent_group_id: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_session_db;

    fn web_routing() -> Routing {
        Routing {
            channel_type: Some("web".to_owned()),
            platform_id: Some("chat-1".to_owned()),
            thread_id: None,
        }
    }

    #[test]
    fn routing_upserts_a_single_row() {
        let (_tmp, db) = test_session_db();
        assert_eq!(db.routing().expect("read"), None);

        db.write_routing(&web_routing()).expect("write");
        assert_eq!(db.routing().expect("read"), Some(web_routing()));

        let rewired = Routing {
            platform_id: Some("chat-2".to_owned()),
            ..web_routing()
        };
        db.write_routing(&rewired).expect("rewrite");
        assert_eq!(db.routing().expect("read"), Some(rewired));
    }

    #[test]
    fn destinations_are_replaced_wholesale_and_looked_up_by_name() {
        let (_tmp, db) = test_session_db();
        let dests = vec![
            Destination {
                name: "family".to_owned(),
                display_name: Some("Family Chat".to_owned()),
                kind: DestinationKind::Channel,
                channel_type: Some("web".to_owned()),
                platform_id: Some("chat-1".to_owned()),
                agent_group_id: None,
            },
            Destination {
                name: "coder".to_owned(),
                display_name: None,
                kind: DestinationKind::Agent,
                channel_type: None,
                platform_id: None,
                agent_group_id: Some(AgentGroupId::new("ag-coder")),
            },
        ];
        db.replace_destinations(&dests).expect("replace");
        let mut by_name = dests.clone();
        by_name.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(db.destinations().expect("list"), by_name);
        assert_eq!(
            db.destination("coder").expect("get").map(|d| d.kind),
            Some(DestinationKind::Agent)
        );
        assert_eq!(db.destination("missing").expect("get"), None);

        db.replace_destinations(&[]).expect("clear");
        assert!(db.destinations().expect("list").is_empty());
    }
}
