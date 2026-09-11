//! Blob 聚合根。与 Python `domain/blob/blob.py` 语义对齐。

use chrono::{DateTime, Utc};

/// Blob 生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobStatus {
    Pending,
    Ready,
    Error,
}

impl BlobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlobStatus::Pending => "pending",
            BlobStatus::Ready => "ready",
            BlobStatus::Error => "error",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "ready" => BlobStatus::Ready,
            "error" => BlobStatus::Error,
            _ => BlobStatus::Pending,
        }
    }
}

/// 内容寻址的 blob（对应 Python `Blob`）。
#[derive(Debug, Clone)]
pub struct Blob {
    pub blob_name: String,
    pub path: String,
    pub status: BlobStatus,
    pub chunks: Vec<crate::chunk::ChunkRef>,
    pub content_size: u64,
    pub language: Option<String>,
    pub file_type: String,
    pub retry_count: u32,
    pub error_message: Option<String>,
    pub last_seen: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl Blob {
    pub fn new(
        blob_name: impl Into<String>,
        path: impl Into<String>,
        status: BlobStatus,
        content_size: u64,
        language: Option<String>,
        file_type: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            blob_name: blob_name.into(),
            path: path.into(),
            status,
            chunks: Vec::new(),
            content_size,
            language,
            file_type: file_type.into(),
            retry_count: 0,
            error_message: None,
            last_seen: now,
            created_at: now,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.status == BlobStatus::Ready
    }

    /// 刷新 last_seen（ingest 重复上传时的 touch 语义）。
    pub fn touch(&mut self) {
        self.last_seen = Utc::now();
    }

    pub fn mark_ready(&mut self) {
        self.status = BlobStatus::Ready;
        self.error_message = None;
    }

    pub fn mark_error(&mut self, message: impl Into<String>) {
        self.status = BlobStatus::Error;
        self.error_message = Some(message.into());
    }
}
