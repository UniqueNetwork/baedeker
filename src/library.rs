use std::any::Any;
use std::collections::BTreeMap;
use std::rc::Rc;

use bip39::{Language, Mnemonic};
use chainql_core::address::{SignatureSchema, Ss58Format};
use jrsonnet_evaluator::manifest::JsonFormat;
use jrsonnet_evaluator::typed::{Either3, Typed};
use jrsonnet_evaluator::{bail, runtime_error, Either, ObjValue};
use jrsonnet_evaluator::{
	error::Result,
	function::{builtin, FuncVal, TlaArg},
	gc::GcHashMap,
	parser::Source,
	Context, ContextBuilder, ContextInitializer, IStr, ObjValueBuilder, Pending, ResultExt, State,
	Thunk, Val,
};
use jrsonnet_gcmodule::Trace;
use libp2p::identity::ed25519;
use tracing::{debug, warn};

use crate::keystore::SecretStorage;
use crate::spec_builder::{docker_mounts, FileLocation, SpecBuilder, SpecSource};
use crate::{apply_tla_opt, spec_builder};

fn mix_inner(
	state: &State,
	mut val: Val,
	mixin: Val,
	glob_args: &GcHashMap<IStr, TlaArg>,
	final_val: Pending<Val>,
) -> Result<Val> {
	match &val {
		Val::Obj(_) => {}
		_ => bail!("mixin target should be object"),
	};
	match mixin {
		Val::Null => Ok(val),
		Val::Obj(obj) => {
			let val = val
				.as_obj()
				.ok_or_else(|| runtime_error!("previous value was not an object!"))?;
			Ok(Val::Obj(obj.extend_from(val)))
		}
		Val::Func(_) => {
			let mut args = GcHashMap::new();
			for (k, v) in glob_args.iter() {
				args.insert(k.clone(), v.clone());
			}
			args.insert("prev".into(), TlaArg::Val(val.clone()));
			args.insert("final".into(), TlaArg::Lazy(final_val.clone().into()));
			let value = apply_tla_opt(state.clone(), &args, mixin)?;
			match value {
				obj @ Val::Obj(_) => Ok(obj),
				mixin @ Val::Arr(_) => mix_inner(state, val, mixin, glob_args, final_val),
				_ => bail!("mixin function should either return object, or "),
			}
		}
		Val::Arr(arr) => {
			for (i, mixin) in arr.iter().enumerate() {
				let mixin = mixin.with_description(|| format!("<mixin arr {i}>"))?;
				val = mix_inner(state, val, mixin, glob_args, final_val.clone())?;
			}
			Ok(val)
		}
		_ => bail!("mixin should be null/object/function!"),
	}
}

#[builtin]
pub fn builtin_mixer(ctx: Context, mixin: Val) -> Result<FuncVal> {
	#[builtin(fields(
		mixin: Val,
		state: State,
	))]
	pub fn builtin_mix(this: &builtin_mix, prev: Val) -> Result<Val> {
		let final_val = Pending::new();
		let result = mix_inner(
			&this.state,
			prev,
			this.mixin.clone(),
			&GcHashMap::new(),
			final_val.clone(),
		)?;
		final_val.fill(result.clone());
		Ok(result)
	}
	Ok(FuncVal::builtin(builtin_mix {
		mixin,
		// FIXME: Propagate in evaluate_simple
		state: ctx.state().clone(),
	}))
}

#[builtin]
pub fn builtin_to_relative(from: String, to: String) -> Result<String> {
	let diff = pathdiff::diff_paths(to, from)
		.ok_or_else(|| runtime_error!("incorrect paths, both should be absolute"))?;
	let diff = diff.to_str().expect("inputs are utf-8");
	Ok(diff.to_string())
}

#[builtin]
pub fn builtin_docker_mounts() -> Result<Vec<String>> {
	warn!(
		"resulting spec will not work on the remote machine, impure bdk.dockerMounts() was used!"
	);
	Ok(docker_mounts()?)
}

#[builtin(fields(
	#[trace(skip)]
	builder: Rc<dyn SpecBuilder>,
))]
pub fn builtin_process_spec(
	this: &builtin_process_spec,
	bin: FileLocation,
	spec: SpecSource,
) -> Result<Val> {
	let builder = &*this.builder;
	match spec {
		SpecSource::Genesis(g) => {
			let spec = build_genesis(&bin, builder, g.chain, g.modify)?;
			build_raw(&bin, builder, g.spec_file_prefix, spec, g.modify_raw)
		}
		SpecSource::Raw(raw) => {
			if let Some(modify_raw) = &raw.modify_raw {
				modify_raw
					.evaluate_simple(&(raw.raw_spec,), true)
					.description("modify_raw callback")
			} else {
				Ok(raw.raw_spec)
			}
		}
		SpecSource::FromScratchGenesis(f) => {
			let spec = if let Some(modify) = &f.modify {
				modify
					.evaluate_simple(&(f.spec,), true)
					.description("modify callback")?
			} else {
				f.spec
			};

			build_raw(&bin, builder, f.spec_file_prefix, spec, f.modify_raw)
		}
	}
}

fn build_genesis(
	bin: &FileLocation,
	spec_builder: &dyn SpecBuilder,
	chain: Option<String>,
	modify: Option<FuncVal>,
) -> Result<Val> {
	debug!("building genesis");

	let v = spec_builder.build_genesis(bin, chain)?;
	let mut v: Val = serde_json::from_slice(&v).map_err(spec_builder::Error::from)?;

	if let Some(modify) = &modify {
		v = modify
			.evaluate_simple(&(v,), true)
			.description("modify callback")?;
	}

	Ok(v)
}

