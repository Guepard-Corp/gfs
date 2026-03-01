use crate::components::{CodeBlock, SchemaDiffTabs};
use leptos::*;
use leptos_router::*;

#[component]
pub fn Docs() -> impl IntoView {
    let params = use_params_map();
    let page = move || {
        params.with(|p| {
            p.get("page")
                .cloned()
                .unwrap_or_else(|| "getting-started".to_string())
        })
    };

    view! {
        <div class="docs-page">
            <div class="container">
                <div class="docs-layout">
                    <aside class="docs-sidebar">
                        <nav class="docs-nav">
                            <div class="nav-section">
                                <h3>"Getting Started"</h3>
                                <ul>
                                    <li><A href="/docs" class="nav-item">"Introduction"</A></li>
                                    <li><A href="/docs/installation" class="nav-item">"Installation"</A></li>
                                    <li><A href="/docs/quick-start" class="nav-item">"Quick Start"</A></li>
                                </ul>
                            </div>
                            <div class="nav-section">
                                <h3>"Commands"</h3>
                                <ul>
                                    <li><A href="/docs/commands/init" class="nav-item">"gfs init"</A></li>
                                    <li><A href="/docs/commands/status" class="nav-item">"gfs status"</A></li>
                                    <li><A href="/docs/commands/commit" class="nav-item">"gfs commit"</A></li>
                                    <li><A href="/docs/commands/log" class="nav-item">"gfs log"</A></li>
                                    <li><A href="/docs/commands/checkout" class="nav-item">"gfs checkout"</A></li>
                                    <li><A href="/docs/commands/providers" class="nav-item">"gfs providers"</A></li>
                                    <li><A href="/docs/commands/query" class="nav-item">"gfs query"</A></li>
                                    <li><A href="/docs/commands/schema" class="nav-item">"gfs schema"</A></li>
                                    <li><A href="/docs/commands/export" class="nav-item">"gfs export"</A></li>
                                    <li><A href="/docs/commands/import" class="nav-item">"gfs import"</A></li>
                                    <li><A href="/docs/commands/config" class="nav-item">"gfs config"</A></li>
                                    <li><A href="/docs/commands/compute" class="nav-item">"gfs compute"</A></li>
                                </ul>
                            </div>
                            <div class="nav-section">
                                <h3>"MCP Server"</h3>
                                <ul>
                                    <li><A href="/docs/mcp/overview" class="nav-item">"Overview"</A></li>
                                    <li><A href="/docs/mcp/claude-desktop" class="nav-item">"Claude Desktop"</A></li>
                                    <li><A href="/docs/mcp/claude-code" class="nav-item">"Claude Code"</A></li>
                                    <li><A href="/docs/mcp/cursor" class="nav-item">"Cursor"</A></li>
                                    <li><A href="/docs/mcp/http-mode" class="nav-item">"HTTP Mode"</A></li>
                                </ul>
                            </div>
                            <div class="nav-section">
                                <h3>"AI Agents"</h3>
                                <ul>
                                    <li><A href="/docs/ai-agents/skills" class="nav-item">"Skills"</A></li>
                                    <li><A href="/docs/ai-agents/subagents" class="nav-item">"Subagents"</A></li>
                                </ul>
                            </div>
                            <div class="nav-section">
                                <h3>"Advanced"</h3>
                                <ul>
                                    <li><A href="/docs/configuration" class="nav-item">"Configuration"</A></li>
                                    <li><A href="/docs/troubleshooting" class="nav-item">"Troubleshooting"</A></li>
                                    <li><A href="/docs/development" class="nav-item">"Development"</A></li>
                                </ul>
                            </div>
                        </nav>
                    </aside>
                    <article class="docs-content">
                        {move || match page().as_str() {
                            "" | "getting-started" => view! { <GettingStarted/> }.into_view(),
                            "installation" => view! { <Installation/> }.into_view(),
                            "quick-start" => view! { <QuickStart/> }.into_view(),
                            "commands/init" => view! { <CommandInit/> }.into_view(),
                            "commands/status" => view! { <CommandStatus/> }.into_view(),
                            "commands/commit" => view! { <CommandCommit/> }.into_view(),
                            "commands/log" => view! { <CommandLog/> }.into_view(),
                            "commands/checkout" => view! { <CommandCheckout/> }.into_view(),
                            "commands/providers" => view! { <CommandProviders/> }.into_view(),
                            "commands/query" => view! { <CommandQuery/> }.into_view(),
                            "commands/schema" => view! { <CommandSchema/> }.into_view(),
                            "commands/export" => view! { <CommandExport/> }.into_view(),
                            "commands/import" => view! { <CommandImport/> }.into_view(),
                            "commands/config" => view! { <CommandConfig/> }.into_view(),
                            "commands/compute" => view! { <CommandCompute/> }.into_view(),
                            "ai-agents/skills" => view! { <AiAgentsSkills/> }.into_view(),
                            "ai-agents/subagents" => view! { <AiAgentsSubagents/> }.into_view(),
                            "mcp/overview" => view! { <McpOverview/> }.into_view(),
                            "mcp/claude-desktop" => view! { <McpClaudeDesktop/> }.into_view(),
                            "mcp/claude-code" => view! { <McpClaudeCode/> }.into_view(),
                            "mcp/cursor" => view! { <McpCursor/> }.into_view(),
                            "mcp/http-mode" => view! { <McpHttpMode/> }.into_view(),
                            _ => view! { <ComingSoon page=page()/> }.into_view(),
                        }}
                    </article>
                </div>
            </div>
        </div>
    }
}

#[component]
fn GettingStarted() -> impl IntoView {
    view! {
        <div>
            <h1>"Getting Started with GFS"</h1>
            <p class="lead">"GFS (Git For database Systems) brings Git-like version control to your databases."</p>

            <h2>"What is GFS?"</h2>
            <p>"GFS enables you to:"</p>
            <ul>
                <li><strong>"Commit"</strong>" database states with meaningful messages"</li>
                <li><strong>"Branch"</strong>" and "<strong>"merge"</strong>" database schemas and data"</li>
                <li><strong>"Time travel"</strong>" through your database history"</li>
                <li><strong>"Collaborate"</strong>" on database changes with confidence"</li>
                <li><strong>"Rollback"</strong>" to any previous state instantly"</li>
            </ul>

            <div class="alert warning">
                <strong>"⚠️ Important Notice"</strong>
                <p>"This project is under active development and not yet suitable for production use. Expect breaking changes, incomplete features, and evolving APIs."</p>
            </div>

            <h2>"Supported Databases"</h2>
            <ul>
                <li>"PostgreSQL (versions 13-18)"</li>
                <li>"MySQL (versions 8.0-8.1)"</li>
            </ul>

            <h2>"Requirements"</h2>
            <ul>
                <li>"Docker (latest version recommended)"</li>
                <li>"Bash/Zsh shell"</li>
                <li><code>"curl"</code>" for installation"</li>
                <li><code>"tar"</code>" for extracting releases"</li>
            </ul>

            <h2>"Next Steps"</h2>
            <p>"Continue to "<a href="/docs/installation">"Installation"</a>" to set up GFS on your system."</p>
        </div>
    }
}

