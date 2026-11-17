use agent_client_protocol::v2::schema as acp_v2;
use ui::IconName;

/// A coding agent Icarus can run in a tab, as the server names it (`agents.default`, an agent's
/// `provider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Harness {
    pub id: &'static str,
    pub name: &'static str,
    pub icon: IconName,
    /// Other words a search for it may use, such as its command.
    pub aliases: &'static [&'static str],
}

pub const HARNESSES: [Harness; 2] = [
    Harness {
        id: "claude",
        name: "Claude",
        icon: IconName::AiClaude,
        aliases: &["claude code", "anthropic"],
    },
    Harness {
        id: "codex",
        name: "Codex",
        icon: IconName::AiOpenAi,
        aliases: &["openai"],
    },
];

pub fn harness(id: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|harness| harness.id == id)
}

/// The agent id Icarus registers in the default settings (`agent_servers.Icarus`).
pub const ICARUS_AGENT: &str = "Icarus";

/// The agent id of `icarus acp` for one harness, or for the server's default one.
pub fn icarus_agent_id(provider: Option<&str>) -> String {
    match provider {
        Some(provider) => format!("{ICARUS_AGENT} {provider}"),
        None => ICARUS_AGENT.to_string(),
    }
}

/// The harness an `icarus acp` agent id was started for, if it names one.
pub fn harness_for_agent_id(agent_id: &str) -> Option<&'static Harness> {
    harness(agent_id.strip_prefix(ICARUS_AGENT)?.strip_prefix(' ')?)
}

/// The harness an Icarus session reports in its `_meta.icarus.provider`.
pub fn session_harness(meta: Option<&acp_v2::Meta>) -> Option<&'static Harness> {
    harness(meta?.get("icarus")?.get("provider")?.as_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_names_its_harness_in_its_metadata() {
        let meta: acp_v2::Meta = serde_json::from_value(serde_json::json!({
            "icarus": { "provider": "codex" }
        }))
        .expect("meta");
        assert_eq!(
            session_harness(Some(&meta)).map(|harness| harness.name),
            Some("Codex")
        );
        let unknown: acp_v2::Meta =
            serde_json::from_value(serde_json::json!({ "icarus": { "provider": "nope" } }))
                .expect("meta");
        assert_eq!(session_harness(Some(&unknown)), None);
        assert_eq!(session_harness(None), None);
    }

    #[test]
    fn each_harness_has_its_own_agent_id() {
        assert_eq!(icarus_agent_id(None), "Icarus");
        assert_eq!(
            harness_for_agent_id(&icarus_agent_id(Some("codex"))).map(|harness| harness.id),
            Some("codex")
        );
        assert_eq!(harness_for_agent_id("Icarus"), None);
        assert_eq!(harness_for_agent_id("Icaruscodex"), None);
    }
}
