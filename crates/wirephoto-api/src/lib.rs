//! Request and response types of the wirephoto HTTP API (docs/api.md), shared by the server and clients.
//!
//! Fields that do not apply are omitted from the JSON (`Option` + `skip_serializing_if`), as the API's
//! conventions require.

use serde::{Deserialize, Serialize};

/// `GET /v1`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub previews: PreviewInfo,
    pub limits: Limits,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<KeyInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewInfo {
    pub sizes: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub page_size: u32,
    pub batch: u32,
    pub previews: u32,
    /// Bytes per chunk of an upload session (`PUT /v1/uploads/{id}`), the last one may be shorter.
    pub upload_chunk: u64,
    /// Chunks worth sending at once.
    pub upload_parallel: u32,
}

/// The key a request was made with.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyInfo {
    pub name: String,
    pub scope: Scope,
    /// Omitted when the key may use every library.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libraries: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Read,
    Write,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub id: String,
    pub name: String,
    pub root: String,
    pub read_only: bool,
    /// Days a deleted entry stays in the trash before the server removes it; 0: kept until removed by hand on
    /// the server. Deletes through the API are never permanent.
    pub trash_days: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexing: Option<Indexing>,
    pub watch: Watch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Indexing {
    pub state: IndexingState,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexingState {
    Scanning,
    Reading,
    Hashing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Watch {
    Live,
    Polling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Folder,
    Image,
    Raw,
    Video,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub library: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub kind: Kind,
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub modified: String,
    pub etag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Media>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trashed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub urls: Option<Urls>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<Counts>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Media {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orientation: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Urls {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub folders: u64,
    pub files: u64,
}

/// A page of a list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// `POST /v1/uploads`: a file to upload in chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateUpload {
    /// The folder it goes into.
    pub parent: String,
    pub name: String,
    pub size: u64,
    /// Lowercase hex, when known: lets the server answer at once when the folder already holds these bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// An upload session (`POST /v1/uploads`, `GET /v1/uploads/{id}`). `received` lists the byte ranges stored so
/// far, `[start, end)`, merged and in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Upload {
    pub id: String,
    pub size: u64,
    pub chunk: u64,
    pub received: Vec<[u64; 2]>,
    pub expires: String,
}

/// `PUT /v1/uploads/{id}`: what is stored after the chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Received {
    pub received: Vec<[u64; 2]>,
}

/// `POST /v1/entries`: a folder (`kind: "folder"`) or a copy (`copyOf`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateEntry {
    pub parent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<String>,
}

/// `POST /v1/previews`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewsRequest {
    pub ids: Vec<String>,
    pub size: u32,
}

/// `POST /v1/batch`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BatchRequest {
    pub requests: Vec<SubRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubRequest {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchResponse {
    pub responses: Vec<SubResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubResponse {
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeType {
    Created,
    Updated,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: ChangeType,
    pub entry: serde_json::Value,
}

/// `GET /v1/changes`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    pub changes: Vec<Change>,
    pub next: u64,
}

/// RFC 9457 problem details, with the stable `code` apps act on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Problem {
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