#[component]
fn Installation() -> impl IntoView {
    view! {
        <div>
            <h1>"Installation"</h1>

            <h2>"Quick Install"</h2>
            <p>"The easiest way to install GFS is using our installation script:"</p>
            <CodeBlock code="curl -fsSL https://gfs.guepard.run/install | bash"/>

            <h2>"Build from Source"</h2>
            <p>"If you prefer to build from source:"</p>
            <CodeBlock code="git clone https://github.com/Guepard-Corp/gfs.git
cd gfs
cargo build --release"/>
            <p>"The binary will be available at "<code>"target/release/gfs"</code>"."</p>

            <h2>"Verify Installation"</h2>
            <p>"After installation, verify that GFS is working:"</p>
            <CodeBlock code="gfs --version"/>

            <h2>"Docker Setup"</h2>
            <p>"GFS requires Docker to be installed and running. Make sure Docker is available before using GFS:"</p>
            <ul>
                <li>"macOS/Windows: Install "<a href="https://www.docker.com/products/docker-desktop/" target="_blank">"Docker Desktop"</a></li>
                <li>"Linux: Install Docker Engine using your distribution's package manager"</li>
            </ul>

            <h2>"Next Steps"</h2>
            <p>"Continue to "<a href="/docs/quick-start">"Quick Start"</a>" to create your first GFS repository."</p>
        </div>
    }
}

#[component]
fn QuickStart() -> impl IntoView {
    view! {
        <div>
            <h1>"Quick Start"</h1>

            <h2>"1. Check Available Providers"</h2>
            <p>"First, see what database providers are available:"</p>
            <CodeBlock code="gfs providers"/>

            <h2>"2. Create a New Project"</h2>
            <CodeBlock code="mkdir my_project
    cd my_project"/>

            <h2>"3. Initialize the Repository"</h2>
            <CodeBlock code="gfs init --database-provider postgres --database-version 17"/>
            <p>"This creates a "<code>".gfs"</code>" directory and starts a PostgreSQL database in a Docker container."</p>

            <h2>"4. Check Status"</h2>
            <CodeBlock code="gfs status"/>

            <h2>"5. Query Your Database"</h2>
            <p>"Execute SQL directly or open an interactive terminal:"</p>
            <CodeBlock code="gfs query \"SELECT 1\"
    # Or: gfs query (interactive)"/>

            <h2>"6. Make Changes and Commit"</h2>
            <p>"After modifying your database schema or data:"</p>
            <CodeBlock code="gfs commit -m \"my first commit\""/>

            <h2>"7. View Commit History"</h2>
            <CodeBlock code="gfs log"/>

            <h2>"8. Time Travel"</h2>
            <p>"Checkout a previous commit:"</p>
            <CodeBlock code="gfs checkout <commit_hash>"/>

            <h2>"9. Work with Branches"</h2>
            <p>"Create a new branch:"</p>
            <CodeBlock code="gfs checkout -b release"/>
            <p>"Switch back to main:"</p>
            <CodeBlock code="gfs checkout main"/>

            <h2>"Next Steps"</h2>
            <p>"Explore the "<a href="/docs/commands/init">"Commands"</a>" section to learn more about what you can do with GFS."</p>
        </div>
    }
}

