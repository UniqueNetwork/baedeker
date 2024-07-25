use std::{
	env,
	fs::{create_dir_all, read_to_string, write},
	path::{Component, PathBuf},
	str::FromStr,
};

use clap::Parser;
use jrsonnet_cli::{MiscOpts, TlaOpts, TraceOpts};
use jrsonnet_evaluator::{
	bail,
	function::{CallLocation, TlaArg},
	gc::GcHashMap,
	manifest::JsonFormat,
	parser::{Source, SourcePath, SourceVirtual},
	runtime_error,
	trace::PathResolver,
	typed::{NativeFn, Typed},
	IStr, ObjValue, ObjValueBuilder, Pending, Result, ResultExt, State, Val,
};
use keystore::SecretBackend;
use spec_builder::SpecBackend;
use std::rc::Rc;
use tokio::runtime::Handle;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

use crate::docker::EMPTY_IMAGE;

// mod asset;
mod docker;
mod keystore;
mod library;
mod spec_builder;

#[derive(Clone)]
enum Generator {
	DockerCompose(PathBuf),
	DockerComposeDiscover(PathBuf),
	Debug,
	AddressBook,
}
impl Generator {
	fn value(self) -> Box<dyn GeneratorT> {
		match self {
			Generator::DockerCompose(output_dir) => Box::new(DockerCompose { output_dir }),
			Generator::DockerComposeDiscover(output_file) => {
				Box::new(DockerComposeDiscover { output_file })
			}
			Generator::Debug => Box::new(DebugGen),
			Generator::AddressBook => Box::new(AddressBook),
		}
	}
}

trait GeneratorT {
	/// List of paths to implicitly include when this generator is used
	/// Should not conflict with other generators
	fn library_modules(&self) -> Vec<String>;
	/// Under which attribute this generator should output data for itself/accept config
	fn output_attribute(&self) -> String;
	/// Supply config data to jsonnet
	fn config(&self) -> Result<Option<Val>>;
	/// Process output attribute data
	fn process(&self, data: Val) -> Result<()>;

	// /// Should not be used, standard library should be same regardless of which generators are in use.
	// fn extend_stdlib(&self, std: &mut ObjValueBuilder) -> Result<()>;
}

struct DockerCompose {
	output_dir: PathBuf,
}
impl GeneratorT for DockerCompose {
	fn library_modules(&self) -> Vec<String> {
		vec!["lib:baedeker-library/outputs/compose.libsonnet".to_string()]
	}

	fn output_attribute(&self) -> String {
		"dockerCompose".to_string()
	}

	fn config(&self) -> Result<Option<Val>> {
		#[derive(Typed)]
		struct Config {
			#[typed(rename = "emptyImage")]
			empty_image: String,
			#[typed(rename = "outputRoot")]
			output_root: String,
		}
		Config::into_untyped(Config {
			empty_image: EMPTY_IMAGE.to_string(),
			output_root: self
				.output_dir
				.to_str()
				.ok_or_else(|| runtime_error!("docker compose output is set to non-utf8 path"))?
				.to_string(),
		})
		.map(Some)
	}

	fn process(&self, data: Val) -> Result<()> {
		let output = ObjValue::from_untyped(data)?;
		let dir = &self.output_dir;

		for (name, value) in output.iter(false) {
			let mut path = dir.clone();
			path.push(name.as_str());
			if path.components().any(|c| c == Component::ParentDir) {
				bail!("generator output should not use parent dir");
			}
			if !path.starts_with(dir) {
				bail!("generator output should not escape the output directory: tried to write to {path:?}, which is outside of {dir:?}");
			}
			let value = IStr::from_untyped(value?)?;
			create_dir_all(path.parent().expect("not root")).expect("mkdirp");
			if path.exists() && output.has_field_ex(format!("reconcile_{name}").into(), true) {
				let data = read_to_string(&path).expect("read");
				let reconciler = output
					.get(format!("reconcile_{name}").into())?
					.expect("reconciler exists");
				let reconciler = <NativeFn<((String, IStr), IStr)>>::from_untyped(reconciler)
					.description("reconciler type")?;
				let reconciled = reconciler(data, value).description("reconciler call")?;
				write(&path, reconciled.as_bytes()).expect("write");
			} else {
				write(&path, value.as_bytes()).expect("write");
			}
		}
		Ok(())
	}
}

struct DockerComposeDiscover {
	output_file: PathBuf,
}
impl GeneratorT for DockerComposeDiscover {
	fn library_modules(&self) -> Vec<String> {
		vec!["lib:baedeker-library/outputs/composediscover.libsonnet".to_string()]
	}

	fn output_attribute(&self) -> String {
		"dockerComposeDiscover".to_string()
	}

	fn config(&self) -> Result<Option<Val>> {
		Ok(None)
	}

