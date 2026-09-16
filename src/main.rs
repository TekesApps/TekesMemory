use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    io::{BufRead, Read, Write},
    path::PathBuf,
    time::Duration,
};
use tekes_memory::{client::Client, common::*, config};
#[derive(Parser)]
#[command(
    version,
    about = "Independent Rust MCP memory service and Kernel lifecycle extension"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Init {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        thread_root: PathBuf,
        #[arg(long, default_value_t = 43187)]
        port: u16,
    },
    Serve {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        ready_file: Option<PathBuf>,
    },
    Kernel {
        #[arg(long)]
        config: PathBuf,
    },
    Schemas {
        #[arg(long)]
        directory: PathBuf,
    },
    LaunchdPlist {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Stdio {
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        credential_file: PathBuf,
    },
    Call {
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        credential_file: PathBuf,
        #[arg(long)]
        tool: String,
        #[arg(long)]
        arguments_file: PathBuf,
    },
}
fn print(v: &Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", canonical(v))?;
    out.flush()?;
    Ok(())
}
fn run(a: Args) -> Result<()> {
    match a.command {
        Command::Init {
            directory,
            workspace,
            thread_root,
            port,
        } => {
            if port == 0 {
                return Err(err("invalid_argument"));
            }
            print(&config::setup(&directory, &workspace, &thread_root, port)?)
        }
        Command::Serve { config, ready_file } => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(tekes_memory::server::serve(&config, ready_file.as_deref())),
        Command::Kernel { config: path } => {
            let config = config::adapter_config(&path)?;
            let mut raw = vec![];
            std::io::stdin()
                .take(MAX_BODY as u64 + 1)
                .read_to_end(&mut raw)?;
            if !raw.ends_with(b"\n")
                || raw[..raw.len().saturating_sub(1)].contains(&b'\n')
                || raw.contains(&b'\r')
            {
                return Err(err("invalid_argument"));
            }
            print(&tekes_memory::adapter::handle(&config, &decode(&raw)?)?)
        }
        Command::Call {
            endpoint,
            credential_file,
            tool,
            arguments_file,
        } => {
            let token = private_json(&credential_file)?;
            let mut raw = vec![];
            std::fs::File::open(arguments_file)?
                .take(MAX_BODY as u64 + 1)
                .read_to_end(&mut raw)?;
            print(
                &Client::open(&endpoint, text(&token, "token")?, Duration::from_secs(5))?
                    .call(&tool, decode(&raw)?)?,
            )
        }
        Command::Schemas { directory } => {
            std::fs::create_dir_all(&directory)?;
            for (name, schema) in config::SCHEMAS.iter() {
                std::fs::write(
                    directory.join(format!("{name}.schema.json")),
                    format!("{}\n", serde_json::to_string_pretty(schema)?),
                )?
            }
            Ok(())
        }
        Command::LaunchdPlist { config, output } => {
            config::service_config(&config)?;
            let config = config.canonicalize()?;
            let mut dict = plist::Dictionary::new();
            dict.insert("Label".into(), "local.tekesmemory".into());
            dict.insert(
                "ProgramArguments".into(),
                plist::Value::Array(vec![
                    std::env::current_exe()?
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                    "serve".into(),
                    "--config".into(),
                    config.to_string_lossy().into_owned().into(),
                ]),
            );
            dict.insert("RunAtLoad".into(), true.into());
            dict.insert("KeepAlive".into(), true.into());
            dict.insert("ThrottleInterval".into(), 10i64.into());
            dict.insert(
                "StandardErrorPath".into(),
                config
                    .parent()
                    .unwrap()
                    .join("service.stderr.log")
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            );
            let mut bytes = vec![];
            plist::Value::Dictionary(dict)
                .to_writer_xml(&mut bytes)
                .map_err(|_| err("invalid_config"))?;
            write_private(&output, &bytes)
        }
        Command::Stdio {
            endpoint,
            credential_file,
        } => {
            let token = private_json(&credential_file)?;
            let mut client = None;
            let mut input = std::io::stdin().lock();
            loop {
                let mut line = vec![];
                input
                    .by_ref()
                    .take(MAX_BODY as u64 + 1)
                    .read_until(b'\n', &mut line)?;
                if line.is_empty() {
                    break;
                }
                if !line.ends_with(b"\n") {
                    return Err(err("invalid_argument"));
                }
                let request = decode(&line)?;
                let method = text(&request, "method")?;
                // Use one HTTP session for the stdio connection; it is never a second DB owner.
                if method == "initialize" {
                    if client.is_some() {
                        return Err(err("invalid_argument"));
                    }
                    client = Some(Client::open(
                        &endpoint,
                        text(&token, "token")?,
                        Duration::from_secs(5),
                    )?);
                    print(
                        &json!({"jsonrpc":"2.0","id":request["id"],"result":{"protocolVersion":PROTOCOL,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"TekesMemory","version":env!("CARGO_PKG_VERSION")}}}),
                    )?;
                } else if method != "notifications/initialized" {
                    let c = client.as_mut().ok_or_else(|| err("invalid_argument"))?;
                    c.reset_deadline(Duration::from_secs(5));
                    let response = c.raw(request.clone())?;
                    if request.get("id").is_some() {
                        print(&response)?
                    }
                }
            }
            Ok(())
        }
    }
}
fn main() {
    unsafe { libc::umask(0o077) };
    if let Err(e) = run(Args::parse()) {
        eprintln!("tekes-memory: {}", e.0);
        std::process::exit(1)
    }
}