#[component]
fn CommandProviders() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs providers"</h1>
            <p class="lead">"List available database providers and their supported versions."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs providers [PROVIDER]"/>

            <h2>"Description"</h2>
            <p>"The "<code>"providers"</code>" command displays all supported database providers along with their available versions and features."</p>

            <h2>"Examples"</h2>
            <h3>"List all providers"</h3>
            <CodeBlock code="gfs providers"/>
            <p>"Output:"</p>
            <pre><code>"  database_provider    | version                        | features                                          \n  ---------------------+--------------------------------+---------------------------------------------------\n  mysql                | 8.0, 8.1                       | tls, schema, masking, backup, import              \n  postgres             | 13, 14, 15, 16, 17, 18         | tls, schema, masking, auto-scaling, performance...\n\n  Images are pulled from Docker Hub by default."</code></pre>

            <h3>"Show details for a specific provider"</h3>
            <CodeBlock code="gfs providers postgres"/>

            <h2>"Supported Providers"</h2>
            <h3>"PostgreSQL"</h3>
            <ul>
                <li><strong>"Versions:"</strong>" 13, 14, 15, 16, 17, 18"</li>
                <li><strong>"Features:"</strong>" TLS, schema management, data masking, auto-scaling, performance monitoring"</li>
            </ul>

            <h3>"MySQL"</h3>
            <ul>
                <li><strong>"Versions:"</strong>" 8.0, 8.1"</li>
                <li><strong>"Features:"</strong>" TLS, schema management, data masking, backup, import"</li>
            </ul>

            <h2>"Use Case"</h2>
            <p>"Run this command before initializing a repository to see what database providers and versions are available."</p>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/init">"gfs init"</a>" - Initialize a repository with a specific provider"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandInit() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs init"</h1>
            <p class="lead">"Initialize a new GFS repository."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs init --database-provider <PROVIDER> --database-version <VERSION>"/>

            <h2>"Options"</h2>
            <ul>
                <li><code>"--database-provider"</code>" (required) - Database type (e.g., "<code>"postgres"</code>", "<code>"mysql"</code>")"</li>
                <li><code>"--database-version"</code>" (required) - Database version (e.g., "<code>"17"</code>" for PostgreSQL, "<code>"8.0"</code>" for MySQL)"</li>
            </ul>

            <h2>"Description"</h2>
            <p>"The "<code>"init"</code>" command creates a new GFS repository in the current directory. It:"</p>
            <ul>
                <li>"Creates a "<code>".gfs"</code>" directory to store repository metadata"</li>
                <li>"Starts a Docker container with the specified database"</li>
                <li>"Initializes the database for version control"</li>
                <li>"Creates an initial commit (root commit)"</li>
            </ul>

            <h2>"Examples"</h2>
            <h3>"Initialize with PostgreSQL 17"</h3>
            <CodeBlock code="gfs init --database-provider postgres --database-version 17"/>

            <h3>"Initialize with MySQL 8.0"</h3>
            <CodeBlock code="gfs init --database-provider mysql --database-version 8.0"/>

            <h2>"What Happens"</h2>
            <ol>
                <li>"A "<code>".gfs"</code>" directory is created in your current directory"</li>
                <li>"Docker pulls the specified database image if not already available"</li>
                <li>"A Docker container starts with the database"</li>
                <li>"The database is configured for GFS version control"</li>
                <li>"Connection information is displayed"</li>
            </ol>

            <h2>"Query Your Database"</h2>
            <p>"Use "<code>"gfs query"</code>" to run SQL or open an interactive session. No separate database client needed:"</p>
            <CodeBlock code="gfs query \"SELECT 1\"
    gfs query  # interactive terminal"/>

            <h2>"Requirements"</h2>
            <ul>
                <li>"Docker must be installed and running"</li>
                <li>"The current directory should be empty or not already a GFS repository"</li>
                <li>"Sufficient disk space for the database container"</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/providers">"gfs providers"</a>" - List available providers and versions"</li>
                <li><a href="/docs/commands/status">"gfs status"</a>" - Check repository status"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandStatus() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs status"</h1>
            <p class="lead">"Show the current state of storage and compute resources."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs status"/>

            <h2>"Description"</h2>
            <p>"The "<code>"status"</code>" command displays information about your GFS repository, including:"</p>
            <ul>
                <li>"Current branch"</li>
                <li>"Database connection information"</li>
                <li>"Docker container status"</li>
                <li>"Storage backend information"</li>
                <li>"Compute resource status"</li>
            </ul>

            <h2>"Example Output"</h2>
            <pre><code>"  Repository\n  ────────────────────────────────────────\n  Branch               main\n  Active workspace     .gfs/workspaces/main/0/data\n\n  Compute\n  ────────────────────────────────────────\n  Provider             postgres\n  Version              17\n  Status               ● running\n  Container ID         37f65464d421…\n  Container data dir   .gfs/workspaces/main/0/data\n  Connection           postgresql://postgres:postgres@localhost:55251/postgres"</code></pre>

            <h2>"Use Cases"</h2>
            <ul>
                <li>"Check if the database container is running"</li>
                <li>"Get connection details for your database"</li>
                <li>"Verify which branch you're currently on"</li>
                <li>"Troubleshoot connection issues"</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/init">"gfs init"</a>" - Initialize a repository"</li>
                <li><a href="/docs/commands/log">"gfs log"</a>" - View commit history"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandCommit() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs commit"</h1>
            <p class="lead">"Commit the current database state."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs commit -m <MESSAGE>"/>

            <h2>"Options"</h2>
            <ul>
                <li><code>"-m, --message"</code>" (required) - Commit message describing the changes"</li>
            </ul>

            <h2>"Description"</h2>
            <p>"The "<code>"commit"</code>" command creates a snapshot of your current database state, including:"</p>
            <ul>
                <li>"Schema changes (tables, columns, indexes, constraints)"</li>
                <li>"Data changes (inserts, updates, deletes)"</li>
                <li>"Database configuration"</li>
            </ul>

            <h2>"Examples"</h2>
            <h3>"Commit with a message"</h3>
            <CodeBlock code="gfs commit -m \"Add users table\""/>
            <p>"Output:"</p>
            <pre><code>"[main] 88b0ff8  Add users table"</code></pre>

            <h3>"Commit schema changes"</h3>
            <CodeBlock code="gfs commit -m \"Add email column to users table\""/>

            <h3>"Commit data changes"</h3>
            <CodeBlock code="gfs commit -m \"Import initial user data\""/>

            <h2>"How It Works"</h2>
            <ol>
                <li>"GFS captures a complete snapshot of your database"</li>
                <li>"The snapshot is stored efficiently using deduplication"</li>
                <li>"A commit hash is generated"</li>
                <li>"The commit is added to the current branch's history"</li>
            </ol>

            <h2>"Best Practices"</h2>
            <ul>
                <li>"Write clear, descriptive commit messages"</li>
                <li>"Commit logical units of change"</li>
                <li>"Test your changes before committing"</li>
                <li>"Commit frequently to maintain detailed history"</li>
            </ul>

            <h2>"Commit Message Guidelines"</h2>
            <ul>
                <li>"Use imperative mood: \"Add column\" not \"Added column\""</li>
                <li>"Be specific: \"Add email index to users table\" not \"Update database\""</li>
                <li>"Keep it concise but informative"</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/log">"gfs log"</a>" - View commit history"</li>
                <li><a href="/docs/commands/checkout">"gfs checkout"</a>" - Switch to a different commit"</li>
                <li><a href="/docs/commands/status">"gfs status"</a>" - Check repository status"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandLog() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs log"</h1>
            <p class="lead">"Show the commit history."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs log"/>

            <h2>"Description"</h2>
            <p>"The "<code>"log"</code>" command displays the commit history of the current branch, showing:"</p>
            <ul>
                <li>"Commit hash (short form)"</li>
                <li>"Commit message"</li>
                <li>"Author information"</li>
                <li>"Timestamp"</li>
                <li>"Branch information"</li>
            </ul>

            <h2>"Example Output"</h2>
            <pre><code>"commit 88b0ff8 (HEAD -> main, main)\nAuthor: user\nDate:   Sun Mar  1 12:56:43 2026 +0000\n\n    Add users table"</code></pre>

            <h2>"Understanding the Output"</h2>
            <ul>
                <li><strong>"commit hash"</strong>" - Unique identifier for the commit"</li>
                <li><strong>"HEAD"</strong>" - Current commit you're on"</li>
                <li><strong>"branch name"</strong>" - Branch this commit belongs to"</li>
                <li><strong>"Author"</strong>" - Who made the commit"</li>
                <li><strong>"Date"</strong>" - When the commit was made"</li>
                <li><strong>"message"</strong>" - Description of the changes"</li>
            </ul>

            <h2>"Use Cases"</h2>
            <ul>
                <li>"Review what changes were made to the database"</li>
                <li>"Find a specific commit to checkout"</li>
                <li>"Understand the evolution of your database schema"</li>
                <li>"Debug when a change was introduced"</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/commit">"gfs commit"</a>" - Create a new commit"</li>
                <li><a href="/docs/commands/checkout">"gfs checkout"</a>" - Switch to a different commit"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandCheckout() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs checkout"</h1>
            <p class="lead">"Switch to a different commit or branch."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="# Checkout a specific commit
