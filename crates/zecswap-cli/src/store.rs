use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand_core::{OsRng, RngCore};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Test wallet files. The seed is stored in the clear: testnet use only.
pub(crate) struct Store {
    dir: PathBuf,
}

impl Store {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
        }
    }

    pub(crate) fn create_seed(&self) -> Result<Vec<u8>> {
        let path = self.dir.join("seed");
        if path.exists() {
            bail!("{} already exists", path.display());
        }
        let mut seed = vec![0; 32];
        OsRng.fill_bytes(&mut seed);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?
            .write_all(hex::encode(&seed).as_bytes())?;
        Ok(seed)
    }

    pub(crate) fn seed(&self) -> Result<Vec<u8>> {
        let hex = fs::read_to_string(self.dir.join("seed")).context("no wallet yet; run `init`")?;
        Ok(hex::decode(hex.trim())?)
    }

    pub(crate) fn save<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let tmp = self.dir.join(format!("{name}.tmp"));
        fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
        Ok(fs::rename(tmp, self.dir.join(name))?)
    }

    pub(crate) fn load<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        match fs::read(self.dir.join(name)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
