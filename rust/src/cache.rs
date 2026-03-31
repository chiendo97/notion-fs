use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

use crate::notion::{NotionClient, Ticket};

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

fn default_epic() -> String {
    "Epic".to_string()
}

fn default_select() -> String {
    "select".to_string()
}

fn default_sort_date() -> String {
    "Sort Date".to_string()
}

fn default_formula() -> String {
    "formula".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ProjectConfig {
    pub database_id: String,
    #[serde(default)]
    pub epics_database_id: String,
    #[serde(default = "default_epic")]
    pub prop_epic: String,
    #[serde(default = "default_select")]
    pub epic_status_type: String,
    #[serde(default = "default_sort_date")]
    pub date_property: String,
    #[serde(default = "default_formula")]
    pub date_property_type: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct Config {
    #[serde(default)]
    pub default_project: String,
    #[serde(default)]
    pub projects: HashMap<String, ProjectConfig>,
    #[serde(default)]
    pub users: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Tree type
// ---------------------------------------------------------------------------

/// project slug -> assignee slug -> status slug -> tickets
pub type Tree = HashMap<String, HashMap<String, HashMap<String, Vec<Ticket>>>>;

// ---------------------------------------------------------------------------
// slugify
// ---------------------------------------------------------------------------

pub fn slugify(name: &str) -> String {
    // NFKD normalize, then strip combining marks (diacritics)
    let nfkd: String = name.nfkd().collect();
    let without_combining: String = nfkd
        .chars()
        .filter(|c| {
            use unicode_normalization::char::is_combining_mark;
            !is_combining_mark(*c)
        })
        .collect();

    // Handle Vietnamese d-with-stroke which is NOT a combining sequence
    let replaced = without_combining.replace('đ', "d").replace('Đ', "D");

    // Lowercase, collapse non-alphanumeric runs to hyphens, trim hyphens
    let mut slug = String::with_capacity(replaced.len());
    let mut prev_was_sep = true; // avoids leading hyphen
    for c in replaced.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_was_sep = false;
        } else if !prev_was_sep {
            slug.push('-');
            prev_was_sep = true;
        }
    }

    // Strip trailing hyphen
    if slug.ends_with('-') {
        slug.pop();
    }

    slug
}

// ---------------------------------------------------------------------------
// NotionCache
// ---------------------------------------------------------------------------

pub struct NotionCache {
    config: Config,
    tree: RwLock<Arc<Tree>>,
    slug_map: RwLock<Arc<HashMap<String, String>>>,
    client: NotionClient,
    cache_dir: Option<PathBuf>,
}

impl NotionCache {
    pub fn new(config: Config, client: NotionClient, cache_dir: Option<PathBuf>) -> Self {
        if let Some(ref dir) = cache_dir {
            let _ = fs::create_dir_all(dir);
        }
        Self {
            config,
            tree: RwLock::new(Arc::new(HashMap::new())),
            slug_map: RwLock::new(Arc::new(HashMap::new())),
            client,
            cache_dir,
        }
    }

    /// Load cached tickets from disk JSON files. Returns total ticket count.
    pub fn load_from_disk(&self) -> usize {
        let mut new_tree: Tree = HashMap::new();
        let mut new_slug_map: HashMap<String, String> = HashMap::new();
        let mut total = 0usize;

        let cache_dir = match self.cache_dir {
            Some(ref d) => d,
            None => return 0,
        };

        for (proj_name, _proj_cfg) in &self.config.projects {
            let proj_slug = slugify(proj_name);
            let path = cache_dir.join(format!("{proj_slug}.json"));

            let data = match fs::read_to_string(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };

            let tickets: Vec<Ticket> = match serde_json::from_str(&data) {
                Ok(t) => t,
                Err(_) => continue,
            };

            new_slug_map.insert(proj_slug.clone(), proj_name.clone());
            let proj_tree = Self::build_project_tree(&tickets, &mut new_slug_map);
            total += tickets.len();
            new_tree.insert(proj_slug, proj_tree);
        }

        // Swap under write lock
        *self.tree.write().unwrap() = Arc::new(new_tree);
        *self.slug_map.write().unwrap() = Arc::new(new_slug_map);

        total
    }

    /// Query Notion API and rebuild the tree. If `project` is given, only refresh
    /// that project (by display name); otherwise refresh all configured projects.
    /// Returns total ticket count across all projects in the tree.
    pub fn refresh(&self, project: Option<&str>) -> usize {
        // Determine which projects to refresh
        let projects_to_refresh: Vec<(String, ProjectConfig)> = match project {
            Some(name) => self
                .config
                .projects
                .iter()
                .filter(|(k, _)| k.as_str() == name)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            None => self
                .config
                .projects
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };

        // Build new data outside the lock
        let mut refreshed_trees: HashMap<String, (String, HashMap<String, HashMap<String, Vec<Ticket>>>, Vec<Ticket>)> =
            HashMap::new();
        let mut refreshed_slug_map: HashMap<String, String> = HashMap::new();

        for (proj_name, proj_cfg) in &projects_to_refresh {
            let pages = match self.client.query_database(&proj_cfg.database_id) {
                Ok(p) => p,
                Err(_) => continue,
            };

            let tickets: Vec<Ticket> = pages.iter().map(Ticket::from_page).collect();
            let proj_slug = slugify(proj_name);
            refreshed_slug_map.insert(proj_slug.clone(), proj_name.clone());
            let proj_tree = Self::build_project_tree(&tickets, &mut refreshed_slug_map);
            refreshed_trees.insert(proj_slug, (proj_name.clone(), proj_tree, tickets));
        }

        // Swap under write lock
        let total;
        {
            let old_tree = self.tree.read().unwrap().clone();
            let old_sm = self.slug_map.read().unwrap().clone();

            let mut new_tree: Tree;
            let mut new_sm: HashMap<String, String>;

            if project.is_some() {
                // Merge into existing tree
                new_tree = (*old_tree).clone();
                new_sm = (*old_sm).clone();
                for (proj_slug, (_proj_name, proj_tree, _tickets)) in &refreshed_trees {
                    new_tree.insert(proj_slug.clone(), proj_tree.clone());
                }
                new_sm.extend(refreshed_slug_map);
            } else {
                new_tree = refreshed_trees
                    .iter()
                    .map(|(slug, (_, t, _))| (slug.clone(), t.clone()))
                    .collect();
                new_sm = refreshed_slug_map;
            }

            total = new_tree
                .values()
                .flat_map(|a| a.values().flat_map(|s| s.values().map(|v| v.len())))
                .sum();

            *self.tree.write().unwrap() = Arc::new(new_tree);
            *self.slug_map.write().unwrap() = Arc::new(new_sm);
        }

        // Save to disk outside the lock
        for (proj_slug, (_proj_name, _proj_tree, tickets)) in &refreshed_trees {
            self.save_tickets_to_disk(proj_slug, tickets);
        }

        total
    }

    /// Fetch the markdown description for a single page via the Notion API.
    pub fn fetch_description(&self, page_id: &str) -> Result<String, reqwest::Error> {
        self.client.get_page_blocks(page_id)
    }

    /// Persist all tickets for a project slug to `{cache_dir}/{proj_slug}.json`.
    pub fn save_project_cache(&self, proj_slug: &str) {
        let tree = self.get_tree();
        let Some(proj_tree) = tree.get(proj_slug) else {
            return;
        };

        let tickets: Vec<&Ticket> = proj_tree
            .values()
            .flat_map(|statuses| statuses.values().flat_map(|v| v.iter()))
            .collect();

        if let Some(ref dir) = self.cache_dir {
            if let Ok(data) = serde_json::to_string_pretty(&tickets) {
                let path = dir.join(format!("{proj_slug}.json"));
                let _ = fs::write(path, data);
            }
        }
    }

    /// Return an Arc reference to the current tree (cheap — no deep clone).
    pub fn get_tree(&self) -> Arc<Tree> {
        self.tree.read().unwrap().clone()
    }

    /// Return an Arc reference to the current slug map (cheap — no deep clone).
    pub fn get_slug_map(&self) -> Arc<HashMap<String, String>> {
        self.slug_map.read().unwrap().clone()
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Build the assignee -> status -> tickets sub-tree for one project.
    /// Also populates `slug_map` with assignee and status slug -> display name.
    fn build_project_tree(
        tickets: &[Ticket],
        slug_map: &mut HashMap<String, String>,
    ) -> HashMap<String, HashMap<String, Vec<Ticket>>> {
        let mut proj_tree: HashMap<String, HashMap<String, Vec<Ticket>>> = HashMap::new();

        for ticket in tickets {
            let assignee_display = resolve_assignee(ticket);
            let assignee_slug = slugify(&assignee_display);
            let status_display = if ticket.status.is_empty() {
                "no-status".to_string()
            } else {
                ticket.status.clone()
            };
            let status_slug = slugify(&status_display);

            slug_map.insert(assignee_slug.clone(), assignee_display);
            slug_map.insert(status_slug.clone(), status_display);

            proj_tree
                .entry(assignee_slug)
                .or_default()
                .entry(status_slug)
                .or_default()
                .push(ticket.clone());
        }

        proj_tree
    }

    fn save_tickets_to_disk(&self, proj_slug: &str, tickets: &[Ticket]) {
        if let Some(ref dir) = self.cache_dir {
            let path = dir.join(format!("{proj_slug}.json"));
            if let Ok(data) = serde_json::to_string_pretty(tickets) {
                let _ = fs::write(path, data);
            }
        }
    }
}

/// Return the assignee display name, falling back to "unassigned".
fn resolve_assignee(ticket: &Ticket) -> String {
    if ticket.assignee.is_empty() {
        "unassigned".to_string()
    } else {
        ticket.assignee.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify_ascii() {
        assert_eq!(slugify("In Progress"), "in-progress");
    }

    #[test]
    fn test_slugify_diacritics() {
        assert_eq!(slugify("Nguyễn Văn"), "nguyen-van");
    }

    #[test]
    fn test_slugify_vietnamese_d() {
        assert_eq!(slugify("Đặng"), "dang");
    }

    #[test]
    fn test_slugify_special_chars() {
        assert_eq!(slugify("foo--bar  baz!"), "foo-bar-baz");
    }

    #[test]
    fn test_slugify_empty() {
        assert_eq!(slugify(""), "");
    }
}