gfs checkout <COMMIT_HASH>

# Create and checkout a new branch
gfs checkout -b <BRANCH_NAME>

# Checkout an existing branch
gfs checkout <BRANCH_NAME>"/>

            <h2>"Options"</h2>
            <ul>
                <li><code>"-b"</code>" - Create a new branch and switch to it"</li>
            </ul>

            <h2>"Description"</h2>
            <p>"The "<code>"checkout"</code>" command allows you to:"</p>
            <ul>
                <li>"Travel back to any previous database state"</li>
                <li>"Switch between branches"</li>
                <li>"Create new branches from the current state"</li>
            </ul>
            <p>"When you checkout a commit or branch, GFS restores your database to that exact state."</p>

            <h2>"Examples"</h2>
            <h3>"Checkout a specific commit"</h3>
            <CodeBlock code="gfs checkout 88b0ff8"/>
            <p>"Your database will be restored to the state at that commit."</p>

            <h3>"Create a new branch"</h3>
            <CodeBlock code="gfs checkout -b feature-test"/>
            <p>"Output:"</p>
            <pre><code>"Switched to new branch 'feature-test' (88b0ff8)"</code></pre>

            <h3>"Switch to an existing branch"</h3>
            <CodeBlock code="gfs checkout main"/>
            <p>"Switches back to the "<code>"main"</code>" branch."</p>

            <h2>"How It Works"</h2>
            <ol>
                <li>"GFS stops the current database container"</li>
                <li>"The database storage is restored to the target commit"</li>
                <li>"A new container starts with the restored state"</li>
                <li>"You can now work with the database at that point in history"</li>
            </ol>

            <h2>"Time Travel Example"</h2>
            <CodeBlock code="# View your commits
gfs log

# Go back to a previous commit
gfs checkout 88b0ff8

# Verify the database is restored
gfs query \"SELECT 1\"

# Return to the latest state
gfs checkout main"/>

            <h2>"Working with Branches"</h2>
            <CodeBlock code="# Create a branch for experimental changes
gfs checkout -b experiment

# Make changes to your database
# ...

# Commit your changes
gfs commit -m \"Experimental schema changes\"

# Switch back to main
gfs checkout main

# Your database is back to main's state
# The experimental changes are preserved in the experiment branch"/>

            <h2>"Important Notes"</h2>
            <ul>
                <li>"Any uncommitted changes will be lost when checking out"</li>
                <li>"The database container is recreated during checkout"</li>
                <li>"Active connections to the database will be closed"</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/log">"gfs log"</a>" - View commit history to find commit hashes"</li>
                <li><a href="/docs/commands/commit">"gfs commit"</a>" - Save changes before checking out"</li>
                <li><a href="/docs/commands/status">"gfs status"</a>" - Check current branch"</li>
                <li><a href="/docs/commands/query">"gfs query"</a>" - Execute SQL or open interactive terminal"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandQuery() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs query"</h1>
            <p class="lead">"Execute SQL queries or open an interactive database terminal."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="# Execute a SQL query
gfs query \"SELECT * FROM users LIMIT 3\"

# Open interactive terminal (omit the query)
gfs query"/>

            <h2>"Options"</h2>
            <ul>
                <li><code>"--database"</code>" - Override the default database name from container config"</li>
                <li><code>"--path"</code>" - Path to the GFS repository root (default: current directory)"</li>
            </ul>

            <h2>"Description"</h2>
            <p>"The "<code>"query"</code>" command lets you interact with your GFS-managed database directly from the CLI. No separate database client (e.g. psql, mysql) is required."</p>
            <ul>
                <li>"Execute ad-hoc SQL queries and see results in the terminal"</li>
                <li>"Open an interactive terminal session when no query is provided"</li>
                <li>"Works with PostgreSQL and MySQL (uses native client under the hood)"</li>
            </ul>

            <h2>"Examples"</h2>
            <h3>"Run a SELECT query"</h3>
            <CodeBlock code="gfs query \"SELECT * FROM users LIMIT 3\""/>
            <p>"Output:"</p>
            <pre><code>" id |  name   | email\n----+---------+-------------------\n  1 | Alice   | alice@example.com\n  2 | Bob     | bob@example.com\n(2 rows)"</code></pre>

            <h3>"Create a table"</h3>
            <CodeBlock code="gfs query \"CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT);\""/>

            <h3>"Interactive mode"</h3>
            <CodeBlock code="gfs query"/>
            <p>"Opens an interactive database shell. Type SQL and press Enter to execute."</p>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/status">"gfs status"</a>" - Get connection details"</li>
                <li><a href="/docs/commands/commit">"gfs commit"</a>" - Save changes after querying"</li>
                <li><a href="/docs/commands/schema">"gfs schema"</a>" - Extract and inspect schema"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandSchema() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs schema"</h1>
            <p class="lead">"Database schema operations: extract, show, and diff."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="# Extract schema from the running database
gfs schema extract [--output <file>] [--compact]

# Show schema from a specific commit
gfs schema show <commit> [--metadata-only] [--ddl-only]

