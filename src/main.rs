use clap::Clap;
use ftp::FtpStream;
use ftp::types::FileType;
use std::path::{ Path, PathBuf };
use anyhow::{ Result, Context, ensure };
use ignore::gitignore::{ GitignoreBuilder, Gitignore };
use url::Url;
use serde::Deserialize;
use std::time::Duration;

mod tomfs;
pub mod mfs;
mod provider;
mod fromftp;
mod nabla;
#[cfg(test)]
mod globtest;

use crate::nabla::SyncActs;
use crate::fromftp::recursor::Recursor;
use crate::fromftp::provider::FtpProvider;
use crate::tomfs::ToMfs;
	
#[derive(Clap, Debug)]
struct Opts {
	/// FTP username override
	#[clap(short, long)]
	user: Option<String>,
	/// FTP password override
	#[clap(short, long)]
	pass: Option<String>,
	/// IPFS files (mfs) base path
	#[clap(short, long)]
	config: PathBuf,
	/// IPFS api url
	#[clap(short, long, default_value = "http://localhost:5001/")]
	api: String,
}

#[derive(Deserialize, Debug)]
struct Settings {
	#[serde(with = "url_serde")]
	source: Url,
	/// Reprieve period for local files after deletion on server
	#[serde(with = "serde_humantime", default)]
	reprieve: Duration,
	/// Ignore glob patterns when listing files on server (gitignore style)
	#[serde(default)]
	ignore: Vec<String>,
	/// FTP username - mostly just "anonymous"
	#[serde(default = "const_anonymous")]
	user: String,
	/// FTP password - mostly, just an e-mail
	pass: Option<String>,
	/// workdir
	#[serde(default)]
	workdir: Option<PathBuf>,
	/// datadir
	target: PathBuf,
}

fn const_anonymous() -> String { "anonymous".to_owned() }

#[tokio::main(basic_scheduler)]
async fn main() -> Result<()> {
	let opts: Opts = Opts::parse();

	let settings = get_settings(&opts.config)
		.context(format!("Loading {:?}", &opts.config))?;

	let out = ToMfs::new(&opts.api, settings)
		.await.with_context(|| format!("Failed to access mfs on ipfs at {}", opts.api))?;
	match run_sync(&opts, &out).await {
		Ok(_) => (),
		e@Err(_) => {
			out.failure_clean_lock().await.ok();
			e?;
		}
	}

	Ok(())
}

async fn run_sync(opts: &Opts, out: &ToMfs) -> Result<()> {
	let settings = out.settings();
	ensure!(settings.source.scheme() == "ftp",
		"Invalid source url {}: only ftp is supported (Will I support others? Who knows!)", settings.source);

	let current_set = out.prepare().await
		.with_context(|| format!("Failed to prepare mfs target folder in {:?} (and read current data state from {:?})", settings.workdir.as_ref().unwrap(), &settings.target))?;

	let mut ftp_stream = ftp_connect(&opts, &settings)
		.context("Failed to establish FTP connection")?;

	let ignore = ignore(&settings.ignore)
		.context("Constructing GlobSet for ignore list")?;

	let ups = Recursor::run(&mut ftp_stream, &ignore)
		.context("Retrieving file list")?;
	let sa = SyncActs::new(current_set, ups, settings.reprieve)
		.context("Internal error: failed to generate delta")?;

	println!("{:#?}", sa);
	out.apply(sa, &FtpProvider::new(ftp_stream)).await
		.context("Sync failure")?;

	Ok(())
}
	

fn ftp_connect(opts: &Opts, settings: &Settings) -> Result<FtpStream> {
	let host = settings.source.host_str()
		.context(format!("No host specified in source url {}", settings.source))?;
	let port = settings.source.port().unwrap_or(21);
	let mut ftp_stream = FtpStream::connect(format!("{}:{}", host, port))?;
	let user = opts.user.as_ref().unwrap_or(&settings.user);
	let pass = opts.pass.as_ref().or(settings.pass.as_ref())
		.context("No FTP password specified")?;
	ftp_stream.login(&user, &pass)?;
	ftp_stream.transfer_type(FileType::Binary)?;
	ftp_stream.cwd(settings.source.path())
		.with_context(|| format!("Cannot switch to directory {}", settings.source.path()))?;
	Ok(ftp_stream)
}

pub(crate) fn ignore<V: AsRef<[T]>, T: AsRef<str>>(base: V) -> Result<Gitignore> {
	let mut globs = GitignoreBuilder::new("");
	for (i, ign) in base.as_ref().iter().map(AsRef::as_ref).enumerate() {
		globs.add_line(None, ign)
			.with_context(|| format!("Parse ignore index {}: {}", i, ign))?;
	}
	return Ok(globs.build()?);
}

fn get_settings(path: &Path) -> Result<(Settings, Vec<u8>)> {
	let bytes: Vec<u8> = std::fs::read(path)?;
	let mut struc: Settings = serde_yaml::from_slice(&bytes)
		.context("Could not parse YAML")?;
	if struc.workdir.is_none() {
		let hash = blake3::Hasher::new().update(&bytes).finalize().to_hex();
		struc.workdir = Some(Path::new("/temp").join(hash.to_string()));
		// TODO: Warn
	}
	ensure!(struc.target.is_absolute(), "target path {:?} is not absolute", &struc.target);
	let workdir = struc.workdir.as_ref().unwrap();
	ensure!(workdir.is_absolute(), "work path {:?} is not absolute", &struc.workdir);
	ensure!(!(workdir.starts_with(&struc.target) || struc.target.starts_with(workdir)),
		"work path {:?} and target path path {:?} cannot contain one another", &workdir, &struc.target);
	return Ok((struc, bytes));
}
