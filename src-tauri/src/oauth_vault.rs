//! Shared encrypted credential storage for independent OAuth logins.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use fs2::FileExt;
use ring::{
    aead, digest,
    rand::{SecureRandom, SystemRandom},
};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "无法生成安全随机数")?;
    Ok(bytes)
}

fn storage_error(label: &str) -> String {
    format!("无法读写{label}加密凭证，请检查 usageBar 数据目录权限及文件完整性")
}

fn private_file(path: &Path, label: &str) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|_| storage_error(label))
}

fn atomic_write(dir: &Path, name: &str, data: &[u8], label: &str) -> Result<(), String> {
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|_| storage_error(label))?;
    file.write_all(data).map_err(|_| storage_error(label))?;
    file.as_file()
        .sync_all()
        .map_err(|_| storage_error(label))?;
    file.persist(dir.join(name))
        .map_err(|_| storage_error(label))?;
    Ok(())
}

/// AES-256-GCM vault with a per-directory key and an exclusive process lock.
pub struct Vault {
    pub dir: PathBuf,
    key: [u8; 32],
    label: &'static str,
    aad: &'static [u8],
    _lock: File,
}

impl Vault {
    pub fn open(
        dir: PathBuf,
        label: &'static str,
        aad: &'static [u8],
        lock_message: &'static str,
    ) -> Result<Self, String> {
        fs::create_dir_all(&dir).map_err(|_| storage_error(label))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                .map_err(|_| storage_error(label))?;
        }
        let lock = private_file(&dir.join("instance.lock"), label)?;
        lock.try_lock_exclusive()
            .map_err(|_| lock_message.to_string())?;
        let key_path = dir.join("vault.key");
        let key = match fs::read(&key_path) {
            Ok(bytes) => bytes.try_into().map_err(|_| storage_error(label))?,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && !dir.join("account.enc").exists() =>
            {
                let key = random()?;
                atomic_write(&dir, "vault.key", &key, label)?;
                key
            }
            Err(_) => return Err(storage_error(label)),
        };
        Ok(Self {
            dir,
            key,
            label,
            aad,
            _lock: lock,
        })
    }

    pub fn account_dir(&self, id: &str) -> PathBuf {
        // IDs can contain Unicode or path separators; never use them as paths.
        let hash = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, id.as_bytes()));
        self.dir.join("accounts").join(hash)
    }

    fn cipher(&self) -> Result<aead::LessSafeKey, String> {
        aead::UnboundKey::new(&aead::AES_256_GCM, &self.key)
            .map(aead::LessSafeKey::new)
            .map_err(|_| storage_error(self.label))
    }

    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>, String> {
        let bytes = match fs::read(self.dir.join("account.enc")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(storage_error(self.label)),
        };
        if bytes.len() < 28 {
            return Err(storage_error(self.label));
        }
        let nonce: [u8; 12] = bytes[..12]
            .try_into()
            .map_err(|_| storage_error(self.label))?;
        let mut body = bytes[12..].to_vec();
        let plain = self
            .cipher()?
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(self.aad),
                &mut body,
            )
            .map_err(|_| storage_error(self.label))?;
        serde_json::from_slice(plain)
            .map(Some)
            .map_err(|_| storage_error(self.label))
    }

    pub fn save<T: Serialize>(&self, value: &T) -> Result<(), String> {
        let nonce = random::<12>()?;
        let mut body = serde_json::to_vec(value).map_err(|_| storage_error(self.label))?;
        self.cipher()?
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(self.aad),
                &mut body,
            )
            .map_err(|_| storage_error(self.label))?;
        let mut bytes = nonce.to_vec();
        bytes.extend(body);
        atomic_write(&self.dir, "account.enc", &bytes, self.label)
    }

    pub fn clear(&self) -> Result<(), String> {
        match fs::remove_file(self.dir.join("account.enc")) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(storage_error(self.label)),
        }
    }
}