# Compare schemas between two commits
gfs schema diff <commit1> <commit2> [--pretty] [--json]"/>

            <h2>"Subcommands"</h2>
            <h3>"extract"</h3>
            <p>"Extract schema metadata from the running database. Outputs structured JSON with schemas, tables, and columns."</p>
            <ul>
                <li><code>"--output"</code>" - Write to file instead of stdout"</li>
                <li><code>"--compact"</code>" - Output compact JSON (no pretty-printing)"</li>
            </ul>

            <h3>"show"</h3>
            <p>"Show schema from a historical commit. Supports revision refs like "<code>"HEAD"</code>", "<code>"main"</code>", "<code>"HEAD~1"</code>"."</p>
            <ul>
                <li><code>"--metadata-only"</code>" - Show metadata only"</li>
                <li><code>"--ddl-only"</code>" - Show DDL only"</li>
            </ul>

            <h3>"diff"</h3>
            <p>"Compare schemas between two commits. Default output is agentic (line-oriented). Use "<code>"--pretty"</code>" for human-readable format with colors, or "<code>"--json"</code>" for structured output."</p>
            <ul>
                <li><code>"--pretty"</code>" - Human-readable format with visual tree"</li>
                <li><code>"--json"</code>" - Structured JSON output"</li>
                <li><code>"--no-color"</code>" - Disable color output"</li>
            </ul>

            <h2>"Examples"</h2>
            <h3>"Extract schema from running database"</h3>
            <CodeBlock code="gfs schema"/>
            <p>"Output:"</p>
            <pre><code>"{ \"version\": \"PostgreSQL 17\", \"schemas\": [\"public\"],\n  \"tables\": [{ \"name\": \"users\", \"schema\": \"public\" }], ... }"</code></pre>

            <h3>"Extract and save schema"</h3>
            <CodeBlock code="gfs schema extract --output schema.json"/>

            <h3>"Compare with previous commit"</h3>
            <p>"Choose the output format that fits your use case:"</p>
            <SchemaDiffTabs/>

            <h2>"Revision References"</h2>
            <p>"Commands like "<code>"show"</code>" and "<code>"diff"</code>" accept Git-style revision notation:"</p>
            <ul>
                <li><code>"HEAD"</code>" - Current commit"</li>
                <li><code>"main"</code>" - Branch tip"</li>
                <li><code>"HEAD~1"</code>" - Parent of HEAD"</li>
                <li><code>"88b0ff8"</code>" - Short or full commit hash"</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/query">"gfs query"</a>" - Run SQL against the database"</li>
                <li><a href="/docs/commands/checkout">"gfs checkout"</a>" - Switch commits"</li>
                <li><a href="/docs/commands/export">"gfs export"</a>" - Export data"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandExport() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs export"</h1>
            <p class="lead">"Export data from the running database to a file."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs export --output-dir <dir> --format <fmt>"/>

            <h2>"Options"</h2>
            <ul>
                <li><code>"--output-dir"</code>" (required) - Directory where the export file will be written (created if absent)"</li>
                <li><code>"--format"</code>" (required) - Export format: sql (plain-text) or custom (PostgreSQL binary)"</li>
                <li><code>"--path"</code>" - Path to the GFS repository root"</li>
            </ul>

            <h2>"Supported Formats (PostgreSQL)"</h2>
            <ul>
                <li><strong>"sql"</strong>" - Plain-text SQL dump (pg_dump --format=plain). Output: export.sql"</li>
                <li><strong>"custom"</strong>" - PostgreSQL custom binary format. Output: export.dump"</li>
            </ul>

            <h2>"Examples"</h2>
            <h3>"Export to current directory as SQL"</h3>
            <CodeBlock code="gfs export --output-dir . --format sql"/>

            <h3>"Export to backup directory"</h3>
            <CodeBlock code="gfs export --output-dir /backups/my-repo --format custom"/>

            <h2>"Output"</h2>
            <p>"On success, prints the absolute path to the exported file:"</p>
            <pre><code>"Exported to /Users/project/export.sql"</code></pre>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/import">"gfs import"</a>" - Import data from a file"</li>
                <li><a href="/docs/commands/status">"gfs status"</a>" - Check container is running"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandImport() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs import"</h1>
            <p class="lead">"Import data from a file into the running database."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs import --file <path> [--format <fmt>]"/>

            <h2>"Options"</h2>
            <ul>
                <li><code>"--file"</code>" (required) - Path to the dump file"</li>
                <li><code>"--format"</code>" - Import format (inferred from file extension when omitted)"</li>
                <li><code>"--path"</code>" - Path to the GFS repository root"</li>
            </ul>

            <h2>"Supported Formats (PostgreSQL)"</h2>
            <ul>
                <li><strong>"sql"</strong>" - Plain-text SQL file (.sql extension)"</li>
                <li><strong>"custom"</strong>" - PostgreSQL binary dump (.dump extension)"</li>
                <li><strong>"csv"</strong>" - CSV file (.csv extension)"</li>
            </ul>

            <h2>"Format Inference"</h2>
            <p>"When "<code>"--format"</code>" is omitted, format is inferred from the file extension."</p>

            <h2>"Examples"</h2>
            <h3>"Import a SQL file"</h3>
            <CodeBlock code="gfs import --file ./backup.sql"/>

            <h3>"Import a CSV file"</h3>
            <CodeBlock code="gfs import --file ./data.csv --format csv"/>

            <h2>"Output"</h2>
            <p>"On success, prints the absolute path to the imported file:"</p>
            <pre><code>"Imported from /Users/project/backup.sql"</code></pre>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/export">"gfs export"</a>" - Export data to a file"</li>
                <li><a href="/docs/commands/status">"gfs status"</a>" - Check container is running"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandConfig() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs config"</h1>
            <p class="lead">"Read or write repository config (e.g. user.name, user.email)."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="# Read a config value
gfs config user.name

# Set a config value
gfs config user.name \"John Doe\""/>

            <h2>"Options"</h2>
            <ul>
                <li><code>"--path"</code>" - Path to the GFS repository root"</li>
            </ul>

            <h2>"Description"</h2>
            <p>"The "<code>"config"</code>" command manages repository-level configuration. Common keys:"</p>
            <ul>
                <li><code>"user.name"</code>" - Author name used in commits (fallback for "<code>"gfs commit --author"</code>")"</li>
                <li><code>"user.email"</code>" - Author email used in commits (fallback for "<code>"gfs commit --author-email"</code>")"</li>
            </ul>

            <h2>"Examples"</h2>
            <h3>"Set author for commits"</h3>
            <CodeBlock code="gfs config user.name \"Jane Smith\"
    gfs config user.email \"jane@example.com\""/>

            <h3>"Read current config"</h3>
            <CodeBlock code="gfs config user.name"/>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/commit">"gfs commit"</a>" - Uses user.name and user.email"</li>
            </ul>
        </div>
    }
}

#[component]
fn CommandCompute() -> impl IntoView {
    view! {
        <div>
            <h1>"gfs compute"</h1>
            <p class="lead">"Manage the database container (start, stop, status, logs)."</p>

            <h2>"Usage"</h2>
            <CodeBlock code="gfs compute <ACTION> [--path <dir>]"/>

            <h2>"Subcommands"</h2>
            <ul>
                <li><code>"start"</code>" - Start the database container"</li>
                <li><code>"stop"</code>" - Stop the database container"</li>
                <li><code>"restart"</code>" - Restart the container"</li>
                <li><code>"status"</code>" - Show container status"</li>
                <li><code>"pause"</code>" - Pause the container"</li>
                <li><code>"unpause"</code>" - Unpause the container"</li>
                <li><code>"logs"</code>" - View container logs"</li>
            </ul>

            <h2>"Logs Options"</h2>
            <p>"For "<code>"gfs compute logs"</code>":"</p>
            <ul>
                <li><code>"--tail"</code>" - Number of lines to show from the end"</li>
                <li><code>"--since"</code>" - Show logs since a timestamp"</li>
                <li><code>"--stdout"</code>" / "<code>"--stderr"</code>" - Toggle stdout/stderr"</li>
            </ul>

            <h2>"Examples"</h2>
            <h3>"Start the database"</h3>
            <CodeBlock code="gfs compute start"/>

            <h3>"View recent logs"</h3>
            <CodeBlock code="gfs compute logs --tail 50"/>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/commands/status">"gfs status"</a>" - Repository and compute status"</li>
                <li><a href="/docs/commands/init">"gfs init"</a>" - Initialize and start the database"</li>
            </ul>
        </div>
    }
}

