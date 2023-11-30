use std::{any::Any, fs, io, path::PathBuf, rc::Rc, result, str::FromStr};

use thiserror::Error;

#[derive(Clone)]
struct AssetHandle(Rc<dyn Any>);

#[derive(Debug, Error)]
pub enum Error {
	#[error("io: {0}")]
	Io(#[from] io::Error),
	#[error("only utf8 filenames supported")]
	UnsupportedFilename,
}
type Result<T, E = Error> = result::Result<T, E>;

pub trait AssetStore {
	fn store_file(&self, name: &str, path: PathBuf) -> Result<AssetHandle>;
	fn store_data(&self, name: &str, data: Vec<u8>) -> Result<AssetHandle>;

	fn local_path(&self, handle: AssetHandle) -> Result<String>;
}

#[derive(Clone)]
struct FileAssetStore {
	root: PathBuf,
}
impl AssetStore for FileAssetStore {
	fn store_file(&self, _name: &str, path: PathBuf) -> Result<AssetHandle> {
		Ok(AssetHandle(Rc::new(path)))
	}

	fn store_data(&self, name: &str, data: Vec<u8>) -> Result<AssetHandle> {
		let mut path = self.root.to_path_buf();
		path.push(name);
		fs::write(&path, &data)?;
		Ok(AssetHandle(Rc::new(path)))
	}

	fn local_path(&self, handle: AssetHandle) -> Result<String> {
		let path = handle
			.0
			.downcast_ref::<PathBuf>()
			.expect("file asset store only provided PathBufs");
		let s = path.to_str().ok_or(Error::UnsupportedFilename)?;
		Ok(s.to_owned())
	}
}

#[derive(Clone)]
pub enum AssetBackend {
	File(FileAssetStore),
}
impl FromStr for AssetBackend {
	type Err = &'static str;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		if let Some(file) = s.strip_prefix("file=") {
			return Ok(Self::File(FileAssetStore {
				root: PathBuf::from(file),
			}));
		}
		Err("unknown secret backend")
	}
}
impl AssetStore for AssetBackend {
	fn store_file(&self, name: &str, path: PathBuf) -> Result<AssetHandle> {
		match self {
			AssetBackend::File(f) => f.store_file(name, path),
		}
	}

	fn store_data(&self, name: &str, data: Vec<u8>) -> Result<AssetHandle> {
		match self {
			AssetBackend::File(f) => f.store_data(name, data),
		}
	}

	fn local_path(&self, handle: AssetHandle) -> Result<String> {
		match self {
			AssetBackend::File(f) => f.local_path(handle),
		}
	}
}
