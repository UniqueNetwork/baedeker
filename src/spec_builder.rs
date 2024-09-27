use std::{
	fs::{self, metadata, read_dir},
	io::Write,
	os::unix::fs::PermissionsExt,
	path::PathBuf,
	process::{Command, Stdio},
	result,
	str::FromStr,
};

use jrsonnet_evaluator::{
	bail,
	function::FuncVal,
	typed::{ComplexValType, Either2, Typed},
	Either, ObjValue, ObjValueBuilder, Val,
};
use jrsonnet_gcmodule::Trace;
use tempfile::Builder;
use tracing::info;

use crate::docker::EMPTY_IMAGE;

#[derive(thiserror::Error, Debug)]
pub enum Error {
	#[error("io: {0}")]
	Io(#[from] std::io::Error),
	#[error("docker finished with non-zero exit code; spec dumped to {0:?}\nCommand was: {1}")]
	DockerCommandFailed(PathBuf, String),
	#[error("json: {0}")]
	Json(#[from] serde_json::Error),
	#[error("binary is not set")]
	BinaryNotSet,
	#[error("invalid parameter: {0}")]
	InvalidParameter(&'static str),
}
type Result<T, E = Error> = result::Result<T, E>;

impl From<Error> for jrsonnet_evaluator::Error {
	fn from(value: Error) -> Self {
		jrsonnet_evaluator::Error::new(jrsonnet_evaluator::RuntimeError(
			format!("spec builder: {value}").into(),
		))
	}
}

pub fn docker_mounts() -> Result<Vec<String>> {
	let mut out = Vec::new();
	for entry in read_dir("/")? {
		let Ok(entry) = entry else {
			continue;
		};
		let Ok(metadata) = metadata(&entry.path()) else {
			continue;
		};
		if !metadata.is_dir() {
			continue;
		}
		let file_name = entry.file_name();
		let Some(name) = file_name.to_str() else {
			continue;
		};
		if name == "proc"
			|| name == "sys"
			|| name == "dev"
			|| name == "tmp"
			|| name == "run"
			|| name == "var"
		{
			continue;
		}
		out.push(name.to_owned());
	}
	Ok(out)
}

pub trait SpecBuilder {
	fn build_genesis(&self, bin: &FileLocation, chain: Option<String>) -> Result<Vec<u8>>;
	fn build_raw(
		&self,
		bin: &FileLocation,
		spec_file_prefix: Option<String>,
		spec: String,
	) -> Result<Vec<u8>>;
}

#[derive(Clone)]
pub struct DockerSpecBuilder;
impl DockerSpecBuilder {
	fn base_command(
		bin: &FileLocation,
		extra_docker: impl FnOnce(&mut Command),
	) -> Result<Command> {
		// FIXME: Needs a timeout in case if ENTRYPOINT is bad, and starts the chain when it should perform what we need
		// to, i.e build-spec. Unfortunately, it can't be done by docker itself: https://github.com/moby/moby/issues/1905
		//
		// Run command in a different thread (since we're use blocking APIs), and in 25 seconds force-stop the container?
		//
		// FIXME: Temporary solution was implemented using timeout command, it is not portable, but it will send SIGINT
		// in 25 seconds, and docker will cleanup the container itself due to --rm.
		let mut command = Command::new("timeout");
		command
			.args(["-s", "INT", "25"])
			.arg("docker")
			.arg("run")
			.arg("--rm")
			.args([
				"-e",
				// Wasm compilation logs are too noisy, github actions can't even handle them
				"RUST_LOG=debug,wasmtime_cranelift=info",
				"-e",
				"RUST_BACKTRACE=full",
				"-e",
				"COLORBT_SHOW_HIDDEN=1",
			]);
		if let Some(image) = &bin.docker_image {
			// Digest is known, nothing wrong will happen if we try to pull this image
			if image.contains('@') {
				command.args(["--pull", "missing"]);
			} else {
				command.args(["--pull", "never"]);
			}
			extra_docker(&mut command);
			if let Some(docker) = &bin.docker {
				command.args(["--entrypoint", docker.as_str()]);
			}
			command.arg(image);
		} else {
			// Digest is explicitly set
			command.args(["--pull", "missing"]);
			for mount in docker_mounts()? {
				command.arg("--mount").arg(format!(
					"type=bind,source=/{mount},target=/{mount},readonly"
				));
			}
			extra_docker(&mut command);
			command.arg(EMPTY_IMAGE);
			if let Some(local) = &bin.local {
				command.arg(local);
			} else {
				return Err(Error::BinaryNotSet);
			}
		}
		command.stdin(Stdio::null());
		command.stderr(Stdio::inherit());
		Ok(command)
	}
}
impl SpecBuilder for DockerSpecBuilder {
	fn build_genesis(&self, bin: &FileLocation, chain: Option<String>) -> Result<Vec<u8>> {
		let mut command = Self::base_command(bin, |_c| {})?;
		command.args(["build-spec", "--base-path", "/tmp/node"]);
		if let Some(chain) = chain {
			command.args(["--chain", &chain]);
		}
		let command_str = format!("{command:?}");
		let output = command.output()?;
		if !output.status.success() {
			return Err(Error::DockerCommandFailed(PathBuf::default(), command_str));
		}
		Ok(output.stdout)
	}

	fn build_raw(
		&self,
		bin: &FileLocation,
		spec_file_prefix: Option<String>,
		spec: String,
	) -> Result<Vec<u8>> {
		let mut tempfile = Builder::new();
		tempfile.permissions(fs::Permissions::from_mode(0o444));
		if let Some(prefix) = &spec_file_prefix {
			tempfile.prefix(prefix);
		}
		let mut spec_json = tempfile.tempfile()?;
		spec_json.write_all(spec.as_bytes())?;
		spec_json.flush()?;
		let spec_path = spec_json
			.path()
			.to_str()
			.expect("no reason for tempfile to be non-utf8");

		let mut command = Self::base_command(bin, |c| {
			c.arg("--mount").arg(format!(
				// FIXME: Moonbeam wants the spec json file to be named after runtime
				"type=bind,source={spec_path},target=/tmp/spec.json,readonly"
			));
		})?;
		command
			.args(["build-spec", "--raw", "--base-path", "/tmp/node"])
			.args(["--chain", "/tmp/spec.json"]);
		let command_str = format!("{command:?}");
		let output = command.output()?;
		if !output.status.success() {
			return Err(Error::DockerCommandFailed(
				{
					let path = spec_json.into_temp_path();
					let buf = path.to_path_buf();
					std::mem::forget(path);
					buf
				},
				command_str,
			));
		}
		Ok(output.stdout)
	}
}

#[derive(Typed, Trace, Clone)]
pub struct GenesisSpecSource {
	pub chain: Option<String>,
	pub modify: Option<FuncVal>,
	#[typed(rename = "specFilePrefix")]
	pub spec_file_prefix: Option<String>,
	#[typed(rename = "modifyRaw")]
	pub modify_raw: Option<FuncVal>,
}
#[derive(Typed, Trace, Clone)]
pub struct RawSpecSource {
	pub raw_spec: Val,
}
#[derive(Typed, Trace, Clone)]
pub struct FromScratchGenesisSpecSource {
	pub spec: Val,
	pub spec_file_prefix: Option<String>,
	#[typed(rename = "modifyRaw")]
	pub modify_raw: Option<FuncVal>,
}
#[derive(Trace, Clone)]
pub enum SpecSource {
	Genesis(GenesisSpecSource),
	Raw(RawSpecSource),
	FromScratchGenesis(FromScratchGenesisSpecSource),
}
const _: () = {
	use jrsonnet_evaluator::Result;
	impl Typed for SpecSource {
		const TYPE: &'static ComplexValType = &ComplexValType::Any;

		fn into_untyped(typed: Self) -> Result<Val> {
			let mut out = ObjValueBuilder::new();
			match typed {
				SpecSource::Genesis(g) => out
					.field("Genesis")
					.value(GenesisSpecSource::into_untyped(g)?),
				SpecSource::Raw(r) => out.field("Raw").value(RawSpecSource::into_untyped(r)?),
				SpecSource::FromScratchGenesis(g) => out
					.field("FromScratchGenesis")
					.value(FromScratchGenesisSpecSource::into_untyped(g)?),
			}
			Ok(Val::Obj(out.build()))
		}

		fn from_untyped(untyped: Val) -> Result<Self> {
			let obj = ObjValue::from_untyped(untyped)?;
			if obj.len() != 1 {
				bail!("not a enum");
			}
			let name = &obj.fields(false)[0];
			Ok(match name.as_str() {
				"Genesis" => Self::Genesis(GenesisSpecSource::from_untyped(
					obj.get("Genesis".into())?.unwrap(),
				)?),
				"Raw" => Self::Raw(RawSpecSource::from_untyped(
					obj.get("Raw".into())?.unwrap(),
				)?),
				"FromScratchGenesis" => {
					Self::FromScratchGenesis(FromScratchGenesisSpecSource::from_untyped(
						obj.get("FromScratchGenesis".into())?.unwrap(),
					)?)
				}
				v => bail!("unknown enum value: {:?}", v),
			})
		}
	}
};

#[derive(Clone, Trace)]
pub struct FileLocation {
	local: Option<String>,
	docker_image: Option<String>,
	docker: Option<String>,
}
const _: () = {
	use jrsonnet_evaluator::Result;
	#[derive(Typed)]
	struct FileLocationLocal {
		local: Option<String>,
		docker: Option<String>,
		#[typed(rename = "dockerImage")]
		docker_image: String,
	}
	type Eith = Either!(String, FileLocationLocal);
	impl Typed for FileLocation {
		const TYPE: &'static ComplexValType = Eith::TYPE;

		fn into_untyped(typed: Self) -> Result<Val> {
			match (typed.local, typed.docker, typed.docker_image) {
				(None, docker, Some(docker_image)) => {
					FileLocationLocal::into_untyped(FileLocationLocal {
						local: None,
						docker,
						docker_image,
					})
				}
				(Some(local), None, None) => Ok(Val::Str(local.into())),
				(Some(local), docker, Some(docker_image)) => {
					FileLocationLocal::into_untyped(FileLocationLocal {
						local: Some(local),
						docker,
						docker_image,
					})
				}
				_ => unreachable!("either docker or local location should be set"),
			}
		}

		fn from_untyped(untyped: Val) -> Result<Self> {
			Ok(match Eith::from_untyped(untyped)? {
				Either2::A(path) => FileLocation {
					local: Some(path),
					docker: None,
					docker_image: None,
				},
				Either2::B(found) => FileLocation {
					local: found.local,
					docker: found.docker,
					docker_image: Some(found.docker_image),
				},
			})
		}
	}
};

#[derive(Default, Clone)]
pub enum SpecBackend {
	Docker(DockerSpecBuilder),
	#[default]
	Unset,
}
impl FromStr for SpecBackend {
	type Err = &'static str;

	fn from_str(s: &str) -> result::Result<Self, Self::Err> {
		Ok(match s {
			"docker" => Self::Docker(DockerSpecBuilder),
			_ => Self::Unset,
		})
	}
}
impl SpecBuilder for SpecBackend {
	fn build_genesis(&self, bin: &FileLocation, chain: Option<String>) -> Result<Vec<u8>> {
		info!("building genesis, chain={chain:?}");
		match self {
			SpecBackend::Docker(d) => d.build_genesis(bin, chain),
			SpecBackend::Unset => Err(Error::InvalidParameter("spec backend is not set")),
		}
	}

	fn build_raw(
		&self,
		bin: &FileLocation,
		spec_file_prefix: Option<String>,
		spec: String,
	) -> Result<Vec<u8>> {
		info!("building raw");
		match self {
			SpecBackend::Docker(d) => d.build_raw(bin, spec_file_prefix, spec),
			SpecBackend::Unset => Err(Error::InvalidParameter("spec backend is not set")),
		}
	}
}