#[component]
fn McpOverview() -> impl IntoView {
    view! {
        <div>
            <h1>"MCP Server Overview"</h1>
            <p class="lead">"The GFS MCP server exposes database version control operations as tools for AI assistants and automation."</p>

            <h2>"What is MCP?"</h2>
            <p>"The Model Context Protocol (MCP) is a standard that lets AI assistants and tools interact with external systems. GFS provides an MCP server so that Claude, Cursor, and other MCP clients can perform repository and compute operations without invoking the CLI directly."</p>

            <h2>"Available Tools"</h2>
            <p>"The GFS MCP server exposes 13 tools that mirror the CLI:"</p>
            <ul>
                <li><code>"list_providers"</code>" - List supported database providers and versions"</li>
                <li><code>"status"</code>" - Repository and compute status (branch, container, connection)"</li>
                <li><code>"commit"</code>" - Create a commit with message"</li>
                <li><code>"log"</code>" - View commit history"</li>
                <li><code>"checkout"</code>" - Switch branch or checkout commit"</li>
                <li><code>"init"</code>" - Initialize a new GFS repository"</li>
                <li><code>"compute"</code>" - Container lifecycle (start, stop, restart, logs)"</li>
                <li><code>"export_database"</code>" - Export data to file"</li>
                <li><code>"import_database"</code>" - Import data from file"</li>
                <li><code>"query"</code>" - Execute SQL against the database"</li>
                <li><code>"extract_schema"</code>" - Extract schema from running database"</li>
                <li><code>"show_schema"</code>" - Show schema from a specific commit"</li>
                <li><code>"diff_schema"</code>" - Compare schemas between two commits"</li>
            </ul>

            <h2>"Transports"</h2>
            <p>"The server supports two modes:"</p>
            <ul>
                <li><strong>"Stdio (default)"</strong>" - For direct client integration (Claude Desktop, Cursor). The client spawns "<code>"gfs mcp"</code>" and communicates over stdin/stdout."</li>
                <li><strong>"HTTP"</strong>" - For daemon mode or remote access. Run "<code>"gfs mcp start"</code>" or "<code>"gfs mcp web"</code>" to listen on port 3000."</li>
            </ul>

            <h2>"Repository Path"</h2>
            <p>"Tools accept an optional "<code>"path"</code>" parameter. When omitted, the server uses "<code>"GFS_REPO_PATH"</code>" or the current working directory at startup."</p>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/mcp/claude-desktop">"Claude Desktop"</a>" - Configure GFS with Claude"</li>
                <li><a href="/docs/mcp/claude-code">"Claude Code"</a>" - IDE extension (VS Code)"</li>
                <li><a href="/docs/mcp/cursor">"Cursor"</a>" - Cursor IDE"</li>
                <li><a href="/docs/mcp/http-mode">"HTTP Mode"</a>" - Daemon and HTTP transport"</li>
                <li><a href="/docs/ai-agents/skills">"Skills"</a>" - use-gfs-mcp skill for agents"</li>
            </ul>
        </div>
    }
}

const CLAUDE_CONFIG_BASIC: &str = r#"{
  "mcpServers": {
    "gfs": {
      "command": "gfs",
      "args": ["mcp"]
    }
  }
}"#;

const CLAUDE_CONFIG_WITH_PATH: &str = r#"{
  "mcpServers": {
    "gfs": {
      "command": "gfs",
      "args": ["mcp", "--path", "/path/to/your/repo"]
    }
  }
}"#;

const CLAUDE_CONFIG_PATH_MACOS: &str =
    "~/Library/Application Support/Claude/claude_desktop_config.json";
const CLAUDE_CONFIG_PATH_WIN: &str = "%APPDATA%/Claude/claude_desktop_config.json";
const CLAUDE_CONFIG_PATH_LINUX: &str = "~/.config/Claude/claude_desktop_config.json";

#[component]
fn McpClaudeDesktop() -> impl IntoView {
    view! {
        <div>
            <h1>"Claude Desktop Integration"</h1>
            <p class="lead">"Configure GFS as an MCP server in Claude Desktop for AI-powered database version control."</p>

            <h2>"Prerequisites"</h2>
            <ul>
                <li>"GFS CLI installed (run "<code>"gfs version"</code>" to verify)"</li>
                <li>"Claude Desktop with MCP support"</li>
            </ul>

            <h2>"Configuration"</h2>
            <p>"Add GFS to your Claude Desktop config. The config file location depends on your OS:"</p>
            <ul>
                <li><strong>"macOS"</strong>" - " {CLAUDE_CONFIG_PATH_MACOS}</li>
                <li><strong>"Windows"</strong>" - " {CLAUDE_CONFIG_PATH_WIN}</li>
                <li><strong>"Linux"</strong>" - " {CLAUDE_CONFIG_PATH_LINUX}</li>
            </ul>

            <h3>"Basic Configuration"</h3>
            <CodeBlock code=CLAUDE_CONFIG_BASIC.to_string()/>

            <h3>"With Repository Path"</h3>
            <p>"To target a specific GFS repository:"</p>
            <CodeBlock code=CLAUDE_CONFIG_WITH_PATH.to_string()/>

            <h2>"Auto-Configuration via Install Script"</h2>
            <p>"When you install GFS with the official script, it can auto-configure Claude Desktop if detected:"</p>
            <CodeBlock code="curl -fsSL https://gfs.guepard.run/install | bash".to_string()/>
            <p>"Select Claude when prompted. The script adds GFS to "<code>"mcpServers"</code>" and copies the use-gfs-cli and use-gfs-mcp skills to "<code>"~/.claude/skills/"</code>"."</p>

            <h2>"Restart Claude Desktop"</h2>
            <p>"After editing the config, restart Claude Desktop. GFS tools will appear and Claude can commit, checkout, query, and manage your database with full version control."</p>

            <h2>"Screenshots"</h2>
            <p>"GFS MCP server in Claude Desktop:"</p>
            <img src="/public/assets/claude-desktop-gfs-mcp.png" alt="GFS MCP in Claude Desktop" class="docs-screenshot"/>
            <p>"GFS tools available to Claude:"</p>
            <img src="/public/assets/claude-desktop-gfs-tools.png" alt="GFS tools in Claude Desktop" class="docs-screenshot"/>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/mcp/overview">"MCP Overview"</a>" - Available tools"</li>
                <li><a href="/docs/mcp/claude-code">"Claude Code"</a>" - IDE extension"</li>
                <li><a href="/docs/ai-agents/skills">"Skills"</a>" - use-gfs-mcp skill"</li>
            </ul>
        </div>
    }
}