fn build_raw(
	bin: &FileLocation,
	spec_builder: &dyn SpecBuilder,
	spec_file_prefix: Option<String>,
	spec: Val,
	modify_raw: Option<FuncVal>,
) -> Result<Val> {
	debug!("building raw");

	let spec = spec.manifest(JsonFormat::cli(4, true))?;

	let v = spec_builder.build_raw(bin, spec_file_prefix, spec)?;
	let mut v: Val = serde_json::from_slice(&v).map_err(spec_builder::Error::from)?;

	if let Some(modify) = &modify_raw {
		v = modify
			.evaluate_simple(&(v,), true)
			.description("modify_raw callback")?;
	}

	Ok(v)
}

#[derive(Typed)]
pub struct AliasName {
	alias: String,
}

#[builtin(fields(
	#[trace(skip)]
	secrets: Rc<dyn SecretStorage>,
))]
pub fn builtin_ensure_keys(
	this: &builtin_ensure_keys,
	path: String,
	wanted_keys: BTreeMap<String, Either![SignatureSchema, AliasName, ObjValue]>,
	format: Option<Ss58Format>,
) -> Result<Val> {
	#[derive(Default, Typed)]
	struct Keys {
		#[typed(rename = "nodeIdentity")]
		node_identity: String,
		#[typed(add)]
		keys: BTreeMap<String, String>,
		#[typed(add)]
		wallets: BTreeMap<String, String>,
		#[typed(rename = "localKeystoreDir")]
		local_keystore_dir: String,
		#[typed(rename = "localNodeFile")]
		local_node_file: String,
	}

	let format = format.unwrap_or_default().0;
	let secrets = &this.secrets;

	let mut out = Keys::default();

	if secrets.get_node_id(&path)?.is_none() {
		let pair = ed25519::Keypair::generate();
		secrets.store_node_key(&path, pair)?;
	}
	out.node_identity = secrets.get_node_id(&path)?.expect("just inserted");

	for (name, scheme) in &wanted_keys {
		if let Some(ty) = name.strip_prefix('_') {
			let Either3::A(scheme) = scheme else {
				bail!("wallet scheme should be string-based: {name}");
			};
			if secrets.get_wallet(&path, ty, *scheme, format)?.is_none() {
				let suri = Mnemonic::generate_in(Language::English, 24)
					.unwrap()
					.to_string();
				secrets.store_wallet(&path, ty, *scheme, &suri, format)?;
			}
			out.wallets.insert(
				name[1..].to_string(),
				secrets
					.get_wallet(&path, ty, *scheme, format)?
					.expect("just inserted"),
			);
		} else if name.ends_with("Keys") && name.len() > 4
			|| name.ends_with("Key") && name.len() > 3
		{
			// Key set, i.e `sessionKeys`, pass.
		} else {
			if matches!(scheme, Either3::B(_)) {
				continue;
			};
			let Either3::A(scheme) = scheme else {
				bail!("secret scheme should be string-based: {name}");
			};
			if secrets.get_typed(&path, name, *scheme, format)?.is_none() {
				let suri = Mnemonic::generate_in(Language::English, 12)
					.unwrap()
					.to_string();
				secrets.store_typed_key(&path, name, *scheme, &suri, format)?;
				for (alias_name, alias) in &wanted_keys {
					let Either3::B(alias) = alias else {
						continue;
					};
					if &alias.alias != name {
						continue;
					};
					secrets.store_typed_key(&path, alias_name, *scheme, &suri, format)?;
				}
			}
			let stored = secrets
				.get_typed(&path, name, *scheme, format)?
				.expect("just inserted");
			out.keys.insert(name.clone(), stored.clone());
			for (alias_name, alias) in &wanted_keys {
				let Either3::B(alias) = alias else {
					continue;
				};
				if &alias.alias != name {
					continue;
				};
				out.keys.insert(alias_name.clone(), stored.clone());
			}
		}
	}
	// TODO: Remove the requirement
	out.local_keystore_dir = secrets
		.local_keystore_dir(&path)?
		.ok_or_else(|| runtime_error!("local keystore dir required"))?;
	out.local_node_file = secrets
		.local_node_file(&path)?
		.ok_or_else(|| runtime_error!("local node file required"))?;
	Keys::into_untyped(out)
}

#[derive(Trace)]
pub struct BdkContextInitializer {
	#[trace(skip)]
	pub spec_builder: Rc<dyn SpecBuilder>,
	#[trace(skip)]
	pub secrets: Rc<dyn SecretStorage>,
}

impl ContextInitializer for BdkContextInitializer {
	fn populate(&self, _for_file: Source, builder: &mut ContextBuilder) {
		let mut bdk = ObjValueBuilder::new();
		bdk.method("mixer", builtin_mixer::INST);
		bdk.method("toRelative", builtin_to_relative::INST);
		bdk.method("dockerMounts", builtin_docker_mounts::INST);
		bdk.method(
			"processSpec",
			builtin_process_spec {
				builder: self.spec_builder.clone(),
			},
		);
		bdk.method(
			"ensureKeys",
			builtin_ensure_keys {
				secrets: self.secrets.clone(),
			},
		);

		builder.bind("bdk", Thunk::evaluated(Val::Obj(bdk.build())));
	}

	fn as_any(&self) -> &dyn Any {
		self
	}
}