	fn process(&self, data: Val) -> Result<()> {
		let output = String::from_untyped(data)?;
		let parent = self
			.output_file
			.parent()
			.ok_or_else(|| runtime_error!("no parent"))?;
		create_dir_all(parent).map_err(|e| runtime_error!("mkdir failed: {e}"))?;
		write(&self.output_file, output.as_bytes())
			.map_err(|e| runtime_error!("write failed: {e}"))?;
		Ok(())
	}
}

struct AddressBook;
impl GeneratorT for AddressBook {
	fn library_modules(&self) -> Vec<String> {
		vec!["lib:baedeker-library/outputs/addressbook.libsonnet".to_string()]
	}

	fn output_attribute(&self) -> String {
		"addressbook".to_owned()
	}

	fn config(&self) -> Result<Option<Val>> {
		Ok(None)
	}

	fn process(&self, data: Val) -> Result<()> {
		let data = data.to_string()?;
		eprintln!("{data}");
		Ok(())
	}
}

struct DebugGen;
impl GeneratorT for DebugGen {
	fn library_modules(&self) -> Vec<String> {
		vec!["lib:baedeker-library/outputs/debug.libsonnet".to_string()]
	}

	fn output_attribute(&self) -> String {
		"debug".to_owned()
	}

	fn config(&self) -> Result<Option<Val>> {
		Ok(None)
	}

	fn process(&self, data: Val) -> Result<()> {
		let debug = data.manifest(JsonFormat::debug())?;
		eprintln!("{debug}");
		Ok(())
	}
}

impl FromStr for Generator {
	type Err = &'static str;

	fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
		if let Some(file) = s.strip_prefix("docker_compose=") {
			return Ok(Self::DockerCompose({
				let mut root = env::current_dir().map_err(|_| "bad cwd")?;
				root.push(file);
				root
			}));
		// } else if let Some(manifester) = s.strip_prefix("haya=") {
		// 	return Ok(Self::Kubernetes());
		} else if let Some(file) = s.strip_prefix("docker_compose_discover=") {
			return Ok(Self::DockerComposeDiscover({
				let mut root = env::current_dir().map_err(|_| "bad cwd")?;
				root.push(file);
				root
			}));
		} else if s == "addressbook" {
			return Ok(Self::AddressBook);
		} else if s == "debug" {
			return Ok(Self::Debug);
		}
		Err("unknown generator")
	}
}

#[derive(Parser)]
struct Opts {
	/// Where and how to store secrets.
	///
	/// Available values: kubernetes, file.
	#[arg(long)]
	secret: SecretBackend,
	/// How to build specs.
	///
	/// Available values: docker.
	#[arg(long)]
	spec: SpecBackend,
	/// Which type of output this generator should produce.
	///
	/// Available values: docker_compose, addressbook, debug.
	#[arg(long)]
	generator: Vec<Generator>,
	#[command(flatten)]
	import: MiscOpts,
	#[command(flatten)]
	trace: TraceOpts,
	#[command(flatten)]
	tla: TlaOpts,
	modules: Vec<String>,
	#[arg(long)]
	input_modules: Vec<String>,
}

pub fn apply_tla_opt(s: State, args: &GcHashMap<IStr, TlaArg>, val: Val) -> Result<Val> {
	let Some(func) = val.as_func() else {
		return Ok(val);
	};
	let params = func.params();
	if params.iter().any(|p| p.name().is_anonymous()) {
		bail!("only named params supported");
	}
	let mut new_args = GcHashMap::new();
	for (name, val) in args.iter() {
		if !params
			.iter()
			.any(|a| a.name().as_str() == Some(name.as_str()))
		{
			continue;
		}
		new_args.insert(name.clone(), val.clone());
	}
	State::push_description(
		|| "during TLA call".to_owned(),
		|| {
			func.evaluate(
				s.create_default_context(Source::new_virtual(
					"<top-level-arg>".into(),
					IStr::empty(),
				)),
				CallLocation::native(),
				&new_args,
				false,
			)
		},
	)
}