const CLAUDE_CODE_CONFIG_PATH_USER: &str = "~/.claude.json";
const CLAUDE_CODE_CONFIG_PATH_PROJECT: &str = ".mcp.json";

#[component]
fn McpClaudeCode() -> impl IntoView {
    view! {
        <div>
            <h1>"Claude Code Integration"</h1>
            <p class="lead">"Configure GFS as an MCP server in Claude Code (VS Code, Cursor) for AI-powered database version control in your IDE."</p>

            <h2>"Prerequisites"</h2>
            <ul>
                <li>"GFS CLI installed (run "<code>"gfs version"</code>" to verify)"</li>
                <li>"Claude Code extension in VS Code or Cursor"</li>
            </ul>

            <h2>"Configuration"</h2>
            <p>"Claude Code supports two MCP config locations:"</p>
            <ul>
                <li><strong>"User scope"</strong>" - " {CLAUDE_CODE_CONFIG_PATH_USER}" - Applies across all projects"</li>
                <li><strong>"Project scope"</strong>" - " {CLAUDE_CODE_CONFIG_PATH_PROJECT}" - In your repo root, shared with collaborators"</li>
            </ul>

            <h3>"Basic Configuration (User)"</h3>
            <p>"Edit "<code>"~/.claude.json"</code>" and add the "<code>"mcpServers"</code>" section (or merge into existing):"</p>
            <CodeBlock code=CLAUDE_CONFIG_BASIC.to_string()/>

            <h3>"With Repository Path"</h3>
            <p>"To target a specific GFS repository:"</p>
            <CodeBlock code=CLAUDE_CONFIG_WITH_PATH.to_string()/>

            <h3>"Project-Level Configuration"</h3>
            <p>"Add "<code>{CLAUDE_CODE_CONFIG_PATH_PROJECT}</code>" in your project root to share GFS with your team. The format is the same as above."</p>

            <h2>"Restart Claude Code"</h2>
            <p>"After editing the config, restart the Claude Code session (or reload the window). GFS tools will appear and Claude can commit, checkout, query, and manage your database with full version control."</p>

            <h2>"Screenshot"</h2>
            <p>"GFS MCP server in Claude Code:"</p>
            <img src="/public/assets/claude-code-mcp.png" alt="GFS MCP in Claude Code" class="docs-screenshot"/>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/mcp/overview">"MCP Overview"</a>" - Available tools"</li>
                <li><a href="/docs/mcp/claude-desktop">"Claude Desktop"</a>" - Standalone app"</li>
                <li><a href="/docs/mcp/cursor">"Cursor"</a>" - Cursor IDE"</li>
                <li><a href="/docs/mcp/http-mode">"HTTP Mode"</a>" - Daemon and HTTP transport"</li>
                <li><a href="/docs/ai-agents/skills">"Skills"</a>" - use-gfs-mcp skill"</li>
            </ul>
        </div>
    }
}

const CURSOR_CONFIG_PATH_GLOBAL: &str = "~/.cursor/mcp.json";
const CURSOR_CONFIG_PATH_PROJECT: &str = ".cursor/mcp.json";

#[component]
fn McpCursor() -> impl IntoView {
    view! {
        <div>
            <h1>"Cursor Integration"</h1>
            <p class="lead">"Configure GFS as an MCP server in Cursor for AI-powered database version control in your IDE."</p>

            <h2>"Prerequisites"</h2>
            <ul>
                <li>"GFS CLI installed (run "<code>"gfs version"</code>" to verify)"</li>
                <li>"Cursor IDE"</li>
            </ul>

            <h2>"Configuration"</h2>
            <p>"Cursor supports two MCP config locations:"</p>
            <ul>
                <li><strong>"Global"</strong>" - " {CURSOR_CONFIG_PATH_GLOBAL}" - Applies across all workspaces"</li>
                <li><strong>"Project"</strong>" - " {CURSOR_CONFIG_PATH_PROJECT}" - In your project root, shared with collaborators"</li>
            </ul>

            <h3>"Basic Configuration (Global)"</h3>
            <p>"Edit "<code>{CURSOR_CONFIG_PATH_GLOBAL}</code>" and add the "<code>"mcpServers"</code>" section (or merge into existing):"</p>
            <CodeBlock code=CLAUDE_CONFIG_BASIC.to_string()/>

            <h3>"With Repository Path"</h3>
            <p>"To target a specific GFS repository:"</p>
            <CodeBlock code=CLAUDE_CONFIG_WITH_PATH.to_string()/>

            <h3>"Project-Level Configuration"</h3>
            <p>"Add "<code>{CURSOR_CONFIG_PATH_PROJECT}</code>" in your project root to share GFS with your team. The format is the same as above."</p>

            <h3>"UI Method"</h3>
            <p>"Alternatively, open Cursor Settings (Cmd+, on macOS, Ctrl+, on Windows) → Tools & MCP → Add new MCP server, then enter the command and args."</p>

            <h2>"Auto-Configuration via Install Script"</h2>
            <p>"When you install GFS with the official script, it can auto-configure Cursor if detected:"</p>
            <CodeBlock code="curl -fsSL https://gfs.guepard.run/install | bash".to_string()/>
            <p>"Select Cursor when prompted. The script adds GFS to "<code>"mcpServers"</code>" and copies the use-gfs-cli and use-gfs-mcp skills to "<code>"~/.cursor/skills/"</code>"."</p>

            <h2>"Restart Cursor"</h2>
            <p>"After editing the config, completely restart Cursor for MCP servers to load. GFS tools will appear and Cursor can commit, checkout, query, and manage your database with full version control."</p>

            <h2>"Screenshot"</h2>
            <p>"GFS MCP server in Cursor:"</p>
            <img src="/public/assets/cursor-gfs-mcp.png" alt="GFS MCP in Cursor" class="docs-screenshot"/>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/mcp/overview">"MCP Overview"</a>" - Available tools"</li>
                <li><a href="/docs/mcp/claude-desktop">"Claude Desktop"</a>" - Standalone app"</li>
                <li><a href="/docs/mcp/claude-code">"Claude Code"</a>" - VS Code extension"</li>
                <li><a href="/docs/mcp/http-mode">"HTTP Mode"</a>" - Daemon and HTTP transport"</li>
                <li><a href="/docs/ai-agents/skills">"Skills"</a>" - use-gfs-mcp skill"</li>
            </ul>
        </div>
    }
}

