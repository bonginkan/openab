mod manifest;
mod apply;
mod get;
mod delete;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "oabctl", about = "OAB agent provisioner for ECS")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create or update OAB services from manifest files
    Apply {
        /// Path to manifest file or directory
        #[arg(short, long)]
        file: String,
    },
    /// List OAB services and their status
    Get {
        /// Resource type
        resource: String,
        /// Optional resource name
        name: Option<String>,
        /// ECS cluster name
        #[arg(long, default_value = "default")]
        cluster: String,
    },
    /// Delete an OAB service
    Delete {
        /// Resource type
        resource: String,
        /// Resource name
        name: String,
        /// ECS cluster name
        #[arg(long, default_value = "default")]
        cluster: String,
        /// Namespace
        #[arg(long, default_value = "prod")]
        namespace: String,
    },
    /// Execute a command in an agent container (via ecsctl)
    Exec {
        /// Agent name (alias)
        agent: String,
        /// Command to run (default: /bin/sh). Use -- to separate args.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Copy files to/from agent containers (via ecsctl)
    Cp {
        /// Source path (local or agent:/path)
        src: String,
        /// Destination path (local or agent:/path)
        dst: String,
    },
    /// Sync directories between local machine and agent containers (via ecsctl)
    Sync {
        /// Source: local dir or agent:/path
        src: String,
        /// Destination: agent:/path or local dir
        dst: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

    match cli.command {
        Commands::Apply { file } => apply::run(&config, &file).await,
        Commands::Get { resource, name, cluster } => get::run(&config, &resource, name.as_deref(), &cluster).await,
        Commands::Delete { resource, name, cluster, namespace } => {
            delete::run(&config, &resource, &name, &cluster, &namespace).await
        }
        Commands::Exec { agent, command } => {
            let resolved = ecsctl::alias::resolve(&config, &agent).await?;
            let cmd = if command.is_empty() {
                None
            } else {
                // Join args with single-quote escaping to prevent shell interpretation
                let joined = command.iter().map(|a| {
                    format!("'{}'", a.replace('\'', "'\\''"))
                }).collect::<Vec<_>>().join(" ");
                Some(joined)
            };
            ecsctl::exec::run(&config, &resolved, cmd.as_deref()).await
        }
        Commands::Cp { src, dst } => {
            let src = ecsctl::alias::resolve_remote(&config, &src).await?;
            let dst = ecsctl::alias::resolve_remote(&config, &dst).await?;
            eprintln!("⇄ Copying {} → {} ...", src, dst);
            ecsctl::cp::run(&config, &src, &dst, None, 60).await?;
            eprintln!("✓ Done");
            Ok(())
        }
        Commands::Sync { src, dst } => {
            let src = ecsctl::alias::resolve_remote(&config, &src).await?;
            let dst = ecsctl::alias::resolve_remote(&config, &dst).await?;
            let src_remote = ecsctl::cp::is_remote(&src);
            let dst_remote = ecsctl::cp::is_remote(&dst);
            eprintln!("⇄ Syncing {} → {} ...", src, dst);
            match (src_remote, dst_remote) {
                (false, true) => {
                    ecsctl::sync::run(&config, &src, &dst, None, 60).await?;
                }
                (true, false) => {
                    ecsctl::sync::run_download(&config, &src, &dst, None, 60).await?;
                }
                _ => anyhow::bail!("exactly one of src/dst must be a remote path (agent:/path)"),
            }
            eprintln!("✓ Done");
            Ok(())
        }
    }
}
