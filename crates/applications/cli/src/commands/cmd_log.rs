use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::ports::repository::Repository;
use gfs_domain::usecases::repository::log_repo_usecase::LogRepoUseCase;

use crate::cli_utils::get_repo_dir;
use crate::output::{dimmed, green};

// ---------------------------------------------------------------------------
// Entry point called from main
// ---------------------------------------------------------------------------

pub async fn log(
    path: Option<PathBuf>,
    max_count: Option<usize>,
    from: Option<String>,
    until: Option<String>,
    full_hash: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let use_case = LogRepoUseCase::new(repository);

    let options = gfs_domain::ports::repository::LogOptions {
        from,
        until,
        limit: max_count,
    };

    let commits = use_case
        .run(repo_path, options)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    for cwr in &commits {
        print_commit_block(cwr, full_hash);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Display (git-style format)
// ---------------------------------------------------------------------------

fn print_commit_block(cwr: &gfs_domain::model::commit::CommitWithRefs, full_hash: bool) {
    let hash_full = cwr
        .commit
        .hash
        .as_deref()
        .unwrap_or("0000000000000000000000000000000000000000000000000000000000000000");

    // Display short or full hash based on flag
    let hash_display =
        gfs_domain::repo_utils::repo_layout::format_commit_hash(hash_full, full_hash);

    let refs_str = if cwr.refs.is_empty() {
        String::new()
    } else {
        format!(" ({})", cwr.refs.join(", "))
    };
    println!(
        "{} {}{}",
        dimmed("commit"),
        dimmed(hash_display),
        green(refs_str)
    );

    let author = &cwr.commit.author;
    let author_email = cwr.commit.author_email.as_deref().unwrap_or("");
    let author_line = if author_email.is_empty() {
        format!("{} {}", dimmed("Author:"), author)
    } else {
        format!(
            "{} {} <{}>",
            dimmed("Author:"),
            author,
            dimmed(author_email)
        )
    };
    println!("{}", author_line);

    let date_str = cwr.commit.author_date.format("%a %b %e %H:%M:%S %Y %z");
    println!("{}   {}", dimmed("Date:"), date_str);

    println!();
    for line in cwr.commit.message.lines() {
        println!("    {}", line);
    }
    if !cwr.commit.message.ends_with('\n') && !cwr.commit.message.is_empty() {
        println!();
    }
    println!();
}
