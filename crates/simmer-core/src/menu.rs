//! The command menu and the entry banner, rendered from env.json — data
//! in, text out. Rendered once at warm time and cached in the profile, so
//! showing it on entry costs the hot path nothing but a string copy.

use crate::envspec::EnvSpec;
use std::collections::BTreeMap;
use std::fmt::Write;

/// The command listing: shell name, then each category with its commands
/// aligned on the longest name.
pub fn render(spec: &EnvSpec) -> String {
    let mut by_category: BTreeMap<String, Vec<(&str, &str)>> = BTreeMap::new();
    for command in &spec.commands {
        by_category
            .entry(command.category.clone().unwrap_or_else(|| "general".to_string()))
            .or_default()
            .push((&command.name, command.help.as_deref().unwrap_or("")));
    }
    let width = spec.commands.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    let _ = writeln!(out, "{}", spec.name);
    for (category, commands) in by_category {
        let _ = writeln!(out, "\n[{category}]");
        for (name, help) in commands {
            let _ = writeln!(out, "  {name:<width$}  {help}");
        }
    }
    out
}

/// What a human sees on entering the shell: the motd, then the menu when
/// there are commands to list. `None` when the shell declares neither.
pub fn banner(spec: &EnvSpec) -> Option<String> {
    let motd = spec.motd.as_deref().filter(|m| !m.trim().is_empty());
    let menu = (!spec.commands.is_empty()).then(|| render(spec));
    match (motd, menu) {
        (None, None) => None,
        (Some(motd), None) => Some(motd.to_string()),
        (None, Some(menu)) => Some(menu.trim_end().to_string()),
        (Some(motd), Some(menu)) => Some(format!("{motd}\n{}", menu.trim_end())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: &str) -> EnvSpec {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn groups_by_category_and_aligns() {
        let spec = spec(
            r#"{"schema":1,"name":"dev","system":"s","profile":"p","commands":[
              {"name":"build","help":"compile","category":"code","run":{"kind":"script","source":"x"}},
              {"name":"deploy-nixos","help":"switch","category":"system","run":{"kind":"script","source":"x"}},
              {"name":"misc","run":{"kind":"exec","program":"x"}}]}"#,
        );
        let text = render(&spec);
        assert!(text.starts_with("dev\n\n[code]\n  build         compile\n"));
        assert!(text.contains("[general]\n  misc"));
        assert!(text.contains("[system]\n  deploy-nixos  switch"));
    }

    #[test]
    fn banner_is_motd_then_menu_or_nothing() {
        let bare = spec(r#"{"schema":1,"name":"dev","system":"s","profile":"p"}"#);
        assert_eq!(banner(&bare), None);
        let motd_only = spec(r#"{"schema":1,"name":"dev","system":"s","profile":"p","motd":"hi"}"#);
        assert_eq!(banner(&motd_only).as_deref(), Some("hi"));
        let both = spec(
            r#"{"schema":1,"name":"dev","system":"s","profile":"p","motd":"hi","commands":[
              {"name":"fmt","help":"format","category":"code","run":{"kind":"script","source":"x"}}]}"#,
        );
        assert_eq!(banner(&both).as_deref(), Some("hi\ndev\n\n[code]\n  fmt  format"));
    }
}
