use std::{fs::DirBuilder, io, os::unix::fs::DirBuilderExt, path::Path};

/// Recursively create a directory and all of its parent components if they
/// are missing with given permissions.
///
/// # Errors
///
/// The same as from [`std::fs::create_dir_all`]
pub fn create_dir_all<P: AsRef<Path>>(path: P, mode: u32) -> io::Result<()> {
	DirBuilder::new()
		.recursive(true)
		.mode(mode)
		.create(path.as_ref())
}
