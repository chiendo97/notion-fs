use serde::{Deserialize, Serialize};
use serde_json::Value;

const NOTION_API_URL: &str = "https://api.notion.com/v1";
const NOTION_VERSION: &str = "2022-06-28";

pub struct NotionClient {
    client: reqwest::blocking::Client,
}

impl NotionClient {
    pub fn new(token: String) -> Self {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token)).expect("invalid token"),
        );
        headers.insert(
            "Notion-Version",
            HeaderValue::from_static(NOTION_VERSION),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = reqwest::blocking::Client::builder()
            .default_headers(headers)
            .build()
            .expect("failed to build HTTP client");

        Self { client }
    }

    pub fn query_database(
        &self,
        database_id: &str,
    ) -> Result<Vec<Value>, reqwest::Error> {
        let url = format!("{}/databases/{}/query", NOTION_API_URL, database_id);
        let mut results: Vec<Value> = Vec::new();
        let mut start_cursor: Option<String> = None;

        loop {
            let mut body = serde_json::json!({});
            if let Some(cursor) = &start_cursor {
                body["start_cursor"] = Value::String(cursor.clone());
            }

            let resp = self
                .client
                .post(&url)
                .json(&body)
                .send()?
                .error_for_status()
                .map_err(|e| {
                    eprintln!("Notion API error querying database {}: {}", database_id, e);
                    e
                })?;

            let data: Value = resp.json()?;

            if let Some(page_results) = data["results"].as_array() {
                results.extend(page_results.iter().cloned());
            }

            let has_more = data["has_more"].as_bool().unwrap_or(false);
            if !has_more {
                break;
            }

            start_cursor = data["next_cursor"].as_str().map(|s| s.to_string());
        }

        Ok(results)
    }

    pub fn get_page_blocks(
        &self,
        page_id: &str,
    ) -> Result<String, reqwest::Error> {
        let mut blocks: Vec<String> = Vec::new();
        let mut start_cursor: Option<String> = None;

        loop {
            let mut url = format!(
                "{}/blocks/{}/children",
                NOTION_API_URL, page_id
            );
            if let Some(cursor) = &start_cursor {
                url.push_str(&format!("?start_cursor={}", cursor));
            }

            let resp = self
                .client
                .get(&url)
                .send()?
                .error_for_status()
                .map_err(|e| {
                    eprintln!("Notion API error fetching blocks for {}: {}", page_id, e);
                    e
                })?;

            let data: Value = resp.json()?;

            if let Some(results) = data["results"].as_array() {
                for block in results {
                    let block_type = block["type"].as_str().unwrap_or("");
                    if let Some(rich_texts) = block[block_type]["rich_text"].as_array() {
                        let text: String = rich_texts
                            .iter()
                            .filter_map(|rt| rt["plain_text"].as_str())
                            .collect::<Vec<&str>>()
                            .join("");
                        blocks.push(text);
                    }
                }
            }

            let has_more = data["has_more"].as_bool().unwrap_or(false);
            if !has_more {
                break;
            }

            start_cursor = data["next_cursor"].as_str().map(|s| s.to_string());
        }

        Ok(blocks.join("\n\n"))
    }
}

// ---------------------------------------------------------------------------
// Property reader helpers
// ---------------------------------------------------------------------------

fn read_title(props: &Value, key: &str) -> String {
    props[key]["title"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v["plain_text"].as_str())
        .unwrap_or("")
        .to_string()
}

fn read_status(props: &Value) -> String {
    let prop = &props["Status"];
    let type_name = prop["type"].as_str().unwrap_or("");
    prop[type_name]["name"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn read_select(props: &Value, key: &str) -> String {
    props[key]["select"]["name"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn read_people(props: &Value, key: &str) -> String {
    props[key]["people"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v["name"].as_str())
        .unwrap_or("")
        .to_string()
}

fn read_unique_id(props: &Value) -> String {
    // Find the first property with type "unique_id"
    if let Some(obj) = props.as_object() {
        for (_key, val) in obj {
            if val["type"].as_str() == Some("unique_id") {
                let prefix = val["unique_id"]["prefix"].as_str().unwrap_or("");
                let number = val["unique_id"]["number"].as_u64().unwrap_or(0);
                return format!("{}-{}", prefix, number);
            }
        }
    }
    String::new()
}

#[allow(dead_code)]
fn read_url(props: &Value, key: &str) -> String {
    props[key]["url"].as_str().unwrap_or("").to_string()
}

fn read_number(props: &Value, key: &str) -> Option<f64> {
    props[key]["number"].as_f64()
}

fn read_timestamp(props: &Value, key: &str) -> String {
    let prop = &props[key];
    let type_name = prop["type"].as_str().unwrap_or("");
    prop[type_name].as_str().unwrap_or("").to_string()
}

#[allow(dead_code)]
fn read_formula_date(props: &Value, key: &str) -> String {
    props[key]["formula"]["date"]["start"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// Ticket
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub ticket_id: String,
    pub name: String,
    pub status: String,
    pub priority: String,
    pub assignee: String,
    pub ah: Option<f64>,
    pub created: String,
    pub edited: String,
    pub url: String,
    pub page_id: String,
    pub description: String,
}

impl Default for Ticket {
    fn default() -> Self {
        Self {
            ticket_id: String::new(),
            name: String::new(),
            status: String::new(),
            priority: String::new(),
            assignee: String::new(),
            ah: None,
            created: String::new(),
            edited: String::new(),
            url: String::new(),
            page_id: String::new(),
            description: String::new(),
        }
    }
}

impl Ticket {
    pub fn from_page(page: &Value) -> Self {
        let props = &page["properties"];
        Self {
            ticket_id: read_unique_id(props),
            name: read_title(props, "Name"),
            status: read_status(props),
            priority: read_select(props, "Priority"),
            assignee: read_people(props, "Assignee"),
            ah: read_number(props, "AH"),
            created: read_timestamp(props, "Created time"),
            edited: read_timestamp(props, "Last edited time"),
            url: page["url"].as_str().unwrap_or("").to_string(),
            page_id: page["id"].as_str().unwrap_or("").to_string(),
            description: String::new(),
        }
    }

    pub fn render(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("ticket: {}\n", self.ticket_id));
        out.push_str(&format!("title: {}\n", self.name));
        out.push_str(&format!("status: {}\n", self.status));
        out.push_str(&format!("priority: {}\n", self.priority));
        out.push_str(&format!("assignee: {}\n", self.assignee));
        match self.ah {
            Some(v) => out.push_str(&format!("ah: {}\n", v)),
            None => out.push_str("ah:\n"),
        }
        out.push_str(&format!("created: {}\n", self.created));
        out.push_str(&format!("edited: {}\n", self.edited));
        out.push_str(&format!("url: {}\n", self.url));
        out.push_str("---\n");

        if !self.description.is_empty() {
            out.push('\n');
            out.push_str(&self.description);
            out.push('\n');
        }

        out.into_bytes()
    }
}
