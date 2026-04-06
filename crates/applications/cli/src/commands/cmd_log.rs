use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::commit::{Commit, CommitWithRefs};
use gfs_domain::model::layout::{GFS_DIR, HEADS_DIR, REFS_DIR};
use gfs_domain::ports::repository::Repository;
use gfs_domain::repo_utils::repo_layout;
use gfs_domain::usecases::repository::log_repo_usecase::LogRepoUseCase;

use crate::cli_utils::get_repo_dir;
use crate::output::{dimmed, gold, green};

// ---------------------------------------------------------------------------
// Entry point called from main
// ---------------------------------------------------------------------------

pub async fn log(
    path: Option<PathBuf>,
    max_count: Option<usize>,
    from: Option<String>,
    until: Option<String>,
    full_hash: bool,
    graph: bool,
    all: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    if graph || all {
        run_graph(&repo_path, max_count, from, full_hash, all)?;
    } else {
        run_linear(&repo_path, max_count, from, until, full_hash).await?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Linear mode (existing behavior — first-parent walk)
// ---------------------------------------------------------------------------

async fn run_linear(
    repo_path: &Path,
    max_count: Option<usize>,
    from: Option<String>,
    until: Option<String>,
    full_hash: bool,
) -> Result<()> {
    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let use_case = LogRepoUseCase::new(repository);

    let options = gfs_domain::ports::repository::LogOptions {
        from,
        until,
        limit: max_count,
    };

    let commits = use_case
        .run(repo_path.to_path_buf(), options)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    for cwr in &commits {
        let dot_prefix = format!("  {}", gold("●"));
        let pipe_prefix = format!("  {}", dimmed("│"));
        print_commit_block(cwr, full_hash, &dot_prefix, &pipe_prefix);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Graph mode — full DAG visualization
// ---------------------------------------------------------------------------

/// Collect all branch tips: Vec<(branch_name, commit_hash)>.
fn list_branches(repo_path: &Path) -> Result<Vec<(String, String)>> {
    let refs_dir = repo_path.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
    if !refs_dir.exists() {
        return Ok(Vec::new());
    }
    collect_refs(&refs_dir, "")
}

fn collect_refs(dir: &Path, prefix: &str) -> Result<Vec<(String, String)>> {
    let mut result = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let branch_name = if prefix.is_empty() {
            name
        } else {
            format!("{}/{}", prefix, name)
        };
        if path.is_file() {
            let tip = std::fs::read_to_string(&path)?.trim().to_string();
            result.push((branch_name, tip));
        } else if path.is_dir() {
            result.extend(collect_refs(&path, &branch_name)?);
        }
    }
    Ok(result)
}

/// A commit node in the DAG with its metadata.
struct GraphCommit {
    commit: Commit,
    refs: Vec<String>,
    parents: Vec<String>,
}

/// Walk the full DAG from the given starting points, collecting all reachable commits.
fn collect_dag(
    repo_path: &Path,
    start_hashes: &[String],
    limit: Option<usize>,
) -> Result<Vec<GraphCommit>> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut commits: Vec<GraphCommit> = Vec::new();

    for hash in start_hashes {
        if visited.insert(hash.clone()) {
            queue.push_back(hash.clone());
        }
    }

    while let Some(hash) = queue.pop_front() {
        if hash == "0" || hash.is_empty() {
            continue;
        }

        let commit = match repo_layout::get_commit_from_hash(repo_path, &hash) {
            Ok(mut c) => {
                c.hash = Some(hash.clone());
                c
            }
            Err(_) => continue,
        };

        let parents: Vec<String> = commit
            .parents
            .as_ref()
            .map(|p| {
                p.iter()
                    .filter(|h| *h != "0" && !h.is_empty())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        // Enqueue parents
        for parent in &parents {
            if visited.insert(parent.clone()) {
                queue.push_back(parent.clone());
            }
        }

        let refs = repo_layout::get_refs_pointing_to(repo_path, &hash).unwrap_or_default();

        commits.push(GraphCommit {
            commit,
            refs,
            parents,
        });

        if let Some(max) = limit {
            if commits.len() >= max {
                break;
            }
        }
    }

    // Sort by timestamp descending (newest first)
    commits.sort_by(|a, b| b.commit.author_date.cmp(&a.commit.author_date));

    Ok(commits)
}

/// Assign each commit a column (lane) and compute the graph lines.
///
/// The algorithm tracks "active lanes" — each lane holds the hash of the next
/// expected commit in that column. When a commit appears, it occupies its lane;
/// its parents replace it in the active set.
fn render_graph(commits: &[GraphCommit], full_hash: bool) {
    // Active lanes: each slot holds a commit hash that we expect to see next.
    let mut lanes: Vec<String> = Vec::new();

    for gc in commits {
        let hash = gc.commit.hash.as_deref().unwrap_or("");

        // Find which lane this commit occupies (or allocate a new one).
        let my_col = if let Some(pos) = lanes.iter().position(|h| h == hash) {
            pos
        } else {
            // New branch head — find an empty slot or push a new lane.
            if let Some(pos) = lanes.iter().position(|h| h.is_empty()) {
                lanes[pos] = hash.to_string();
                pos
            } else {
                lanes.push(hash.to_string());
                lanes.len() - 1
            }
        };

        let num_lanes = lanes.len();

        // Build the graph prefix for the commit line (● in my_col, │ elsewhere).
        let commit_prefix = build_commit_line(&lanes, my_col);

        // Build the connector prefix for continuation lines (│ everywhere active).
        let continuation_prefix = build_continuation_line(&lanes, my_col);

        // Determine parent placement.
        let first_parent = gc.parents.first().cloned().unwrap_or_default();
        let merge_parents: Vec<String> = gc.parents.iter().skip(1).cloned().collect();

        // Check if first parent already occupies another lane (convergence).
        let mut converge_cols: Vec<usize> = Vec::new();
        if !first_parent.is_empty() {
            if let Some(existing) = lanes.iter().position(|h| h == &first_parent) {
                if existing != my_col {
                    // Parent already has a lane elsewhere — converge into it, close my lane.
                    converge_cols.push(existing);
                    lanes[my_col] = String::new();
                } else {
                    lanes[my_col] = first_parent;
                }
            } else {
                lanes[my_col] = first_parent;
            }
        } else {
            lanes[my_col] = String::new(); // Root commit — lane dies.
        }

        // Place merge parents: find or allocate lanes.
        for mp in &merge_parents {
            if let Some(pos) = lanes.iter().position(|h| h == mp) {
                converge_cols.push(pos);
            } else if let Some(pos) = lanes
                .iter()
                .enumerate()
                .position(|(i, h)| h.is_empty() && i != my_col)
            {
                lanes[pos] = mp.clone();
                converge_cols.push(pos);
            } else {
                lanes.push(mp.clone());
                converge_cols.push(lanes.len() - 1);
            }
        }

        // Print the commit block with the graph prefix.
        let cwr = CommitWithRefs {
            commit: gc.commit.clone(),
            refs: gc.refs.clone(),
        };
        print_commit_block(&cwr, full_hash, &commit_prefix, &continuation_prefix);

        // Print convergence/merge line after the commit block.
        if !converge_cols.is_empty() {
            let merge_line = build_merge_line(&lanes, my_col, &converge_cols, num_lanes);
            println!("{}", merge_line);
        }

        // Trim trailing empty lanes.
        while lanes.last().is_some_and(|h| h.is_empty()) {
            lanes.pop();
        }
    }
}

/// Build the graph prefix for a commit line: ● in `col`, │ in other active lanes.
fn build_commit_line(lanes: &[String], col: usize) -> String {
    let width = lanes.len().max(col + 1);
    let mut parts: Vec<String> = Vec::with_capacity(width);
    for i in 0..width {
        if i == col {
            parts.push(gold("●"));
        } else if !lanes.get(i).is_some_and(|h| h.is_empty()) {
            parts.push(dimmed("│"));
        } else {
            parts.push(" ".to_string());
        }
    }
    format!("  {}", parts.join(" "))
}

/// Build the continuation prefix: │ in all active lanes.
fn build_continuation_line(lanes: &[String], _col: usize) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(lanes.len());
    for lane in lanes {
        if !lane.is_empty() {
            parts.push(dimmed("│"));
        } else {
            parts.push(" ".to_string());
        }
    }
    format!("  {}", parts.join(" "))
}

/// Build a merge/convergence line showing a branch closing into a parent lane.
/// `my_col` is the branch that just closed; `merge_cols` are the target parent lanes.
/// Example: ├─╯   (my_col=1 converging left into col 0)
///          ╰─┤   (my_col=0 converging right into col 1)
fn build_merge_line(
    lanes: &[String],
    my_col: usize,
    merge_cols: &[usize],
    prev_num_lanes: usize,
) -> String {
    let width = lanes.len().max(prev_num_lanes).max(my_col + 1);

    // Determine the span.
    let all_cols: Vec<usize> = std::iter::once(my_col)
        .chain(merge_cols.iter().copied())
        .collect();
    let min_col = *all_cols.iter().min().unwrap();
    let max_col = *all_cols.iter().max().unwrap();

    let mut line = vec![' '; width * 2]; // char + separator for each column

    // Fill in active lane pipes first.
    for i in 0..width {
        let pos = i * 2;
        let is_active = lanes.get(i).is_some_and(|h| !h.is_empty());
        if is_active && i != my_col && !merge_cols.contains(&i) {
            line[pos] = '│';
        }
    }

    // Draw horizontal connections in the merge range.
    let start_pos = min_col * 2;
    let end_pos = max_col * 2;
    for p in start_pos..=end_pos {
        if line[p] == ' ' {
            line[p] = '─';
        }
    }

    // Place junction characters at key columns.
    for &mc in merge_cols {
        let pos = mc * 2;
        // The parent lane continues down and receives a branch.
        if mc < my_col {
            line[pos] = '├';
        } else {
            line[pos] = '┤';
        }
    }

    // Place the closing corner at my_col.
    let my_pos = my_col * 2;
    if my_col > *merge_cols.iter().min().unwrap_or(&my_col) {
        line[my_pos] = '╯'; // Branch closing to the left.
    } else {
        line[my_pos] = '╰'; // Branch closing to the right.
    }

    // Trim trailing spaces.
    let s: String = line.into_iter().collect();
    let trimmed = s.trim_end();
    format!("  {}", dimmed(trimmed))
}


fn run_graph(
    repo_path: &Path,
    max_count: Option<usize>,
    from: Option<String>,
    full_hash: bool,
    all: bool,
) -> Result<()> {
    let start_hashes = if all {
        // Collect all branch tips.
        let branches = list_branches(repo_path)?;
        if branches.is_empty() {
            return Ok(());
        }
        branches.into_iter().map(|(_, hash)| hash).collect()
    } else {
        // Start from HEAD or specified revision.
        let start = if let Some(ref rev) = from {
            repo_layout::rev_parse(repo_path, rev)
                .map_err(|e| anyhow::anyhow!("failed to resolve revision: {e}"))?
        } else {
            repo_layout::get_current_commit_id(repo_path)
                .map_err(|e| anyhow::anyhow!("failed to get HEAD: {e}"))?
        };
        vec![start]
    };

    let commits = collect_dag(repo_path, &start_hashes, max_count)?;
    if commits.is_empty() {
        return Ok(());
    }

    render_graph(&commits, full_hash);

    Ok(())
}

// ---------------------------------------------------------------------------
// Display — shared commit block printer
// ---------------------------------------------------------------------------

fn print_commit_block(
    cwr: &CommitWithRefs,
    full_hash: bool,
    commit_prefix: &str,
    continuation_prefix: &str,
) {
    let hash_full = cwr
        .commit
        .hash
        .as_deref()
        .unwrap_or("0000000000000000000000000000000000000000000000000000000000000000");

    let hash_display = repo_layout::format_commit_hash(hash_full, full_hash);

    let refs_str = if cwr.refs.is_empty() {
        String::new()
    } else {
        format!("  ({})", cwr.refs.join(", "))
    };

    // Commit line (● already in prefix for graph mode)
    println!(
        "{} {}{}",
        commit_prefix,
        dimmed(hash_display),
        green(refs_str)
    );

    // Author line
    let author = &cwr.commit.author;
    let author_email = cwr.commit.author_email.as_deref().unwrap_or("");
    if author_email.is_empty() {
        println!(
            "{} {} {}",
            continuation_prefix,
            dimmed("Author:"),
            author
        );
    } else {
        println!(
            "{} {} {} <{}>",
            continuation_prefix,
            dimmed("Author:"),
            author,
            dimmed(author_email)
        );
    }

    // Date line
    let date_str = cwr.commit.author_date.format("%a %b %e %H:%M:%S %Y %z");
    println!(
        "{} {}   {}",
        continuation_prefix,
        dimmed("Date:"),
        date_str
    );

    // Blank separator
    println!("{}", continuation_prefix);

    // Message body (indented)
    for line in cwr.commit.message.lines() {
        println!("{}     {}", continuation_prefix, line);
    }
    if !cwr.commit.message.ends_with('\n') && !cwr.commit.message.is_empty() {
        println!("{}", continuation_prefix);
    }
    println!("{}", continuation_prefix);
}