fn main_jrsonnet(opts: Opts) -> Result<()> {
	let state = State::default();
	state.set_import_resolver(opts.import.import_resolver());
	state.set_context_initializer((
		jrsonnet_stdlib::ContextInitializer::new(state.clone(), PathResolver::new_cwd_fallback()),
		chainql_core::CqlContextInitializer::default(),
		library::BdkContextInitializer {
			spec_builder: Rc::new(opts.spec),
			secrets: Rc::new(opts.secret),
		},
	));

	let generators = opts
		.generator
		.into_iter()
		.map(Generator::value)
		.collect::<Vec<_>>();

	let mut tla = opts.tla.tla_opts()?;
	if tla.contains_key("prev") || tla.contains_key("final") {
		bail!("TLA should not contain prev/final")
	}

	let config = {
		let final_config = <Pending<Val>>::new();

		info!("evaluating config");

		tla.insert("final".into(), TlaArg::Lazy(final_config.clone().into()));

		let mut modules = opts.modules.clone();
		modules.push("lib:baedeker-library/inputs/base.libsonnet".to_owned());

		let mut modules = modules.iter();

		let config = modules
			.next()
			.ok_or_else(|| runtime_error!("at least one module should be specified"))?;
		let config = state.import(config)?;
		let mut initial_modules = vec![];

		let config = if let Val::Arr(arr) = config {
			let mut iter = arr.iter();
			let config = iter
				.next()
				.ok_or_else(|| runtime_error!("empty array config"))??;
			for v in iter {
				let v = v.description("from config array")?;
				initial_modules.push(v);
			}
			config
		} else {
			config
		};

		let mut config = apply_tla_opt(state.clone(), &tla, config)?;

		for (i, module) in initial_modules.into_iter().enumerate() {
			tla.insert("prev".into(), TlaArg::Val(config));
			config = apply_tla_opt(state.clone(), &tla, module)
				.with_description(|| format!("<config array[{}]", i + 1))?;
		}

		for module in modules {
			debug!("module: {module:?}");
			let module = if let Some(module) = module.strip_prefix("lib:") {
				state
					.import_from(
						&SourcePath::new(SourceVirtual("module import".into())),
						module,
					)
					.description(module)?
			} else if let Some(code) = module.strip_prefix("snippet:") {
				state.evaluate_snippet("<snippet>", code)?
			} else {
				state.import(module)?
			};
			tla.insert("prev".into(), TlaArg::Val(config.clone()));
			config = apply_tla_opt(state.clone(), &tla, module)?;
		}

		// let config_mixin = generate_missing_keys(config.clone(), &opts.secret)?;
		// let config = config.as_obj().expect("checked to be obj");
		// let config = Val::Obj(config_mixin.extend_from(config));

		final_config.fill(config.clone());
		config
	};

	let config = {
		let mut libraries = opts.input_modules.clone();
		for generator in &generators {
			libraries.extend(generator.library_modules());
		}

		let final_config = <Pending<Val>>::new();

		info!("evaluating input config");
		let mut config = config;

		tla.insert("final".into(), TlaArg::Lazy(final_config.clone().into()));
		tla.insert("prev".into(), TlaArg::Val(config.clone()));

		for module in libraries {
			debug!("input module: {module:?}");
			let module = if let Some(module) = module.strip_prefix("lib:") {
				state
					.import_from(
						&SourcePath::new(SourceVirtual("module import".into())),
						module,
					)
					.description("input module (is baedeker-library updated?)")?
			} else {
				state.import(&module).description(&module)?
			};
			tla.insert("prev".into(), TlaArg::Val(config.clone()));
			config = apply_tla_opt(state.clone(), &tla, module)?;
		}

		for generator in &generators {
			let Some(generator_config) = generator.config()? else {
				continue;
			};
			let mut config_mixin = ObjValueBuilder::new();
			config_mixin
				.field("_config")
				.hide()
				.add()
				.value(generator_config);
			let mut output_mixin = ObjValueBuilder::new();
			output_mixin
				.field(generator.output_attribute())
				.add()
				.value(config_mixin.build());
			let mut mixin = ObjValueBuilder::new();
			mixin.field("_output").add().value(output_mixin.build());

			config = Val::Obj(
				mixin
					.build()
					.extend_from(config.as_obj().expect("checked obj")),
			);
		}

		final_config.fill(config.clone());
		config
	};

	let config = config.as_obj().expect("checked to be obj");
	let output = config.get("_output".into())?.ok_or_else(|| {
		runtime_error!("missing output key, have you imported any of the generators?")
	})?;
	let output = ObjValue::from_untyped(output)?;
	for generator in generators {
		let attr = generator.output_attribute();
		let data = output.get(attr.as_str().into())?.ok_or_else(|| {
			runtime_error!("missing generator output: {attr}, make sure your library is updated.")
		})?;
		generator.process(data)?;
	}

	Ok(())
}

fn main_sync() {
	tracing_subscriber::fmt()
		.without_time()
		.with_env_filter(EnvFilter::from_default_env())
		.init();

	let opts = Opts::parse();
	let trace_format = opts.trace.trace_format();

	match main_jrsonnet(opts) {
		Ok(_) => {}
		Err(e) => {
			let v = trace_format.format(&e).unwrap();
			error!("{v}");
			std::process::exit(1);
		}
	}
}

#[tokio::main]
async fn main() {
	Handle::current().spawn_blocking(main_sync).await.expect("baedeker should not panic, this is a bug, report to https://github.com/UniqueNetwork/baedeker/issues");
}
