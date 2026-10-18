//! Standalone rule checks/previews share the exact production time model.
use clap::{Parser, Subcommand};
use peri_scheduler::Timing;

#[derive(Parser)]
#[command(
    version,
    about = "Peri schedule rule inspector. Cloud-host owns embedded execution; no agent or transport credentials are required."
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Check {
        #[arg(long)]
        timing: String,
    },
    Preview {
        #[arg(long)]
        timing: String,
        #[arg(long)]
        after: Option<String>,
        #[arg(long,default_value_t=5,value_parser=clap::value_parser!(u16).range(1..=100))]
        count: u16,
    },
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let (input, after, count) = match args.command {
        Command::Check { timing } => (timing, None, 1),
        Command::Preview {
            timing,
            after,
            count,
        } => (timing, after, count),
    };
    if input.len() > 4096 {
        return Err("timing JSON too long".into());
    }
    let now = match after {
        Some(t) => chrono::DateTime::parse_from_rfc3339(&t)?.timestamp(),
        None => chrono::Utc::now().timestamp(),
    };
    let timing: Timing = serde_json::from_str(&input)?;
    let timing = timing.resolve(now)?;
    let mut cursor = now;
    let mut times = Vec::new();
    for _ in 0..count {
        let Some(next) = timing.next_after(cursor)? else {
            break;
        };
        times.push(
            chrono::DateTime::from_timestamp(next, 0)
                .ok_or("timestamp out of range")?
                .to_rfc3339(),
        );
        cursor = next;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"timing":timing,"next_utc":times}))?
    );
    Ok(())
}
