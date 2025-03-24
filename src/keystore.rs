use std::{
	env,
	fs::{self, create_dir_all, Permissions},
	io::{self, ErrorKind, Write},
	os::unix::fs::PermissionsExt,
	path::PathBuf,
	result,
	str::FromStr,
};

use chainql_core::address::{address_seed, public_bytes_seed, SignatureSchema};
use libp2p::identity::{ed25519, PeerId};
use sp_core::crypto::{SecretStringError, Ss58AddressFormat};
use tempfile::{NamedTempFile, PersistError};
use tracing::info;

use crate::fs_utils::create_dir_mode;

#[derive(thiserror::Error, Debug)]
pub enum Error {
	#[error("io: {0}")]
	Io(#[from] io::Error),
	#[error("persist: {0}")]
	Persist(#[from] PersistError),
	#[error("decoding libp2p identity: {0}")]
	IdentityDecoding(#[from] libp2p::identity::DecodingError),
	#[error("keystore ty should be 4 chars")]
	InvalidKeystoreTy,
	#[error("secret string: {0}")]
	SecretString(#[from] SecretStringError),
	#[error("only utf-8 filenames supported")]
	UnsupportedFileName,
	#[error("unsupported keystore entry")]
	UnsupportedKeystoreEntry,
	#[error("json: {0}")]
	Json(#[from] serde_json::Error),
	#[error("duplicate key by type: {0}")]
	DuplicateKeyByType(String),
	#[error("invalid parameter: {0}")]
	InvalidParameter(&'static str),
}
type Result<T, E = Error> = result::Result<T, E>;

impl From<Error> for jrsonnet_evaluator::Error {
	fn from(value: Error) -> Self {
		jrsonnet_evaluator::Error::new(jrsonnet_evaluator::RuntimeError(
			format!("keystore: {value}").into(),
		))
	}
}

pub trait SecretStorage {
	fn store_node_key(&self, name: &str, keypair: ed25519::Keypair) -> Result<()>;
	fn get_node_id(&self, name: &str) -> Result<Option<String>>;

	fn store_typed_key(
		&self,
		node: &str,
		ty: &str,
		schema: SignatureSchema,
		suri: &str,
		format: Ss58AddressFormat,
	) -> Result<()>;
	fn get_typed(
		&self,
		node: &str,
		ty: &str,
		schema: SignatureSchema,
		format: Ss58AddressFormat,
	) -> Result<Option<String>>;

	fn store_wallet(
		&self,
		name: &str,
		ty: &str,
		schema: SignatureSchema,
		suri: &str,
		format: Ss58AddressFormat,
	) -> Result<()>;
	fn get_wallet(
		&self,
		node: &str,
		ty: &str,
		schema: SignatureSchema,
		format: Ss58AddressFormat,
	) -> Result<Option<String>>;

	/// If keystore is stored on disk as a directory, return the path to it
	/// If the keystore for node is empty, should return path to the entry directory instead
	/// (I.e /var/empty)
	fn local_keystore_dir(&self, node: &str) -> Result<Option<String>>;
	fn local_node_file(&self, node: &str) -> Result<Option<String>>;
}

#[derive(Clone)]
pub struct FileNodeKeys {
	pub root: PathBuf,
}
impl FileNodeKeys {
	fn node_keys_dir(&self) -> Result<Option<PathBuf>> {
		let mut path = self.root.to_path_buf();
		path.push("node");
		if !path.is_dir() {
			return Ok(None);
		}
		Ok(Some(path))
	}
	fn node_keys_dir_create(&self) -> Result<PathBuf> {
		let mut path = self.root.to_path_buf();
		path.push("node");
		fs::create_dir_all(&path)?;
		Ok(path)
	}
	fn keystore_dir(&self, node: &str) -> Result<Option<PathBuf>> {
		let mut path = self.root.to_path_buf();
		path.push(format!("keystore/{node}"));
		if !path.is_dir() {
			return Ok(None);
		}
		Ok(Some(path))
	}

	fn keystore_dir_create(&self, node: &str) -> Result<PathBuf> {
		let keystore_path = self.root.join("keystore");
		create_dir_all(&keystore_path)?;

		let keystore_node_path = keystore_path.join(node);
		create_dir_mode(&keystore_node_path, 0o755)?;

		Ok(keystore_node_path)
	}
	fn wallet_dir(&self) -> Result<Option<PathBuf>> {
		let mut path = self.root.to_path_buf();
		path.push("wallet");
		if !path.is_dir() {
			return Ok(None);
		}
		Ok(Some(path))
	}
	fn wallet_dir_create(&self) -> Result<PathBuf> {
		let mut path = self.root.to_path_buf();
		path.push("wallet");
		fs::create_dir_all(&path)?;
		Ok(path)
	}
}

impl SecretStorage for FileNodeKeys {
	fn store_node_key(&self, name: &str, keypair: ed25519::Keypair) -> Result<()> {
		let mut path = self.node_keys_dir_create()?;
		path.push(name);

		let mut temp = NamedTempFile::new_in(&self.root)?;
		temp.write_all(keypair.secret().as_ref())?;
		temp.as_file_mut()
			.set_permissions(Permissions::from_mode(0o644))?;
		temp.persist(path)?;

		Ok(())
	}

	fn get_node_id(&self, name: &str) -> Result<Option<String>> {
		// FIXME: file store should protect secret file, and store public key in other location
		let Some(mut path) = self.node_keys_dir()? else {
			return Ok(None);
		};
		path.push(name);

		let data = match fs::read(&path) {
			Ok(v) => v,
			Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
			Err(e) => return Err(e.into()),
		};

		let secret = ed25519::SecretKey::try_from_bytes(data)?;
		let pair = ed25519::Keypair::from(secret);

		let base58 = PeerId::from_public_key(&pair.public().into()).to_base58();
		Ok(Some(base58))
	}

	fn store_typed_key(
		&self,
		node: &str,
		ty: &str,
		schema: SignatureSchema,
		suri: &str,
		_format: Ss58AddressFormat,
	) -> Result<()> {
		if ty.chars().count() != 4 {
			return Err(Error::InvalidKeystoreTy);
		}
		let dir = self.keystore_dir_create(node)?;

		let ty_hex = hex::encode(ty);
		let public_hex = hex::encode(public_bytes_seed(schema, suri)?);

		let name = format!("{ty_hex}{public_hex}");

		let mut secret = dir.to_owned();
		secret.push(&name);

		{
			let mut file = NamedTempFile::new_in(&dir)?;
			file.write_all(serde_json::to_string(&suri).unwrap().as_bytes())?;
			file.as_file_mut()
				.set_permissions(Permissions::from_mode(0o644))?;
			file.persist(&secret)?;
		}

		for entry in dir.read_dir()? {
			let entry = entry?;
			let file_name = entry.file_name();
			let file_name_str = file_name.to_str().ok_or(Error::UnsupportedFileName)?;
			if !entry.metadata()?.is_file() {
				return Err(Error::UnsupportedKeystoreEntry);
			}
			if file_name_str.starts_with(&ty_hex) && file_name_str != name {
				fs::remove_file(&entry.path())?;
			}
		}

		Ok(())
	}

	fn get_typed(
		&self,
		node: &str,
		ty: &str,
		schema: SignatureSchema,
		format: Ss58AddressFormat,
	) -> Result<Option<String>> {
		if ty.chars().count() != 4 {
			return Err(Error::InvalidKeystoreTy);
		}
		let Some(dir) = self.keystore_dir(node)? else {
			return Ok(None);
		};
		let ty_hex = hex::encode(ty);

		let mut found = None;
		for entry in dir.read_dir()? {
			let entry = entry?;
			let file_name = entry.file_name();
			let file_name_str = file_name.to_str().ok_or(Error::UnsupportedFileName)?;
			if !entry.metadata()?.is_file() {
				return Err(Error::UnsupportedKeystoreEntry);
			}
			if file_name_str.starts_with(&ty_hex) {
				let data = fs::read_to_string(&entry.path())?;
				let suri: String = serde_json::from_str(&data)?;
				if found.is_some() {
					return Err(Error::DuplicateKeyByType(ty.to_string()));
				}
				found = Some(suri);
			}
		}

		let Some(suri) = found else {
			return Ok(None);
		};
		let public = address_seed(schema, &suri, format)?;
		Ok(Some(public))
	}

	fn store_wallet(
		&self,
		name: &str,
		ty: &str,
		_schema: SignatureSchema,
		suri: &str,
		_format: Ss58AddressFormat,
	) -> Result<()> {
		let dir = self.wallet_dir_create()?;
		let mut secret = dir.clone();
		secret.push(format!("{name}-{ty}"));

		{
			let file = NamedTempFile::new_in(&dir)?;
			fs::write(&file, serde_json::to_string(&suri)?)?;
			file.persist(secret)?;
		}

		Ok(())
	}

	fn get_wallet(
		&self,
		node: &str,
		ty: &str,
		schema: SignatureSchema,
		format: Ss58AddressFormat,
	) -> Result<Option<String>> {
		let Some(dir) = self.wallet_dir()? else {
			return Ok(None);
		};
		let mut secret = dir;
		secret.push(format!("{node}-{ty}"));

		let data = match fs::read_to_string(&secret) {
			Ok(v) => v,
			Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
			Err(e) => return Err(e.into()),
		};
		let suri: String = serde_json::from_str(&data)?;

		let public = address_seed(schema, &suri, format)?;
		Ok(Some(public))
	}

	fn local_keystore_dir(&self, node: &str) -> Result<Option<String>> {
		if let Some(dir) = self.keystore_dir(node)? {
			let dir = dir.to_str().ok_or(Error::UnsupportedFileName)?;
			Ok(Some(dir.to_string()))
		} else {
			Ok(Some("/var/empty".to_owned()))
		}
	}

	fn local_node_file(&self, node: &str) -> Result<Option<String>> {
		let Some(mut file) = self.node_keys_dir()? else {
			return Ok(None);
		};
		file.push(node);
		Ok(Some(
			file.to_str().ok_or(Error::UnsupportedFileName)?.to_string(),
		))
	}
}

#[derive(Default, Clone)]
pub enum SecretBackend {
	File(FileNodeKeys),
	#[default]
	Unset,
}
impl FromStr for SecretBackend {
	type Err = &'static str;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		if let Some(file) = s.strip_prefix("file=") {
			Ok(Self::File(FileNodeKeys {
				root: {
					let mut cwd = env::current_dir().map_err(|_| "failed to get CWD")?;
					cwd.push(file);
					cwd
				},
			}))
		} else {
			Ok(SecretBackend::Unset)
		}
	}
}

impl SecretStorage for SecretBackend {
	fn store_node_key(&self, name: &str, keypair: ed25519::Keypair) -> Result<()> {
		let base58 = PeerId::from_public_key(&keypair.public().into()).to_base58();
		info!("🛂 new node identity {name} => {base58}");
		match self {
			SecretBackend::File(f) => f.store_node_key(name, keypair),
			SecretBackend::Unset => Err(Error::InvalidParameter("secret backend is not set")),
		}
	}

	fn get_node_id(&self, name: &str) -> Result<Option<String>> {
		match self {
			SecretBackend::File(f) => f.get_node_id(name),
			SecretBackend::Unset => Err(Error::InvalidParameter("secret backend is not set")),
		}
	}

	fn store_typed_key(
		&self,
		node: &str,
		ty: &str,
		schema: SignatureSchema,
		suri: &str,
		format: Ss58AddressFormat,
	) -> Result<()> {
		info!("🔑 new node key {node} ({ty}) => {}", {
			address_seed(schema, suri, format)?
		});
		match self {
			SecretBackend::File(f) => f.store_typed_key(node, ty, schema, suri, format),
			SecretBackend::Unset => Err(Error::InvalidParameter("secret backend is not set")),
		}
	}

	fn get_typed(
		&self,
		node: &str,
		ty: &str,
		schema: SignatureSchema,
		format: Ss58AddressFormat,
	) -> Result<Option<String>> {
		match self {
			SecretBackend::File(f) => f.get_typed(node, ty, schema, format),
			SecretBackend::Unset => Err(Error::InvalidParameter("secret backend is not set")),
		}
	}

	fn store_wallet(
		&self,
		name: &str,
		ty: &str,
		schema: SignatureSchema,
		suri: &str,
		format: Ss58AddressFormat,
	) -> Result<()> {
		// todo!()
		info!(" new node wallet {name} ({ty}) => {}", {
			address_seed(schema, suri, format)?
		});
		match self {
			SecretBackend::File(f) => f.store_wallet(name, ty, schema, suri, format),
			SecretBackend::Unset => Err(Error::InvalidParameter("secret backend is not set")),
		}
	}

	fn get_wallet(
		&self,
		node: &str,
		ty: &str,
		schema: SignatureSchema,
		format: Ss58AddressFormat,
	) -> Result<Option<String>> {
		match self {
			SecretBackend::File(f) => f.get_wallet(node, ty, schema, format),
			SecretBackend::Unset => Err(Error::InvalidParameter("secret backend is not set")),
		}
	}

	fn local_keystore_dir(&self, node: &str) -> Result<Option<String>> {
		match self {
			SecretBackend::File(f) => f.local_keystore_dir(node),
			SecretBackend::Unset => Err(Error::InvalidParameter("secret backend is not set")),
		}
	}

	fn local_node_file(&self, node: &str) -> Result<Option<String>> {
		match self {
			SecretBackend::File(f) => f.local_node_file(node),
			SecretBackend::Unset => Err(Error::InvalidParameter("secret backend is not set")),
		}
	}
}
