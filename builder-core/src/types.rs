use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStack {
    Unspecified,
    Dockerfile,
    Buildpacks,
    Railpack,
    Static,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRequest {
    pub repo_url: String,
    pub git_ref: String,
    pub stack: BuildStack,
    pub target_image: String,
    #[serde(default)]
    pub cache_key: String,
    #[serde(default)]
    pub static_cfg: Option<StaticConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StaticConfig {
    #[serde(default)]
    pub output_dir: String,
    #[serde(default)]
    pub base_image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    pub digest: String,
    pub registry_ref: String,
    pub duration_ms: u64,
    pub stack_used: BuildStack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildError {
    pub code: u32,
    pub message: String,
    #[serde(default)]
    pub stage: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub stage: String,
    pub log: String,
    pub percent: u32,
}
