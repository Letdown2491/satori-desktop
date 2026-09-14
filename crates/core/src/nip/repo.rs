//! NIP-34 git repositories — tag parsing slice of `src/manifest/repo.ts`.

#[derive(Clone)]
pub struct RepoData {
    pub name: String,
    pub description: String,
    /// Clone URLs (monospace blocks).
    pub clone: Vec<String>,
    /// Web interface URLs.
    pub web: Vec<String>,
    pub relays: Vec<String>,
    pub maintainers: Vec<String>,
    pub topics: Vec<String>,
}

use nostr::prelude::*;

pub fn parse(ev: &Event) -> Option<RepoData> {
    let mut d = RepoData {
        name: String::new(),
        description: ev.content.clone(),
        clone: Vec::new(),
        web: Vec::new(),
        relays: Vec::new(),
        maintainers: Vec::new(),
        topics: Vec::new(),
    };
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("name") if parts.len() >= 2 => d.name = parts[1].clone(),
            Some("clone") if parts.len() >= 2 => d.clone.push(parts[1].clone()),
            Some("web") if parts.len() >= 2 => d.web.push(parts[1].clone()),
            Some("rel") if parts.len() >= 2 => d.relays.push(parts[1].clone()),
            Some("maintainers") if parts.len() >= 2 => {
                d.maintainers.extend(parts[1..].iter().cloned())
            }
            Some("t") if parts.len() >= 2 => d.topics.push(parts[1].clone()),
            _ => {}
        }
    }
    (!(d.name.is_empty() && d.clone.is_empty())).then_some(d)
}
