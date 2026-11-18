use serde_json::Value;

const SCRIPT: &str = include_str!("../templates/orca-flow.ts");
const DEFAULTS: &str = include_str!("../templates/orca-flow.json");
const PLACEHOLDER: &str = "__ICARUS_CONFIG__";

/// The template's options, before any are changed.
pub fn orca_flow_defaults() -> Value {
    serde_json::from_str(DEFAULTS).expect("orca-flow.json is valid JSON")
}

/// The Orca PR flow script with `config` as its options.
pub fn orca_flow(config: &Value) -> String {
    let json = serde_json::to_string_pretty(config).unwrap_or_else(|_| "{}".to_owned());
    SCRIPT.replacen(PLACEHOLDER, &json, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_options_replace_the_placeholder() {
        let script = orca_flow(&orca_flow_defaults());
        assert!(!script.contains(PLACEHOLDER));
        assert!(script.contains("\"mergingCheckName\": \"Reviews satisfied\""));
    }
}