#[component]
fn McpHttpMode() -> impl IntoView {
    view! {
        <div>
            <h1>"HTTP Mode"</h1>
            <p class="lead">"Run the GFS MCP server over HTTP for daemon mode or remote access."</p>

            <h2>"Daemon Mode"</h2>
            <p>"Start the MCP server as a background daemon:"</p>
            <CodeBlock code="gfs mcp start"/>
            <p>"The server listens on "<code>"http://127.0.0.1:3000/mcp"</code>" by default. Manage the daemon with:"</p>
            <ul>
                <li><code>"gfs mcp status"</code>" - Check if the daemon is running"</li>
                <li><code>"gfs mcp stop"</code>" - Stop the daemon"</li>
                <li><code>"gfs mcp restart"</code>" - Restart the daemon"</li>
            </ul>

            <h2>"Foreground HTTP"</h2>
            <p>"Run the HTTP server in the foreground (useful for debugging):"</p>
            <CodeBlock code="gfs mcp web
# Or with custom port:
gfs mcp web --port 8080"/>

            <h2>"With Repository Path"</h2>
            <CodeBlock code="gfs mcp --path /path/to/repo start
    gfs mcp --path /path/to/repo web --port 8080"/>

            <h2>"Endpoint"</h2>
            <p>"Clients send JSON-RPC requests to "<code>"POST http://127.0.0.1:PORT/mcp"</code>". The server uses the streamable HTTP transport. No authentication is required by default."</p>

            <h2>"Use Cases"</h2>
            <ul>
                <li>"CI/CD pipelines - Call MCP tools from scripts"</li>
                <li>"Remote management - Access from another machine on the network"</li>
                <li>"Custom tooling - Build UIs or integrations on top of the HTTP API"</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/mcp/overview">"MCP Overview"</a>" - Tools and architecture"</li>
                <li><a href="/docs/mcp/claude-desktop">"Claude Desktop"</a>" - Stdio integration"</li>
                <li><a href="/docs/mcp/claude-code">"Claude Code"</a>" - IDE extension"</li>
                <li><a href="/docs/mcp/cursor">"Cursor"</a>" - Cursor IDE"</li>
            </ul>
        </div>
    }
}

#[component]
fn AiAgentsSkills() -> impl IntoView {
    view! {
        <div>
            <h1>"Skills"</h1>
            <p class="lead">"Prebuilt skills that teach AI agents how to use GFS effectively."</p>

            <h2>"What are Skills?"</h2>
            <p>"Skills are reusable knowledge packages that AI agents can load to understand and operate GFS. They provide structured instructions, examples, and best practices for database version control."</p>

            <h2>"Available Skills"</h2>
            <h3>"use-gfs-cli"</h3>
            <p>"Git-like version control for databases using the GFS CLI. Covers commits, branches, time travel, and schema versioning."</p>
            <ul>
                <li>"Installation and quick start"</li>
                <li>"Core commands: init, commit, checkout, log, status"</li>
                <li>"Schema operations: extract, show, diff"</li>
                <li>"Query, export, and import"</li>
            </ul>
            <p><a href="https://github.com/Guepard-Corp/gfs/blob/main/skills/use-gfs-cli/SKILL.md" target="_blank">"View use-gfs-cli skill"</a></p>

            <h3>"use-gfs-mcp"</h3>
            <p>"GFS MCP Server for AI agent integration. Provides Model Context Protocol tools for database version control with automatic schema versioning."</p>
            <ul>
                <li>"MCP configuration and setup"</li>
                <li>"All GFS operations exposed as tools"</li>
                <li>"Revision references for schema and diff"</li>
                <li>"Integration with Claude Desktop and other MCP clients"</li>
            </ul>
            <p><a href="https://github.com/Guepard-Corp/gfs/blob/main/skills/use-gfs-mcp/SKILL.md" target="_blank">"View use-gfs-mcp skill"</a></p>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/ai-agents/subagents">"Subagents"</a>" - Specialized agents for database tasks"</li>
                <li><a href="/docs/mcp/overview">"MCP Server"</a>" - Programmatic access to GFS"</li>
            </ul>
        </div>
    }
}

#[component]
fn AiAgentsSubagents() -> impl IntoView {
    view! {
        <div>
            <h1>"Subagents"</h1>
            <p class="lead">"Specialized AI agents for database querying and schema management."</p>

            <h2>"What are Subagents?"</h2>
            <p>"Subagents are expert AI agents configured with GFS tools and skills. They handle specific database tasks like natural language to SQL conversion, schema-aware querying, and safe schema evolution."</p>

            <h2>"Available Subagents"</h2>
            <h3>"Qwery Agent"</h3>
            <p>"Expert database query agent with schema awareness. Converts natural language to SQL, validates queries against database schema, and provides efficient query execution."</p>
            <ul>
                <li>"Natural language to SQL conversion"</li>
                <li>"Schema-aware query generation and validation"</li>
                <li>"Query optimization and syntax validation"</li>
                <li>"Schema evolution tracking and time-travel queries"</li>
                <li>"Safe destructive operations via GFS branching"</li>
            </ul>
            <p>"Uses the "<code>"use-gfs-mcp"</code>" skill and GFS MCP tools."</p>
            <p><a href="https://github.com/Guepard-Corp/gfs/blob/main/agents/qwery-agent.md" target="_blank">"View Qwery Agent"</a></p>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/ai-agents/skills">"Skills"</a>" - Prebuilt knowledge for agents"</li>
                <li><a href="/docs/mcp/overview">"MCP Server"</a>" - Tool integration"</li>
            </ul>
        </div>
    }
}

#[component]
fn ComingSoon(#[prop(into)] page: String) -> impl IntoView {
    view! {
        <div>
            <h1>"Page Not Found"</h1>
            <p>"The page "<code>{page}</code>" was not found."</p>
            <p><a href="/docs">"Return to documentation"</a></p>
        </div>
    }
}
