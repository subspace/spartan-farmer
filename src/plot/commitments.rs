use crate::{utils, Salt};
use async_std::io;
use async_std::path::PathBuf;
use rocksdb::{DBWithThreadMode, SingleThreaded, DB};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub(super) enum DbError {
    #[error("RocksDB database opening error: {0}")]
    RocksDb(rocksdb::Error),
    #[error("Metadata file error: {0}")]
    Metadata(io::Error),
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
enum CommitmentStatus {
    /// In-progress commitment to the part of the plot
    InProgress,
    /// Commitment to the whole plot and not some in-progress partial commitment
    Created,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Metadata {
    pub(super) commitments: HashMap<Salt, CommitmentStatus>,
}

#[derive(Debug)]
pub(super) struct Commitments {
    path: PathBuf,
    databases: HashMap<Salt, Arc<DBWithThreadMode<SingleThreaded>>>,
    metadata: Metadata,
}

impl Commitments {
    pub(super) async fn new(path: PathBuf) -> io::Result<Self> {
        let mut metadata: Metadata = async_std::fs::read_to_string(path.join("metadata.json"))
            .await
            .ok()
            .and_then(|metadata| serde_json::from_str(&metadata).ok())
            .unwrap_or_default();

        // Remove unfinished commitments from the previous run
        for (salt, _status) in metadata
            .commitments
            .drain_filter(|_salt, status| *status != CommitmentStatus::Created)
        {
            async_std::fs::remove_dir_all(path.join(hex::encode(salt))).await?;
        }

        Ok(Self {
            path,
            databases: HashMap::new(),
            metadata,
        })
    }

    /// Get existing database or create an empty one with [`CommitmentStatus::InProgress`] status
    pub(super) async fn get_or_create_db(
        &mut self,
        salt: Salt,
    ) -> Result<Arc<DBWithThreadMode<SingleThreaded>>, DbError> {
        match self.databases.entry(salt) {
            Entry::Occupied(entry) => Ok(Arc::clone(entry.get())),
            Entry::Vacant(entry) => {
                let db_path = self.path.join(hex::encode(salt));
                let db = Arc::new(
                    utils::spawn_blocking(move || DB::open_default(db_path))
                        .await
                        .map_err(DbError::RocksDb)?,
                );

                entry.insert(Arc::clone(&db));
                self.metadata
                    .commitments
                    .insert(salt, CommitmentStatus::InProgress);
                async_std::fs::write(
                    self.path.join("metadata.json"),
                    serde_json::to_string(&self.metadata).unwrap(),
                )
                .await
                .map_err(DbError::Metadata)?;

                Ok(db)
            }
        }
    }

    /// Transition database associated with `salt` to created status, meaning that it represents the
    /// whole plot and not some in-progress partial commitment
    pub(super) async fn finish_commitment_creation(&mut self, salt: Salt) -> io::Result<()> {
        self.metadata
            .commitments
            .insert(salt, CommitmentStatus::Created);
        async_std::fs::write(
            self.path.join("metadata.json"),
            serde_json::to_string(&self.metadata).unwrap(),
        )
        .await
    }

    /// Removes commitment from disk
    pub(super) async fn remove_commitment(&mut self, salt: Salt) -> io::Result<()> {
        self.metadata.commitments.remove(&salt);
        if let Some(database) = self.databases.remove(&salt) {
            utils::spawn_blocking(move || {
                let path = database.path().to_path_buf();
                drop(database);
                std::fs::remove_dir_all(path)
            })
            .await?;
        }

        Ok(())
    }
}