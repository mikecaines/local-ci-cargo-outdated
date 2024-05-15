use crate::args::AppArgs;
use clap::Parser;
use futures_util::{stream, StreamExt, TryStreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;
use tokio::io::AsyncWriteExt;
use tokio::{fs, process, sync};

mod args;

// exit code returned for successful state, but with outdated projects
const EXIT_CODE_OUTDATED: u8 = 3;

#[tokio::main]
async fn main() -> Result<ExitCode, BoxError> {
	let AppArgs {
		config_path,
		output_path,
	} = AppArgs::parse();

	let config: Config = toml::from_str(fs::read_to_string(config_path).await?.as_str())?;

	// channel to receive tuple of project label & cargo-outdated output text
	let (tx, mut rx) = sync::mpsc::unbounded_channel::<(String, String)>();

	// spawn a writer task, to collect the cargo-update results & save the output file
	// we will join this task later
	let writer_task = tokio::spawn(async move {
		let mut file_chunks = Vec::new();
		let mut has_outdated = false;

		while let Some((label, text)) = rx.recv().await {
			// tweak the output of cargo-update
			let text = text
				.lines()
				.filter(|line| {
					// filter out some "in progress" lines
					!line.contains("Blocking waiting for file lock on package cache")
						&& !line.contains("Updating git repository")
				})
				.map(|line| {
					// indent each line
					format!("\t{line}\n")
				})
				.collect::<String>();

			if !text.contains("All dependencies are up to date") {
				has_outdated = true;
			}

			file_chunks.push(format!("{label}\n{text}\n"));
		}

		if has_outdated {
			// create/overwrite the output file on disk
			let mut file = fs::File::create(output_path).await?;

			file_chunks.sort();

			for chunk in file_chunks {
				file.write_all(chunk.as_bytes()).await?;
			}
		} else {
			// delete the output file on disk
			// we ignore some expected types of errors
			if let Err(e) = fs::remove_file(output_path).await {
				match e.kind() {
					std::io::ErrorKind::NotFound => {}
					_ => Err(e)?,
				}
			}
		}

		Ok::<_, BoxError>(has_outdated)
	});

	// spawn a task for each source
	stream::iter(config.sources)
		.map(Ok)
		.try_for_each_concurrent(10, |(source_label, source_path)| {
			// create a clone of the tx for each source/task
			let tx = tx.clone();

			async move {
				// spawn the task and join it
				tokio::spawn(async move {
					// execute the call to cargo
					let command_result = process::Command::new("cargo")
						.arg("outdated")
						.arg("--root-deps-only")
						.current_dir(source_path) // set the CWD to the project's dir
						.output()
						.await;

					match command_result {
						// send a tuple of the source label & message text
						Ok(output) => tx.send((
							source_label,
							format!(
								"{} {}",
								String::from_utf8(output.stdout)?,
								String::from_utf8(output.stderr)?
							)
							.trim()
							.to_string(),
						))?,

						Err(e) => {
							tx.send((source_label, format!("Failed to call cargo: {:?}", e)))?
						}
					}

					Ok::<_, BoxError>(())
				})
				.await??; // the additional "?" is for the JoinHandle

				Ok::<_, BoxError>(())
			}
		})
		.await?;

	// drop the original tx
	// This ensures the rx will close when all tx clones are dropped
	drop(tx);

	// join the writer task and get the final "we have outdated projects" flag
	// the additional "?" is for the JoinHandle
	let has_outdated = writer_task.await??;

	if has_outdated {
		Ok(EXIT_CODE_OUTDATED.into())
	} else {
		Ok(ExitCode::SUCCESS)
	}
}

#[derive(Deserialize, Debug)]
struct Config {
	sources: HashMap<String, PathBuf>,
}

type BoxError = Box<dyn Error + Send + Sync + 'static>;
